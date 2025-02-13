/// An async function that can recursively call itself.
type AsyncRecursiveType<'a, T> = Pin<Box<dyn Future<Output = T> + 'a + Send>>;

use std::{
	collections::{hash_map, HashSet},
	pin::Pin,
	time::{Duration, Instant, SystemTime},
};

use futures_util::{stream::FuturesUnordered, Future, StreamExt};
use ruma::{
	api::{
		client::error::ErrorKind,
		federation::{
			discovery::{
				get_remote_server_keys,
				get_remote_server_keys_batch::{self, v2::QueryCriteria},
				get_server_keys,
			},
			event::{get_event, get_room_state_ids},
			membership::create_join_event,
		},
	},
	events::{
		room::{create::RoomCreateEventContent, server_acl::RoomServerAclEventContent},
		StateEventType,
	},
	int,
	serde::Base64,
	state_res::{self, RoomVersion, StateMap},
	uint, CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedServerName,
	OwnedServerSigningKeyId, RoomId, RoomVersionId, ServerName,
};
use serde_json::value::RawValue as RawJsonValue;
use tokio::sync::{RwLock, RwLockWriteGuard, Semaphore};
use tracing::{debug, error, info, trace, warn};

use super::state_compressor::CompressedStateEvent;
use crate::{
	service::{pdu, Arc, BTreeMap, HashMap, Result},
	services, Error, PduEvent,
};

type AsyncRecursiveCanonicalJsonVec<'a> =
	AsyncRecursiveType<'a, Vec<(Arc<PduEvent>, Option<BTreeMap<String, CanonicalJsonValue>>)>>;
type AsyncRecursiveCanonicalJsonResult<'a> =
	AsyncRecursiveType<'a, Result<(Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)>>;

pub struct Service;

impl Service {
	/// When receiving an event one needs to:
	/// 0. Check the server is in the room
	/// 1. Skip the PDU if we already know about it
	/// 1.1. Remove unsigned field
	/// 2. Check signatures, otherwise drop
	/// 3. Check content hash, redact if doesn't match
	/// 4. Fetch any missing auth events doing all checks listed here starting
	///    at 1. These are not timeline events
	/// 5. Reject "due to auth events" if can't get all the auth events or some
	///    of the auth events are also rejected "due to auth events"
	/// 6. Reject "due to auth events" if the event doesn't pass auth based on
	///    the auth events
	/// 7. Persist this event as an outlier
	/// 8. If not timeline event: stop
	/// 9. Fetch any missing prev events doing all checks listed here starting
	///    at 1. These are timeline events
	/// 10. Fetch missing state and auth chain events by calling `/state_ids` at
	///     backwards extremities doing all the checks in this list starting at
	///     1. These are not timeline events
	/// 11. Check the auth of the event passes based on the state of the event
	/// 12. Ensure that the state is derived from the previous current state
	///     (i.e. we calculated by doing state res where one of the inputs was a
	///     previously trusted set of state, don't just trust a set of state we
	///     got from a remote)
	/// 13. Use state resolution to find new room state
	/// 14. Check if the event passes auth based on the "current state" of the
	///     room, if not soft fail it
	// We use some AsyncRecursiveType hacks here so we can call this async funtion
	// recursively
	pub(crate) async fn handle_incoming_pdu<'a>(
		&self, origin: &'a ServerName, event_id: &'a EventId, room_id: &'a RoomId,
		value: BTreeMap<String, CanonicalJsonValue>, is_timeline_event: bool,
		pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> Result<Option<Vec<u8>>> {
		// 0. Check the server is in the room
		if !services().rooms.metadata.exists(room_id)? {
			return Err(Error::BadRequest(ErrorKind::NotFound, "Room is unknown to this server"));
		}

		if services().rooms.metadata.is_disabled(room_id)? {
			info!(
				"Federaton of room {room_id} is currently disabled on this server. Request by origin {origin} and \
				 event ID {event_id}"
			);
			return Err(Error::BadRequest(
				ErrorKind::forbidden(),
				"Federation of this room is currently disabled on this server.",
			));
		}

		services().rooms.event_handler.acl_check(origin, room_id)?;

		// 1. Skip the PDU if we already have it as a timeline event
		if let Some(pdu_id) = services().rooms.timeline.get_pdu_id(event_id)? {
			return Ok(Some(pdu_id));
		}

		let create_event = services()
			.rooms
			.state_accessor
			.room_state_get(room_id, &StateEventType::RoomCreate, "")?
			.ok_or_else(|| Error::bad_database("Failed to find create event in db."))?;

		let create_event_content: RoomCreateEventContent =
			serde_json::from_str(create_event.content.get()).map_err(|e| {
				error!("Invalid create event: {}", e);
				Error::BadDatabase("Invalid create event in db")
			})?;
		let room_version_id = &create_event_content.room_version;

		let first_pdu_in_room = services()
			.rooms
			.timeline
			.first_pdu_in_room(room_id)?
			.ok_or_else(|| Error::bad_database("Failed to find first pdu in db."))?;

		let (incoming_pdu, val) = self
			.handle_outlier_pdu(origin, &create_event, event_id, room_id, value, false, pub_key_map)
			.await?;
		self.check_room_id(room_id, &incoming_pdu)?;

		// 8. if not timeline event: stop
		if !is_timeline_event {
			return Ok(None);
		}

		// Skip old events
		if incoming_pdu.origin_server_ts < first_pdu_in_room.origin_server_ts {
			return Ok(None);
		}

		// 9. Fetch any missing prev events doing all checks listed here starting at 1.
		//    These are timeline events
		let (sorted_prev_events, mut eventid_info) = self
			.fetch_unknown_prev_events(
				origin,
				&create_event,
				room_id,
				room_version_id,
				pub_key_map,
				incoming_pdu.prev_events.clone(),
			)
			.await?;

		let mut errors = 0;
		debug!(events = ?sorted_prev_events, "Got previous events");
		for prev_id in sorted_prev_events {
			// Check for disabled again because it might have changed
			if services().rooms.metadata.is_disabled(room_id)? {
				info!(
					"Federaton of room {room_id} is currently disabled on this server. Request by origin {origin} and \
					 event ID {event_id}"
				);
				return Err(Error::BadRequest(
					ErrorKind::forbidden(),
					"Federation of this room is currently disabled on this server.",
				));
			}

			if let Some((time, tries)) = services()
				.globals
				.bad_event_ratelimiter
				.read()
				.await
				.get(&*prev_id)
			{
				// Exponential backoff
				let mut min_elapsed_duration = Duration::from_secs(5 * 60) * (*tries) * (*tries);
				if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
					min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
				}

				if time.elapsed() < min_elapsed_duration {
					info!("Backing off from {}", prev_id);
					continue;
				}
			}

			if errors >= 5 {
				// Timeout other events
				match services()
					.globals
					.bad_event_ratelimiter
					.write()
					.await
					.entry((*prev_id).to_owned())
				{
					hash_map::Entry::Vacant(e) => {
						e.insert((Instant::now(), 1));
					},
					hash_map::Entry::Occupied(mut e) => {
						*e.get_mut() = (Instant::now(), e.get().1 + 1);
					},
				}
				continue;
			}

			if let Some((pdu, json)) = eventid_info.remove(&*prev_id) {
				// Skip old events
				if pdu.origin_server_ts < first_pdu_in_room.origin_server_ts {
					continue;
				}

				let start_time = Instant::now();
				services()
					.globals
					.roomid_federationhandletime
					.write()
					.await
					.insert(room_id.to_owned(), ((*prev_id).to_owned(), start_time));

				if let Err(e) = self
					.upgrade_outlier_to_timeline_pdu(pdu, json, &create_event, origin, room_id, pub_key_map)
					.await
				{
					errors += 1;
					warn!("Prev event {} failed: {}", prev_id, e);
					match services()
						.globals
						.bad_event_ratelimiter
						.write()
						.await
						.entry((*prev_id).to_owned())
					{
						hash_map::Entry::Vacant(e) => {
							e.insert((Instant::now(), 1));
						},
						hash_map::Entry::Occupied(mut e) => {
							*e.get_mut() = (Instant::now(), e.get().1 + 1);
						},
					}
				}
				let elapsed = start_time.elapsed();
				services()
					.globals
					.roomid_federationhandletime
					.write()
					.await
					.remove(&room_id.to_owned());
				debug!(
					"Handling prev event {} took {}m{}s",
					prev_id,
					elapsed.as_secs() / 60,
					elapsed.as_secs() % 60
				);
			}
		}

