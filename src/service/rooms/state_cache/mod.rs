use std::{collections::HashSet, sync::Arc};

pub use data::Data;
use ruma::{
	events::{
		direct::DirectEvent,
		ignored_user_list::IgnoredUserListEvent,
		room::{
			create::RoomCreateEventContent,
			member::{MembershipState, RoomMemberEventContent},
		},
		AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
	},
	serde::Raw,
	OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, ServerName, UserId,
};
use tracing::warn;

use crate::{service::appservice::RegistrationInfo, services, Error, Result};

mod data;

pub struct Service {
	pub db: &'static dyn Data,
}

impl Service {
	/// Update current membership data.
	#[tracing::instrument(skip(self, last_state))]
	pub async fn update_membership(
		&self, room_id: &RoomId, user_id: &UserId, membership_event: RoomMemberEventContent, sender: &UserId,
		last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>, update_joined_count: bool,
	) -> Result<()> {
		let membership = membership_event.membership;

		// Keep track what remote users exist by adding them as "deactivated" users
		//
		// TODO: use futures to update remote profiles without blocking the membership
		// update
		#[allow(clippy::collapsible_if)]
		if user_id.server_name() != services().globals.server_name() {
			if !services().users.exists(user_id)? {
				services().users.create(user_id, None)?;
			}

			/*
			// Try to update our local copy of the user if ours does not match
			if ((services().users.displayname(user_id)? != membership_event.displayname)
				|| (services().users.avatar_url(user_id)? != membership_event.avatar_url)
				|| (services().users.blurhash(user_id)? != membership_event.blurhash))
				&& (membership != MembershipState::Leave)
			{
				let response = services()
					.sending
					.send_federation_request(
						user_id.server_name(),
						federation::query::get_profile_information::v1::Request {
							user_id: user_id.into(),
							field: None, // we want the full user's profile to update locally too
						},
					)
					.await;

				services().users.set_displayname(user_id, response.displayname.clone()).await?;
				services().users.set_avatar_url(user_id, response.avatar_url).await?;
				services().users.set_blurhash(user_id, response.blurhash).await?;
			};
			*/
		}

		match &membership {
			MembershipState::Join => {
				// Check if the user never joined this room
				if !self.once_joined(user_id, room_id)? {
					// Add the user ID to the join list then
					self.db.mark_as_once_joined(user_id, room_id)?;

					// Check if the room has a predecessor
					if let Some(predecessor) = services()
						.rooms
						.state_accessor
						.room_state_get(room_id, &StateEventType::RoomCreate, "")?
						.and_then(|create| serde_json::from_str(create.content.get()).ok())
						.and_then(|content: RoomCreateEventContent| content.predecessor)
					{
						// Copy user settings from predecessor to the current room:
						// - Push rules
						//
						// TODO: finish this once push rules are implemented.
						//
						// let mut push_rules_event_content: PushRulesEvent = account_data
						//     .get(
						//         None,
						//         user_id,
						//         EventType::PushRules,
						//     )?;
						//
						// NOTE: find where `predecessor.room_id` match
						//       and update to `room_id`.
						//
						// account_data
						//     .update(
						//         None,
						//         user_id,
						//         EventType::PushRules,
						//         &push_rules_event_content,
						//         globals,
						//     )
						//     .ok();

						// Copy old tags to new room
						if let Some(tag_event) = services()
							.account_data
							.get(Some(&predecessor.room_id), user_id, RoomAccountDataEventType::Tag)?
							.map(|event| {
								serde_json::from_str(event.get()).map_err(|e| {
									warn!("Invalid account data event in db: {e:?}");
									Error::BadDatabase("Invalid account data event in db.")
								})
							}) {
							services()
								.account_data
								.update(Some(room_id), user_id, RoomAccountDataEventType::Tag, &tag_event?)
								.ok();
						};

						// Copy direct chat flag
						if let Some(direct_event) = services()
							.account_data
							.get(None, user_id, GlobalAccountDataEventType::Direct.to_string().into())?
							.map(|event| {
								serde_json::from_str::<DirectEvent>(event.get()).map_err(|e| {
									warn!("Invalid account data event in db: {e:?}");
									Error::BadDatabase("Invalid account data event in db.")
								})
							}) {
							let mut direct_event = direct_event?;
							let mut room_ids_updated = false;

							for room_ids in direct_event.content.0.values_mut() {
								if room_ids.iter().any(|r| r == &predecessor.room_id) {
									room_ids.push(room_id.to_owned());
									room_ids_updated = true;
								}
							}

							if room_ids_updated {
								services().account_data.update(
									None,
									user_id,
									GlobalAccountDataEventType::Direct.to_string().into(),
									&serde_json::to_value(&direct_event).expect("to json always works"),
								)?;
							}
						};
					}
				}

				self.db.mark_as_joined(user_id, room_id)?;
			},
			MembershipState::Invite => {
				// We want to know if the sender is ignored by the receiver
				let is_ignored = services()
					.account_data
					.get(
						None,    // Ignored users are in global account data
						user_id, // Receiver
						GlobalAccountDataEventType::IgnoredUserList
							.to_string()
							.into(),
					)?
					.map(|event| {
						serde_json::from_str::<IgnoredUserListEvent>(event.get()).map_err(|e| {
							warn!("Invalid account data event in db: {e:?}");
							Error::BadDatabase("Invalid account data event in db.")
						})
					})
					.transpose()?
					.map_or(false, |ignored| {
						ignored
							.content
							.ignored_users
							.iter()
							.any(|(user, _details)| user == sender)
					});

				if is_ignored {
					return Ok(());
				}

				self.db.mark_as_invited(user_id, room_id, last_state)?;
			},
			MembershipState::Leave | MembershipState::Ban => {
				self.db.mark_as_left(user_id, room_id)?;
			},
			_ => {},
		}

		if update_joined_count {
			self.update_joined_count(room_id)?;
		}

		Ok(())
	}

	#[tracing::instrument(skip(self, room_id))]
	pub fn update_joined_count(&self, room_id: &RoomId) -> Result<()> { self.db.update_joined_count(room_id) }

	#[tracing::instrument(skip(self, room_id))]
	pub fn get_our_real_users(&self, room_id: &RoomId) -> Result<Arc<HashSet<OwnedUserId>>> {
		self.db.get_our_real_users(room_id)
	}

	#[tracing::instrument(skip(self, room_id, appservice))]
	pub fn appservice_in_room(&self, room_id: &RoomId, appservice: &RegistrationInfo) -> Result<bool> {
		self.db.appservice_in_room(room_id, appservice)
	}

	/// Makes a user forget a room.
	#[tracing::instrument(skip(self))]
	pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> { self.db.forget(room_id, user_id) }

	/// Returns an iterator of all servers participating in this room.
	#[tracing::instrument(skip(self))]
	pub fn room_servers<'a>(&'a self, room_id: &RoomId) -> impl Iterator<Item = Result<OwnedServerName>> + 'a {
		self.db.room_servers(room_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn server_in_room(&self, server: &ServerName, room_id: &RoomId) -> Result<bool> {
		self.db.server_in_room(server, room_id)
	}

	/// Returns an iterator of all rooms a server participates in (as far as we
	/// know).
	#[tracing::instrument(skip(self))]
	pub fn server_rooms<'a>(&'a self, server: &ServerName) -> impl Iterator<Item = Result<OwnedRoomId>> + 'a {
		self.db.server_rooms(server)
	}

	/// Returns true if server can see user by sharing at least one room.
	#[tracing::instrument(skip(self))]
	pub fn server_sees_user(&self, server: &ServerName, user_id: &UserId) -> Result<bool> {
		Ok(self
			.server_rooms(server)
			.filter_map(Result::ok)
			.any(|room_id: OwnedRoomId| self.is_joined(user_id, &room_id).unwrap_or(false)))
	}

	/// Returns true if user_a and user_b share at least one room.
	#[tracing::instrument(skip(self))]
	pub fn user_sees_user(&self, user_a: &UserId, user_b: &UserId) -> Result<bool> {
		// Minimize number of point-queries by iterating user with least nr rooms
		let (a, b) = if self.rooms_joined(user_a).count() < self.rooms_joined(user_b).count() {
			(user_a, user_b)
		} else {
			(user_b, user_a)
		};

		Ok(self
			.rooms_joined(a)
			.filter_map(Result::ok)
			.any(|room_id| self.is_joined(b, &room_id).unwrap_or(false)))
	}

	/// Returns an iterator over all joined members of a room.
	#[tracing::instrument(skip(self))]
	pub fn room_members<'a>(&'a self, room_id: &RoomId) -> impl Iterator<Item = Result<OwnedUserId>> + 'a {
		self.db.room_members(room_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn room_joined_count(&self, room_id: &RoomId) -> Result<Option<u64>> { self.db.room_joined_count(room_id) }

	#[tracing::instrument(skip(self))]
	pub fn room_invited_count(&self, room_id: &RoomId) -> Result<Option<u64>> { self.db.room_invited_count(room_id) }

	/// Returns an iterator over all User IDs who ever joined a room.
	#[tracing::instrument(skip(self))]
	pub fn room_useroncejoined<'a>(&'a self, room_id: &RoomId) -> impl Iterator<Item = Result<OwnedUserId>> + 'a {
		self.db.room_useroncejoined(room_id)
	}

	/// Returns an iterator over all invited members of a room.
	#[tracing::instrument(skip(self))]
	pub fn room_members_invited<'a>(&'a self, room_id: &RoomId) -> impl Iterator<Item = Result<OwnedUserId>> + 'a {
		self.db.room_members_invited(room_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
		self.db.get_invite_count(room_id, user_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
		self.db.get_left_count(room_id, user_id)
	}

	/// Returns an iterator over all rooms this user joined.
	#[tracing::instrument(skip(self))]
	pub fn rooms_joined<'a>(&'a self, user_id: &UserId) -> impl Iterator<Item = Result<OwnedRoomId>> + 'a {
		self.db.rooms_joined(user_id)
	}

	/// Returns an iterator over all rooms a user was invited to.
	#[tracing::instrument(skip(self))]
	pub fn rooms_invited<'a>(
		&'a self, user_id: &UserId,
	) -> impl Iterator<Item = Result<(OwnedRoomId, Vec<Raw<AnyStrippedStateEvent>>)>> + 'a {
		self.db.rooms_invited(user_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn invite_state(&self, user_id: &UserId, room_id: &RoomId) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
		self.db.invite_state(user_id, room_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn left_state(&self, user_id: &UserId, room_id: &RoomId) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
		self.db.left_state(user_id, room_id)
	}

	/// Returns an iterator over all rooms a user left.
	#[tracing::instrument(skip(self))]
	pub fn rooms_left<'a>(
		&'a self, user_id: &UserId,
	) -> impl Iterator<Item = Result<(OwnedRoomId, Vec<Raw<AnySyncStateEvent>>)>> + 'a {
		self.db.rooms_left(user_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
		self.db.once_joined(user_id, room_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> { self.db.is_joined(user_id, room_id) }

	#[tracing::instrument(skip(self))]
	pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
		self.db.is_invited(user_id, room_id)
	}

	#[tracing::instrument(skip(self))]
	pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> { self.db.is_left(user_id, room_id) }
}