		// Done with prev events, now handling the incoming event

		let start_time = Instant::now();
		services()
			.globals
			.roomid_federationhandletime
			.write()
			.await
			.insert(room_id.to_owned(), (event_id.to_owned(), start_time));
		let r = services()
			.rooms
			.event_handler
			.upgrade_outlier_to_timeline_pdu(incoming_pdu, val, &create_event, origin, room_id, pub_key_map)
			.await;
		services()
			.globals
			.roomid_federationhandletime
			.write()
			.await
			.remove(&room_id.to_owned());

		r
	}

	#[allow(clippy::too_many_arguments)]
	fn handle_outlier_pdu<'a>(
		&'a self, origin: &'a ServerName, create_event: &'a PduEvent, event_id: &'a EventId, room_id: &'a RoomId,
		mut value: BTreeMap<String, CanonicalJsonValue>, auth_events_known: bool,
		pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> AsyncRecursiveCanonicalJsonResult<'a> {
		Box::pin(async move {
			// 1. Remove unsigned field
			value.remove("unsigned");

			// TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

			// 2. Check signatures, otherwise drop
			// 3. check content hash, redact if doesn't match
			let create_event_content: RoomCreateEventContent = serde_json::from_str(create_event.content.get())
				.map_err(|e| {
					error!("Invalid create event: {}", e);
					Error::BadDatabase("Invalid create event in db")
				})?;

			let room_version_id = &create_event_content.room_version;
			let room_version = RoomVersion::new(room_version_id).expect("room version is supported");

			let guard = pub_key_map.read().await;
			let mut val = match ruma::signatures::verify_event(&guard, &value, room_version_id) {
				Err(e) => {
					// Drop
					warn!("Dropping bad event {}: {}", event_id, e,);
					return Err(Error::BadRequest(ErrorKind::InvalidParam, "Signature verification failed"));
				},
				Ok(ruma::signatures::Verified::Signatures) => {
					// Redact
					warn!("Calculated hash does not match: {}", event_id);
					let Ok(obj) = ruma::canonical_json::redact(value, room_version_id, None) else {
						return Err(Error::BadRequest(ErrorKind::InvalidParam, "Redaction failed"));
					};

					// Skip the PDU if it is redacted and we already have it as an outlier event
					if services().rooms.timeline.get_pdu_json(event_id)?.is_some() {
						return Err(Error::BadRequest(
							ErrorKind::InvalidParam,
							"Event was redacted and we already knew about it",
						));
					}

					obj
				},
				Ok(ruma::signatures::Verified::All) => value,
			};

			drop(guard);

			// Now that we have checked the signature and hashes we can add the eventID and
			// convert to our PduEvent type
			val.insert("event_id".to_owned(), CanonicalJsonValue::String(event_id.as_str().to_owned()));
			let incoming_pdu = serde_json::from_value::<PduEvent>(
				serde_json::to_value(&val).expect("CanonicalJsonObj is a valid JsonValue"),
			)
			.map_err(|_| Error::bad_database("Event is not a valid PDU."))?;

			self.check_room_id(room_id, &incoming_pdu)?;

			if !auth_events_known {
				// 4. fetch any missing auth events doing all checks listed here starting at 1.
				//    These are not timeline events
				// 5. Reject "due to auth events" if can't get all the auth events or some of
				//    the auth events are also rejected "due to auth events"
				// NOTE: Step 5 is not applied anymore because it failed too often
				debug!(event_id = ?incoming_pdu.event_id, "Fetching auth events");
				self.fetch_and_handle_outliers(
					origin,
					&incoming_pdu
						.auth_events
						.iter()
						.map(|x| Arc::from(&**x))
						.collect::<Vec<_>>(),
					create_event,
					room_id,
					room_version_id,
					pub_key_map,
				)
				.await;
			}

			// 6. Reject "due to auth events" if the event doesn't pass auth based on the
			//    auth events
			debug!("Auth check for {} based on auth events", incoming_pdu.event_id);

			// Build map of auth events
			let mut auth_events = HashMap::new();
			for id in &incoming_pdu.auth_events {
				let Some(auth_event) = services().rooms.timeline.get_pdu(id)? else {
					warn!("Could not find auth event {}", id);
					continue;
				};

				self.check_room_id(room_id, &auth_event)?;

				match auth_events.entry((
					auth_event.kind.to_string().into(),
					auth_event
						.state_key
						.clone()
						.expect("all auth events have state keys"),
				)) {
					hash_map::Entry::Vacant(v) => {
						v.insert(auth_event);
					},
					hash_map::Entry::Occupied(_) => {
						return Err(Error::BadRequest(
							ErrorKind::InvalidParam,
							"Auth event's type and state_key combination exists multiple times.",
						));
					},
				}
			}

			// The original create event must be in the auth events
			if !matches!(
				auth_events
					.get(&(StateEventType::RoomCreate, String::new()))
					.map(AsRef::as_ref),
				Some(_) | None
			) {
				return Err(Error::BadRequest(
					ErrorKind::InvalidParam,
					"Incoming event refers to wrong create event.",
				));
			}

			if !state_res::event_auth::auth_check(
				&room_version,
				&incoming_pdu,
				None::<PduEvent>, // TODO: third party invite
				|k, s| auth_events.get(&(k.to_string().into(), s.to_owned())),
			)
			.map_err(|_e| Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed"))?
			{
				return Err(Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed"));
			}

			debug!("Validation successful.");

			// 7. Persist the event as an outlier.
			services()
				.rooms
				.outlier
				.add_pdu_outlier(&incoming_pdu.event_id, &val)?;

			debug!("Added pdu as outlier.");

			Ok((Arc::new(incoming_pdu), val))
		})
	}

	pub async fn upgrade_outlier_to_timeline_pdu(
		&self, incoming_pdu: Arc<PduEvent>, val: BTreeMap<String, CanonicalJsonValue>, create_event: &PduEvent,
		origin: &ServerName, room_id: &RoomId, pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> Result<Option<Vec<u8>>> {
		// Skip the PDU if we already have it as a timeline event
		if let Ok(Some(pduid)) = services().rooms.timeline.get_pdu_id(&incoming_pdu.event_id) {
			return Ok(Some(pduid));
		}

		if services()
			.rooms
			.pdu_metadata
			.is_event_soft_failed(&incoming_pdu.event_id)?
		{
			return Err(Error::BadRequest(ErrorKind::InvalidParam, "Event has been soft failed"));
		}

		info!("Upgrading {} to timeline pdu", incoming_pdu.event_id);

		let create_event_content: RoomCreateEventContent =
			serde_json::from_str(create_event.content.get()).map_err(|e| {
				warn!("Invalid create event: {}", e);
				Error::BadDatabase("Invalid create event in db")
			})?;

		let room_version_id = &create_event_content.room_version;
		let room_version = RoomVersion::new(room_version_id).expect("room version is supported");

		// 10. Fetch missing state and auth chain events by calling /state_ids at
		//     backwards extremities doing all the checks in this list starting at 1.
		//     These are not timeline events.

		// TODO: if we know the prev_events of the incoming event we can avoid the
		// request and build the state from a known point and resolve if > 1 prev_event

		debug!("Requesting state at event");
		let mut state_at_incoming_event = None;

		if incoming_pdu.prev_events.len() == 1 {
			let prev_event = &*incoming_pdu.prev_events[0];
			let prev_event_sstatehash = services()
				.rooms
				.state_accessor
				.pdu_shortstatehash(prev_event)?;

			let state = if let Some(shortstatehash) = prev_event_sstatehash {
				Some(
					services()
						.rooms
						.state_accessor
						.state_full_ids(shortstatehash)
						.await,
				)
			} else {
				None
			};

			if let Some(Ok(mut state)) = state {
				debug!("Using cached state");
				let prev_pdu = services()
					.rooms
					.timeline
					.get_pdu(prev_event)
					.ok()
					.flatten()
					.ok_or_else(|| Error::bad_database("Could not find prev event, but we know the state."))?;

				if let Some(state_key) = &prev_pdu.state_key {
					let shortstatekey = services()
						.rooms
						.short
						.get_or_create_shortstatekey(&prev_pdu.kind.to_string().into(), state_key)?;

					state.insert(shortstatekey, Arc::from(prev_event));
					// Now it's the state after the pdu
				}

				state_at_incoming_event = Some(state);
			}
		} else {
			debug!("Calculating state at event using state res");
			let mut extremity_sstatehashes = HashMap::new();

			let mut okay = true;
			for prev_eventid in &incoming_pdu.prev_events {
				let Ok(Some(prev_event)) = services().rooms.timeline.get_pdu(prev_eventid) else {
					okay = false;
					break;
				};

				let Ok(Some(sstatehash)) = services()
					.rooms
					.state_accessor
					.pdu_shortstatehash(prev_eventid)
				else {
					okay = false;
					break;
				};

				extremity_sstatehashes.insert(sstatehash, prev_event);
			}

			if okay {
				let mut fork_states = Vec::with_capacity(extremity_sstatehashes.len());
				let mut auth_chain_sets = Vec::with_capacity(extremity_sstatehashes.len());

				for (sstatehash, prev_event) in extremity_sstatehashes {
					let mut leaf_state: HashMap<_, _> = services()
						.rooms
						.state_accessor
						.state_full_ids(sstatehash)
						.await?;

					if let Some(state_key) = &prev_event.state_key {
						let shortstatekey = services()
							.rooms
							.short
							.get_or_create_shortstatekey(&prev_event.kind.to_string().into(), state_key)?;
						leaf_state.insert(shortstatekey, Arc::from(&*prev_event.event_id));
						// Now it's the state after the pdu
					}

					let mut state = StateMap::with_capacity(leaf_state.len());
					let mut starting_events = Vec::with_capacity(leaf_state.len());

					for (k, id) in leaf_state {
						if let Ok((ty, st_key)) = services().rooms.short.get_statekey_from_short(k) {
							// FIXME: Undo .to_string().into() when StateMap
							//        is updated to use StateEventType
							state.insert((ty.to_string().into(), st_key), id.clone());
						} else {
							warn!("Failed to get_statekey_from_short.");
						}
						starting_events.push(id);
					}

					auth_chain_sets.push(
						services()
							.rooms
							.auth_chain
							.get_auth_chain(room_id, starting_events)
							.await?
							.collect(),
					);

					fork_states.push(state);
				}

				let lock = services().globals.stateres_mutex.lock();

				let result = state_res::resolve(room_version_id, &fork_states, auth_chain_sets, |id| {
					let res = services().rooms.timeline.get_pdu(id);
					if let Err(e) = &res {
						error!("Failed to fetch event: {}", e);
					}
					res.ok().flatten()
				});
				drop(lock);

				state_at_incoming_event = match result {
					Ok(new_state) => Some(
						new_state
							.into_iter()
							.map(|((event_type, state_key), event_id)| {
								let shortstatekey = services()
									.rooms
									.short
									.get_or_create_shortstatekey(&event_type.to_string().into(), &state_key)?;
								Ok((shortstatekey, event_id))
							})
							.collect::<Result<_>>()?,
					),
					Err(e) => {
						warn!(
							"State resolution on prev events failed, either an event could not be found or \
							 deserialization: {}",
							e
						);
						None
					},
				}
			}
		}

		if state_at_incoming_event.is_none() {
			debug!("Calling /state_ids");
			// Call /state_ids to find out what the state at this pdu is. We trust the
			// server's response to some extend, but we still do a lot of checks on the
			// events
			match services()
				.sending
				.send_federation_request(
					origin,
					get_room_state_ids::v1::Request {
						room_id: room_id.to_owned(),
						event_id: (*incoming_pdu.event_id).to_owned(),
					},
				)
				.await
			{
				Ok(res) => {
					debug!("Fetching state events at event.");

					let collect = res
						.pdu_ids
						.iter()
						.map(|x| Arc::from(&**x))
						.collect::<Vec<_>>();

					let state_vec = self
						.fetch_and_handle_outliers(
							origin,
							&collect,
							create_event,
							room_id,
							room_version_id,
							pub_key_map,
						)
						.await;

					let mut state: HashMap<_, Arc<EventId>> = HashMap::new();
					for (pdu, _) in state_vec {
						let state_key = pdu
							.state_key
							.clone()
							.ok_or_else(|| Error::bad_database("Found non-state pdu in state events."))?;

						let shortstatekey = services()
							.rooms
							.short
							.get_or_create_shortstatekey(&pdu.kind.to_string().into(), &state_key)?;

						match state.entry(shortstatekey) {
							hash_map::Entry::Vacant(v) => {
								v.insert(Arc::from(&*pdu.event_id));
							},
							hash_map::Entry::Occupied(_) => {
								return Err(Error::bad_database(
									"State event's type and state_key combination exists multiple times.",
								))
							},
						}
					}

					// The original create event must still be in the state
					let create_shortstatekey = services()
						.rooms
						.short
						.get_shortstatekey(&StateEventType::RoomCreate, "")?
						.expect("Room exists");

					if state.get(&create_shortstatekey).map(AsRef::as_ref) != Some(&create_event.event_id) {
						return Err(Error::bad_database("Incoming event refers to wrong create event."));
					}

					state_at_incoming_event = Some(state);
				},
				Err(e) => {
					warn!("Fetching state for event failed: {}", e);
					return Err(e);
				},
			};
		}

		let state_at_incoming_event = state_at_incoming_event.expect("we always set this to some above");

		debug!("Starting auth check");
		// 11. Check the auth of the event passes based on the state of the event
		let check_result = state_res::event_auth::auth_check(
			&room_version,
			&incoming_pdu,
			None::<PduEvent>, // TODO: third party invite
			|k, s| {
				services()
					.rooms
					.short
					.get_shortstatekey(&k.to_string().into(), s)
					.ok()
					.flatten()
					.and_then(|shortstatekey| state_at_incoming_event.get(&shortstatekey))
					.and_then(|event_id| services().rooms.timeline.get_pdu(event_id).ok().flatten())
			},
		)
		.map_err(|_e| Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed."))?;

		if !check_result {
			return Err(Error::bad_database("Event has failed auth check with state at the event."));
		}
		debug!("Auth check succeeded");

		// Soft fail check before doing state res
		let auth_events = services().rooms.state.get_auth_events(
			room_id,
			&incoming_pdu.kind,
			&incoming_pdu.sender,
			incoming_pdu.state_key.as_deref(),
			&incoming_pdu.content,
		)?;

		let soft_fail = !state_res::event_auth::auth_check(&room_version, &incoming_pdu, None::<PduEvent>, |k, s| {
			auth_events.get(&(k.clone(), s.to_owned()))
		})
		.map_err(|_e| Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed."))?;

		// 13. Use state resolution to find new room state

		// We start looking at current room state now, so lets lock the room
		let mutex_state = Arc::clone(
			services()
				.globals
				.roomid_mutex_state
				.write()
				.await
				.entry(room_id.to_owned())
				.or_default(),
		);
		let state_lock = mutex_state.lock().await;

		// Now we calculate the set of extremities this room has after the incoming
		// event has been applied. We start with the previous extremities (aka leaves)
		debug!("Calculating extremities");
		let mut extremities = services().rooms.state.get_forward_extremities(room_id)?;
		debug!("Amount of forward extremities in room {room_id}: {extremities:?}");

		// Remove any forward extremities that are referenced by this incoming event's
		// prev_events
		for prev_event in &incoming_pdu.prev_events {
			if extremities.contains(prev_event) {
				extremities.remove(prev_event);
			}
		}

		// Only keep those extremities were not referenced yet
		extremities.retain(|id| {
			!matches!(
				services()
					.rooms
					.pdu_metadata
					.is_event_referenced(room_id, id),
				Ok(true)
			)
		});

		debug!("Compressing state at event");
		let state_ids_compressed = Arc::new(
			state_at_incoming_event
				.iter()
				.map(|(shortstatekey, id)| {
					services()
						.rooms
						.state_compressor
						.compress_state_event(*shortstatekey, id)
				})
				.collect::<Result<_>>()?,
		);

		if incoming_pdu.state_key.is_some() {
			debug!("Preparing for stateres to derive new room state");

			// We also add state after incoming event to the fork states
			let mut state_after = state_at_incoming_event.clone();
			if let Some(state_key) = &incoming_pdu.state_key {
				let shortstatekey = services()
					.rooms
					.short
					.get_or_create_shortstatekey(&incoming_pdu.kind.to_string().into(), state_key)?;

				state_after.insert(shortstatekey, Arc::from(&*incoming_pdu.event_id));
			}

			let new_room_state = self
				.resolve_state(room_id, room_version_id, state_after)
				.await?;

			// Set the new room state to the resolved state
			debug!("Forcing new room state");

			let (sstatehash, new, removed) = services()
				.rooms
				.state_compressor
				.save_state(room_id, new_room_state)?;

			services()
				.rooms
				.state
				.force_state(room_id, sstatehash, new, removed, &state_lock)
				.await?;
		}

		// 14. Check if the event passes auth based on the "current state" of the room,
		//     if not soft fail it
		debug!("Starting soft fail auth check");

		if soft_fail {
			services()
				.rooms
				.timeline
				.append_incoming_pdu(
					&incoming_pdu,
					val,
					extremities.iter().map(|e| (**e).to_owned()).collect(),
					state_ids_compressed,
					soft_fail,
					&state_lock,
				)
				.await?;

			// Soft fail, we keep the event as an outlier but don't add it to the timeline
			warn!("Event was soft failed: {:?}", incoming_pdu);
			services()
				.rooms
				.pdu_metadata
				.mark_event_soft_failed(&incoming_pdu.event_id)?;
			return Err(Error::BadRequest(ErrorKind::InvalidParam, "Event has been soft failed"));
		}

		debug!("Appending pdu to timeline");
		extremities.insert(incoming_pdu.event_id.clone());

		// Now that the event has passed all auth it is added into the timeline.
		// We use the `state_at_event` instead of `state_after` so we accurately
		// represent the state for this event.

		let pdu_id = services()
			.rooms
			.timeline
			.append_incoming_pdu(
				&incoming_pdu,
				val,
				extremities.iter().map(|e| (**e).to_owned()).collect(),
				state_ids_compressed,
				soft_fail,
				&state_lock,
			)
			.await?;

		debug!("Appended incoming pdu");

		// Event has passed all auth/stateres checks
		drop(state_lock);
		Ok(pdu_id)
	}

	async fn resolve_state(
		&self, room_id: &RoomId, room_version_id: &RoomVersionId, incoming_state: HashMap<u64, Arc<EventId>>,
	) -> Result<Arc<HashSet<CompressedStateEvent>>> {
		debug!("Loading current room state ids");
		let current_sstatehash = services()
			.rooms
			.state
			.get_room_shortstatehash(room_id)?
			.expect("every room has state");

		let current_state_ids = services()
			.rooms
			.state_accessor
			.state_full_ids(current_sstatehash)
			.await?;

		let fork_states = [current_state_ids, incoming_state];

		let mut auth_chain_sets = Vec::new();
		for state in &fork_states {
			auth_chain_sets.push(
				services()
					.rooms
					.auth_chain
					.get_auth_chain(room_id, state.iter().map(|(_, id)| id.clone()).collect())
					.await?
					.collect(),
			);
		}

		debug!("Loading fork states");

		let fork_states: Vec<_> = fork_states
			.into_iter()
			.map(|map| {
				map.into_iter()
					.filter_map(|(k, id)| {
						services()
							.rooms
							.short
							.get_statekey_from_short(k)
							.map(|(ty, st_key)| ((ty.to_string().into(), st_key), id))
							.ok()
					})
					.collect::<StateMap<_>>()
			})
			.collect();

		debug!("Resolving state");

		let lock = services().globals.stateres_mutex.lock();
		let state_resolve = state_res::resolve(room_version_id, &fork_states, auth_chain_sets, |id| {
			let res = services().rooms.timeline.get_pdu(id);
			if let Err(e) = &res {
				error!("Failed to fetch event: {}", e);
			}
			res.ok().flatten()
		});

		let state = match state_resolve {
			Ok(new_state) => new_state,
			Err(e) => {
				error!("State resolution failed: {}", e);
				return Err(Error::bad_database(
					"State resolution failed, either an event could not be found or deserialization",
				));
			},
		};

		drop(lock);

		debug!("State resolution done. Compressing state");

		let new_room_state = state
			.into_iter()
			.map(|((event_type, state_key), event_id)| {
				let shortstatekey = services()
					.rooms
					.short
					.get_or_create_shortstatekey(&event_type.to_string().into(), &state_key)?;
				services()
					.rooms
					.state_compressor
					.compress_state_event(shortstatekey, &event_id)
			})
			.collect::<Result<_>>()?;

		Ok(Arc::new(new_room_state))
	}

	/// Find the event and auth it. Once the event is validated (steps 1 - 8)
	/// it is appended to the outliers Tree.
	///
	/// Returns pdu and if we fetched it over federation the raw json.
	///
	/// a. Look in the main timeline (pduid_pdu tree)
	/// b. Look at outlier pdu tree
	/// c. Ask origin server over federation
	/// d. TODO: Ask other servers over federation?
	#[tracing::instrument(skip_all)]
	pub(crate) fn fetch_and_handle_outliers<'a>(
		&'a self, origin: &'a ServerName, events: &'a [Arc<EventId>], create_event: &'a PduEvent, room_id: &'a RoomId,
		room_version_id: &'a RoomVersionId, pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> AsyncRecursiveCanonicalJsonVec<'a> {
		Box::pin(async move {
			let back_off = |id| async {
				match services()
					.globals
					.bad_event_ratelimiter
					.write()
					.await
					.entry(id)
				{
					hash_map::Entry::Vacant(e) => {
						e.insert((Instant::now(), 1));
					},
					hash_map::Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
				}
			};

			let mut events_with_auth_events = vec![];
			for id in events {
				// a. Look in the main timeline (pduid_pdu tree)
				// b. Look at outlier pdu tree
				// (get_pdu_json checks both)
				if let Ok(Some(local_pdu)) = services().rooms.timeline.get_pdu(id) {
					trace!("Found {} in db", id);
					events_with_auth_events.push((id, Some(local_pdu), vec![]));
					continue;
				}

				// c. Ask origin server over federation
				// We also handle its auth chain here so we don't get a stack overflow in
				// handle_outlier_pdu.
				let mut todo_auth_events = vec![Arc::clone(id)];
				let mut events_in_reverse_order = Vec::new();
				let mut events_all = HashSet::new();
				let mut i = 0;
				while let Some(next_id) = todo_auth_events.pop() {
					if let Some((time, tries)) = services()
						.globals
						.bad_event_ratelimiter
						.read()
						.await
						.get(&*next_id)
					{
						// Exponential backoff
						let mut min_elapsed_duration = Duration::from_secs(5 * 60) * (*tries) * (*tries);
						if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
							min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
						}

						if time.elapsed() < min_elapsed_duration {
							info!("Backing off from {}", next_id);
							continue;
						}
					}

					if events_all.contains(&next_id) {
						continue;
					}

					i += 1;
					if i % 100 == 0 {
						tokio::task::yield_now().await;
					}

					if let Ok(Some(_)) = services().rooms.timeline.get_pdu(&next_id) {
						trace!("Found {} in db", next_id);
						continue;
					}

					info!("Fetching {} over federation.", next_id);
					match services()
						.sending
						.send_federation_request(
							origin,
							get_event::v1::Request {
								event_id: (*next_id).to_owned(),
							},
						)
						.await
					{
						Ok(res) => {
							info!("Got {} over federation", next_id);
							let Ok((calculated_event_id, value)) =
								pdu::gen_event_id_canonical_json(&res.pdu, room_version_id)
							else {
								back_off((*next_id).to_owned()).await;
								continue;
							};

							if calculated_event_id != *next_id {
								warn!(
									"Server didn't return event id we requested: requested: {}, we got {}. Event: {:?}",
									next_id, calculated_event_id, &res.pdu
								);
							}

							if let Some(auth_events) = value.get("auth_events").and_then(|c| c.as_array()) {
								for auth_event in auth_events {
									if let Ok(auth_event) = serde_json::from_value(auth_event.clone().into()) {
										let a: Arc<EventId> = auth_event;
										todo_auth_events.push(a);
									} else {
										warn!("Auth event id is not valid");
									}
								}
							} else {
								warn!("Auth event list invalid");
							}

							events_in_reverse_order.push((next_id.clone(), value));
							events_all.insert(next_id);
						},
						Err(e) => {
							warn!("Failed to fetch event {next_id}: {e}");
							back_off((*next_id).to_owned()).await;
						},
					}
				}
				events_with_auth_events.push((id, None, events_in_reverse_order));
			}

			// We go through all the signatures we see on the PDUs and their unresolved
			// dependencies and fetch the corresponding signing keys
			info!("fetch_required_signing_keys for {}", origin);
			self.fetch_required_signing_keys(
				events_with_auth_events
					.iter()
					.flat_map(|(_id, _local_pdu, events)| events)
					.map(|(_event_id, event)| event),
				pub_key_map,
			)
			.await
			.unwrap_or_else(|e| {
				warn!("Could not fetch all signatures for PDUs from {}: {:?}", origin, e);
			});

			let mut pdus = vec![];
			for (id, local_pdu, events_in_reverse_order) in events_with_auth_events {
				// a. Look in the main timeline (pduid_pdu tree)
				// b. Look at outlier pdu tree
				// (get_pdu_json checks both)
				if let Some(local_pdu) = local_pdu {
					trace!("Found {} in db", id);
					pdus.push((local_pdu, None));
				}
				for (next_id, value) in events_in_reverse_order.iter().rev() {
					if let Some((time, tries)) = services()
						.globals
						.bad_event_ratelimiter
						.read()
						.await
						.get(&**next_id)
					{
						// Exponential backoff
						let mut min_elapsed_duration = Duration::from_secs(5 * 60) * (*tries) * (*tries);
						if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
							min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
						}

						if time.elapsed() < min_elapsed_duration {
							info!("Backing off from {}", next_id);
							continue;
						}
					}

					match self
						.handle_outlier_pdu(origin, create_event, next_id, room_id, value.clone(), true, pub_key_map)
						.await
					{
						Ok((pdu, json)) => {
							if next_id == id {
								pdus.push((pdu, Some(json)));
							}
						},
						Err(e) => {
							warn!("Authentication of event {} failed: {:?}", next_id, e);
							back_off((**next_id).to_owned()).await;
						},
					}
				}
			}
			pdus
		})
	}

	async fn fetch_unknown_prev_events(
		&self, origin: &ServerName, create_event: &PduEvent, room_id: &RoomId, room_version_id: &RoomVersionId,
		pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>, initial_set: Vec<Arc<EventId>>,
	) -> Result<(
		Vec<Arc<EventId>>,
		HashMap<Arc<EventId>, (Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)>,
	)> {
		let mut graph: HashMap<Arc<EventId>, _> = HashMap::new();
		let mut eventid_info = HashMap::new();
		let mut todo_outlier_stack: Vec<Arc<EventId>> = initial_set;

		let first_pdu_in_room = services()
			.rooms
			.timeline
			.first_pdu_in_room(room_id)?
			.ok_or_else(|| Error::bad_database("Failed to find first pdu in db."))?;

		let mut amount = 0;

		while let Some(prev_event_id) = todo_outlier_stack.pop() {
			if let Some((pdu, json_opt)) = self
				.fetch_and_handle_outliers(
					origin,
					&[prev_event_id.clone()],
					create_event,
					room_id,
					room_version_id,
					pub_key_map,
				)
				.await
				.pop()
			{
				self.check_room_id(room_id, &pdu)?;

				if amount > services().globals.max_fetch_prev_events() {
					// Max limit reached
					info!(
						"Max prev event limit reached! Limit: {}",
						services().globals.max_fetch_prev_events()
					);
					graph.insert(prev_event_id.clone(), HashSet::new());
					continue;
				}

				if let Some(json) = json_opt.or_else(|| {
					services()
						.rooms
						.outlier
						.get_outlier_pdu_json(&prev_event_id)
						.ok()
						.flatten()
				}) {
					if pdu.origin_server_ts > first_pdu_in_room.origin_server_ts {
						amount += 1;
						for prev_prev in &pdu.prev_events {
							if !graph.contains_key(prev_prev) {
								todo_outlier_stack.push(prev_prev.clone());
							}
						}

						graph.insert(prev_event_id.clone(), pdu.prev_events.iter().cloned().collect());
					} else {
						// Time based check failed
						graph.insert(prev_event_id.clone(), HashSet::new());
					}

					eventid_info.insert(prev_event_id.clone(), (pdu, json));
				} else {
					// Get json failed, so this was not fetched over federation
					graph.insert(prev_event_id.clone(), HashSet::new());
				}
			} else {
				// Fetch and handle failed
				graph.insert(prev_event_id.clone(), HashSet::new());
			}
		}

		let sorted = state_res::lexicographical_topological_sort(&graph, |event_id| {
			// This return value is the key used for sorting events,
			// events are then sorted by power level, time,
			// and lexically by event_id.
			Ok((
				int!(0),
				MilliSecondsSinceUnixEpoch(
					eventid_info
						.get(event_id)
						.map_or_else(|| uint!(0), |info| info.0.origin_server_ts),
				),
			))
		})
		.map_err(|e| {
			error!("Error sorting prev events: {e}");
			Error::bad_database("Error sorting prev events")
		})?;

		Ok((sorted, eventid_info))
	}

	#[tracing::instrument(skip_all)]
	pub(crate) async fn fetch_required_signing_keys<'a, E>(
		&'a self, events: E, pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> Result<()>
	where
		E: IntoIterator<Item = &'a BTreeMap<String, CanonicalJsonValue>>,
	{
		let mut server_key_ids = HashMap::new();

		for event in events {
			debug!("Fetching keys for event: {event:?}");
			for (signature_server, signature) in event
				.get("signatures")
				.ok_or(Error::BadServerResponse("No signatures in server response pdu."))?
				.as_object()
				.ok_or(Error::BadServerResponse("Invalid signatures object in server response pdu."))?
			{
				let signature_object = signature.as_object().ok_or(Error::BadServerResponse(
					"Invalid signatures content object in server response pdu.",
				))?;

				for signature_id in signature_object.keys() {
					server_key_ids
						.entry(signature_server.clone())
						.or_insert_with(HashSet::new)
						.insert(signature_id.clone());
				}
			}
		}

		if server_key_ids.is_empty() {
			// Nothing to do, can exit early
			trace!("server_key_ids is empty, not fetching any keys");
			return Ok(());
		}

		info!(
			"Fetch keys for {}",
			server_key_ids
				.keys()
				.cloned()
				.collect::<Vec<_>>()
				.join(", ")
		);

		let mut server_keys: FuturesUnordered<_> = server_key_ids
			.into_iter()
			.map(|(signature_server, signature_ids)| async {
				let fetch_res = self
					.fetch_signing_keys_for_server(
						signature_server.as_str().try_into().map_err(|e| {
							info!("Invalid servername in signatures of server response pdu: {e}");
							(
								signature_server.clone(),
								Error::BadServerResponse("Invalid servername in signatures of server response pdu."),
							)
						})?,
						signature_ids.into_iter().collect(), // HashSet to Vec
					)
					.await;

				match fetch_res {
					Ok(keys) => Ok((signature_server, keys)),
					Err(e) => {
						warn!("Signature verification failed: Could not fetch signing key for {signature_server}: {e}",);
						Err((signature_server, e))
					},
				}
			})
			.collect();

		while let Some(fetch_res) = server_keys.next().await {
			match fetch_res {
				Ok((signature_server, keys)) => {
					pub_key_map
						.write()
						.await
						.insert(signature_server.clone(), keys);
				},
				Err((signature_server, e)) => {
					warn!("Failed to fetch keys for {}: {:?}", signature_server, e);
				},
			}
		}

		Ok(())
	}

	// Gets a list of servers for which we don't have the signing key yet. We go
	// over the PDUs and either cache the key or add it to the list that needs to be
	// retrieved.
	async fn get_server_keys_from_cache(
		&self, pdu: &RawJsonValue,
		servers: &mut BTreeMap<OwnedServerName, BTreeMap<OwnedServerSigningKeyId, QueryCriteria>>,
		room_version: &RoomVersionId,
		pub_key_map: &mut RwLockWriteGuard<'_, BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> Result<()> {
		let value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
			error!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
			Error::BadServerResponse("Invalid PDU in server response")
		})?;

		let event_id = format!(
			"${}",
			ruma::signatures::reference_hash(&value, room_version).expect("ruma can calculate reference hashes")
		);
		let event_id = <&EventId>::try_from(event_id.as_str()).expect("ruma's reference hashes are valid event ids");

		if let Some((time, tries)) = services()
			.globals
			.bad_event_ratelimiter
			.read()
			.await
			.get(event_id)
		{
			// Exponential backoff
			let mut min_elapsed_duration = Duration::from_secs(5 * 60) * (*tries) * (*tries);
			if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
				min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
			}

			if time.elapsed() < min_elapsed_duration {
				debug!("Backing off from {}", event_id);
				return Err(Error::BadServerResponse("bad event, still backing off"));
			}
		}

		let signatures = value
			.get("signatures")
			.ok_or(Error::BadServerResponse("No signatures in server response pdu."))?
			.as_object()
			.ok_or(Error::BadServerResponse("Invalid signatures object in server response pdu."))?;

		for (signature_server, signature) in signatures {
			let signature_object = signature.as_object().ok_or(Error::BadServerResponse(
				"Invalid signatures content object in server response pdu.",
			))?;

			let signature_ids = signature_object.keys().cloned().collect::<Vec<_>>();

			let contains_all_ids =
				|keys: &BTreeMap<String, Base64>| signature_ids.iter().all(|id| keys.contains_key(id));

			let origin = <&ServerName>::try_from(signature_server.as_str()).map_err(|e| {
				info!("Invalid servername in signatures of server response pdu: {e}");
				Error::BadServerResponse("Invalid servername in signatures of server response pdu.")
			})?;

			if servers.contains_key(origin) || pub_key_map.contains_key(origin.as_str()) {
				continue;
			}

			debug!("Loading signing keys for {}", origin);

			let result: BTreeMap<_, _> = services()
				.globals
				.signing_keys_for(origin)?
				.into_iter()
				.map(|(k, v)| (k.to_string(), v.key))
				.collect();

			if !contains_all_ids(&result) {
				debug!("Signing key not loaded for {}", origin);
				servers.insert(origin.to_owned(), BTreeMap::new());
			}

			pub_key_map.insert(origin.to_string(), result);
		}

		Ok(())
	}

	/// Batch requests homeserver signing keys from trusted notary key servers
	/// (`trusted_servers` config option)
	async fn batch_request_signing_keys(
		&self, mut servers: BTreeMap<OwnedServerName, BTreeMap<OwnedServerSigningKeyId, QueryCriteria>>,
		pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> Result<()> {
		for server in services().globals.trusted_servers() {
			info!("Asking batch signing keys from trusted server {}", server);
			match services()
				.sending
				.send_federation_request(
					server,
					get_remote_server_keys_batch::v2::Request {
						server_keys: servers.clone(),
					},
				)
				.await
			{
				Ok(keys) => {
					debug!("Got signing keys: {:?}", keys);
					let mut pkm = pub_key_map.write().await;
					for k in keys.server_keys {
						let k = match k.deserialize() {
							Ok(key) => key,
							Err(e) => {
								warn!("Received error {e} while fetching keys from trusted server {server}");
								warn!("{}", k.into_json());
								continue;
							},
						};

						// TODO: Check signature from trusted server?
						servers.remove(&k.server_name);

						let result = services()
							.globals
							.add_signing_key(&k.server_name, k.clone())?
							.into_iter()
							.map(|(k, v)| (k.to_string(), v.key))
							.collect::<BTreeMap<_, _>>();

						pkm.insert(k.server_name.to_string(), result);
					}
				},
				Err(e) => {
					warn!(
						"Failed sending batched key request to trusted key server {server} for the remote servers \
						 {:?}: {e}",
						servers
					);
				},
			}
		}

		Ok(())
	}

	/// Requests multiple homeserver signing keys from individual servers (not
	/// trused notary servers)
	async fn request_signing_keys(
		&self, servers: BTreeMap<OwnedServerName, BTreeMap<OwnedServerSigningKeyId, QueryCriteria>>,
		pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> Result<()> {
		info!("Asking individual servers for signing keys: {servers:?}");
		let mut futures: FuturesUnordered<_> = servers
			.into_keys()
			.map(|server| async move {
				(
					services()
						.sending
						.send_federation_request(&server, get_server_keys::v2::Request::new())
						.await,
					server,
				)
			})
			.collect();

		while let Some(result) = futures.next().await {
			debug!("Received new Future result");
			if let (Ok(get_keys_response), origin) = result {
				info!("Result is from {origin}");
				if let Ok(key) = get_keys_response.server_key.deserialize() {
					let result: BTreeMap<_, _> = services()
						.globals
						.add_signing_key(&origin, key)?
						.into_iter()
						.map(|(k, v)| (k.to_string(), v.key))
						.collect();
					pub_key_map.write().await.insert(origin.to_string(), result);
				}
			}
			debug!("Done handling Future result");
		}

		Ok(())
	}

	pub(crate) async fn fetch_join_signing_keys(
		&self, event: &create_join_event::v2::Response, room_version: &RoomVersionId,
		pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
	) -> Result<()> {
		let mut servers: BTreeMap<OwnedServerName, BTreeMap<OwnedServerSigningKeyId, QueryCriteria>> = BTreeMap::new();

		{
			let mut pkm = pub_key_map.write().await;

			// Try to fetch keys, failure is okay
			// Servers we couldn't find in the cache will be added to `servers`
			for pdu in &event.room_state.state {
				_ = self
					.get_server_keys_from_cache(pdu, &mut servers, room_version, &mut pkm)
					.await;
			}
			for pdu in &event.room_state.auth_chain {
				_ = self
					.get_server_keys_from_cache(pdu, &mut servers, room_version, &mut pkm)
					.await;
			}

			drop(pkm);
		};

		if servers.is_empty() {
			trace!("We had all keys cached locally, not fetching any keys from remote servers");
			return Ok(());
		}

		if services().globals.query_trusted_key_servers_first() {
			info!(
				"query_trusted_key_servers_first is set to true, querying notary trusted key servers first for \
				 homeserver signing keys."
			);

			self.batch_request_signing_keys(servers.clone(), pub_key_map)
				.await?;

			if servers.is_empty() {
				info!("Trusted server supplied all signing keys, no more keys to fetch");
				return Ok(());
			}

			info!("Remaining servers left that the notary/trusted servers did not provide: {servers:?}");

			self.request_signing_keys(servers.clone(), pub_key_map)
				.await?;
		} else {
			info!("query_trusted_key_servers_first is set to false, querying individual homeservers first");

			self.request_signing_keys(servers.clone(), pub_key_map)
				.await?;

			if servers.is_empty() {
				info!("Individual homeservers supplied all signing keys, no more keys to fetch");
				return Ok(());
			}

			info!("Remaining servers left the individual homeservers did not provide: {servers:?}");

			self.batch_request_signing_keys(servers.clone(), pub_key_map)
				.await?;
		}

		debug!("Search for signing keys done");

		/*if servers.is_empty() {
			warn!("Failed to find homeserver signing keys for the remaining servers: {servers:?}");
		}*/

		Ok(())
	}

	/// Returns Ok if the acl allows the server
	pub fn acl_check(&self, server_name: &ServerName, room_id: &RoomId) -> Result<()> {
		let acl_event = if let Some(acl) =
			services()
				.rooms
				.state_accessor
				.room_state_get(room_id, &StateEventType::RoomServerAcl, "")?
		{
			debug!("ACL event found: {acl:?}");
			acl
		} else {
			debug!("No ACL event found");
			return Ok(());
		};

		let acl_event_content: RoomServerAclEventContent = match serde_json::from_str(acl_event.content.get()) {
			Ok(content) => {
				debug!("Found ACL event contents: {content:?}");
				content
			},
			Err(e) => {
				warn!("Invalid ACL event: {e}");
				return Ok(());
			},
		};

		if acl_event_content.allow.is_empty() {
			warn!("Ignoring broken ACL event (allow key is empty)");
			// Ignore broken acl events
			return Ok(());
		}

		if acl_event_content.is_allowed(server_name) {
			debug!("server {server_name} is allowed by ACL");
			Ok(())
		} else {
			info!("Server {} was denied by room ACL in {}", server_name, room_id);
			Err(Error::BadRequest(ErrorKind::forbidden(), "Server was denied by room ACL"))
		}
	}

	/// Search the DB for the signing keys of the given server, if we don't have
	/// them fetch them from the server and save to our DB.
	#[tracing::instrument(skip_all)]
	pub async fn fetch_signing_keys_for_server(
		&self, origin: &ServerName, signature_ids: Vec<String>,
	) -> Result<BTreeMap<String, Base64>> {
		let contains_all_ids = |keys: &BTreeMap<String, Base64>| signature_ids.iter().all(|id| keys.contains_key(id));

		let permit = services()
			.globals
			.servername_ratelimiter
			.read()
			.await
			.get(origin)
			.map(|s| Arc::clone(s).acquire_owned());

		let permit = if let Some(p) = permit {
			p
		} else {
			let mut write = services().globals.servername_ratelimiter.write().await;
			let s = Arc::clone(
				write
					.entry(origin.to_owned())
					.or_insert_with(|| Arc::new(Semaphore::new(1))),
			);

			s.acquire_owned()
		}
		.await;

		let back_off = |id| async {
			match services()
				.globals
				.bad_signature_ratelimiter
				.write()
				.await
				.entry(id)
			{
				hash_map::Entry::Vacant(e) => {
					e.insert((Instant::now(), 1));
				},
				hash_map::Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
			}
		};

		if let Some((time, tries)) = services()
			.globals
			.bad_signature_ratelimiter
			.read()
			.await
			.get(&signature_ids)
		{
			// Exponential backoff
			let mut min_elapsed_duration = Duration::from_secs(5 * 60) * (*tries) * (*tries);
			if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
				min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
			}

			if time.elapsed() < min_elapsed_duration {
				debug!("Backing off from {:?}", signature_ids);
				return Err(Error::BadServerResponse("bad signature, still backing off"));
			}
		}

		let mut result: BTreeMap<_, _> = services()
			.globals
			.signing_keys_for(origin)?
			.into_iter()
			.map(|(k, v)| (k.to_string(), v.key))
			.collect();

		if contains_all_ids(&result) {
			trace!("We have all homeserver signing keys locally for {origin}, not fetching any remotely");
			return Ok(result);
		}

		// i didnt split this out into their own functions because it's relatively small
		if services().globals.query_trusted_key_servers_first() {
			info!(
				"query_trusted_key_servers_first is set to true, querying notary trusted servers first for {origin} \
				 keys"
			);

			for server in services().globals.trusted_servers() {
				debug!("Asking notary server {server} for {origin}'s signing key");
				if let Some(server_keys) = services()
					.sending
					.send_federation_request(
						server,
						get_remote_server_keys::v2::Request::new(
							origin.to_owned(),
							MilliSecondsSinceUnixEpoch::from_system_time(
								SystemTime::now()
									.checked_add(Duration::from_secs(3600))
									.expect("SystemTime too large"),
							)
							.expect("time is valid"),
						),
					)
					.await
					.ok()
					.map(|resp| {
						resp.server_keys
							.into_iter()
							.filter_map(|e| e.deserialize().ok())
							.collect::<Vec<_>>()
					}) {
					debug!("Got signing keys: {:?}", server_keys);
					for k in server_keys {
						services().globals.add_signing_key(origin, k.clone())?;
						result.extend(
							k.verify_keys
								.into_iter()
								.map(|(k, v)| (k.to_string(), v.key)),
						);
						result.extend(
							k.old_verify_keys
								.into_iter()
								.map(|(k, v)| (k.to_string(), v.key)),
						);
					}

					if contains_all_ids(&result) {
						return Ok(result);
					}
				}
			}

			debug!("Asking {origin} for their signing keys over federation");
			if let Some(server_key) = services()
				.sending
				.send_federation_request(origin, get_server_keys::v2::Request::new())
				.await
				.ok()
				.and_then(|resp| resp.server_key.deserialize().ok())
			{
				services()
					.globals
					.add_signing_key(origin, server_key.clone())?;

				result.extend(
					server_key
						.verify_keys
						.into_iter()
						.map(|(k, v)| (k.to_string(), v.key)),
				);
				result.extend(
					server_key
						.old_verify_keys
						.into_iter()
						.map(|(k, v)| (k.to_string(), v.key)),
				);

				if contains_all_ids(&result) {
					return Ok(result);
				}
			}
		} else {
			info!("query_trusted_key_servers_first is set to false, querying {origin} first");

			debug!("Asking {origin} for their signing keys over federation");
			if let Some(server_key) = services()
				.sending
				.send_federation_request(origin, get_server_keys::v2::Request::new())
				.await
				.ok()
				.and_then(|resp| resp.server_key.deserialize().ok())
			{
				services()
					.globals
					.add_signing_key(origin, server_key.clone())?;

				result.extend(
					server_key
						.verify_keys
						.into_iter()
						.map(|(k, v)| (k.to_string(), v.key)),
				);
				result.extend(
					server_key
						.old_verify_keys
						.into_iter()
						.map(|(k, v)| (k.to_string(), v.key)),
				);

				if contains_all_ids(&result) {
					return Ok(result);
				}
			}

			for server in services().globals.trusted_servers() {
				debug!("Asking notary server {server} for {origin}'s signing key");
				if let Some(server_keys) = services()
					.sending
					.send_federation_request(
						server,
						get_remote_server_keys::v2::Request::new(
							origin.to_owned(),
							MilliSecondsSinceUnixEpoch::from_system_time(
								SystemTime::now()
									.checked_add(Duration::from_secs(3600))
									.expect("SystemTime too large"),
							)
							.expect("time is valid"),
						),
					)
					.await
					.ok()
					.map(|resp| {
						resp.server_keys
							.into_iter()
							.filter_map(|e| e.deserialize().ok())
							.collect::<Vec<_>>()
					}) {
					debug!("Got signing keys: {:?}", server_keys);
					for k in server_keys {
						services().globals.add_signing_key(origin, k.clone())?;
						result.extend(
							k.verify_keys
								.into_iter()
								.map(|(k, v)| (k.to_string(), v.key)),
						);
						result.extend(
							k.old_verify_keys
								.into_iter()
								.map(|(k, v)| (k.to_string(), v.key)),
						);
					}

					if contains_all_ids(&result) {
						return Ok(result);
					}
				}
			}
		}

		drop(permit);

		back_off(signature_ids).await;

		warn!("Failed to find public key for server: {origin}");
		Err(Error::BadServerResponse("Failed to find public key for server"))
	}

	fn check_room_id(&self, room_id: &RoomId, pdu: &PduEvent) -> Result<()> {
		if pdu.room_id != room_id {
			warn!("Found event from room {} in room {}", pdu.room_id, room_id);
			return Err(Error::BadRequest(ErrorKind::InvalidParam, "Event has wrong room id"));
		}
		Ok(())
	}
}
