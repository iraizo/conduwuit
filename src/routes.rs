use std::future::Future;

use axum::{
	extract::FromRequestParts,
	response::IntoResponse,
	routing::{get, on, post, MethodFilter},
	Router,
};
use conduit::{
	api::{client_server, server_server},
	Error, Result, Ruma, RumaResponse,
};
use http::{Method, Uri};
use ruma::api::{client::error::ErrorKind, IncomingRequest};
use tracing::{info, warn};

pub fn routes() -> Router {
	Router::new()
		.ruma_route(client_server::get_supported_versions_route)
		.ruma_route(client_server::get_register_available_route)
		.ruma_route(client_server::register_route)
		.ruma_route(client_server::get_login_types_route)
		.ruma_route(client_server::login_route)
		.ruma_route(client_server::whoami_route)
		.ruma_route(client_server::logout_route)
		.ruma_route(client_server::logout_all_route)
		.ruma_route(client_server::change_password_route)
		.ruma_route(client_server::deactivate_route)
		.ruma_route(client_server::third_party_route)
		.ruma_route(client_server::request_3pid_management_token_via_email_route)
		.ruma_route(client_server::request_3pid_management_token_via_msisdn_route)
		.ruma_route(client_server::get_capabilities_route)
		.ruma_route(client_server::get_pushrules_all_route)
		.ruma_route(client_server::set_pushrule_route)
		.ruma_route(client_server::get_pushrule_route)
		.ruma_route(client_server::set_pushrule_enabled_route)
		.ruma_route(client_server::get_pushrule_enabled_route)
		.ruma_route(client_server::get_pushrule_actions_route)
		.ruma_route(client_server::set_pushrule_actions_route)
		.ruma_route(client_server::delete_pushrule_route)
		.ruma_route(client_server::get_room_event_route)
		.ruma_route(client_server::get_room_aliases_route)
		.ruma_route(client_server::get_filter_route)
		.ruma_route(client_server::create_filter_route)
		.ruma_route(client_server::set_global_account_data_route)
		.ruma_route(client_server::set_room_account_data_route)
		.ruma_route(client_server::get_global_account_data_route)
		.ruma_route(client_server::get_room_account_data_route)
		.ruma_route(client_server::set_displayname_route)
		.ruma_route(client_server::get_displayname_route)
		.ruma_route(client_server::set_avatar_url_route)
		.ruma_route(client_server::get_avatar_url_route)
		.ruma_route(client_server::get_profile_route)
		.ruma_route(client_server::set_presence_route)
		.ruma_route(client_server::get_presence_route)
		.ruma_route(client_server::upload_keys_route)
		.ruma_route(client_server::get_keys_route)
		.ruma_route(client_server::claim_keys_route)
		.ruma_route(client_server::create_backup_version_route)
		.ruma_route(client_server::update_backup_version_route)
		.ruma_route(client_server::delete_backup_version_route)
		.ruma_route(client_server::get_latest_backup_info_route)
		.ruma_route(client_server::get_backup_info_route)
		.ruma_route(client_server::add_backup_keys_route)
		.ruma_route(client_server::add_backup_keys_for_room_route)
		.ruma_route(client_server::add_backup_keys_for_session_route)
		.ruma_route(client_server::delete_backup_keys_for_room_route)
		.ruma_route(client_server::delete_backup_keys_for_session_route)
		.ruma_route(client_server::delete_backup_keys_route)
		.ruma_route(client_server::get_backup_keys_for_room_route)
		.ruma_route(client_server::get_backup_keys_for_session_route)
		.ruma_route(client_server::get_backup_keys_route)
		.ruma_route(client_server::set_read_marker_route)
		.ruma_route(client_server::create_receipt_route)
		.ruma_route(client_server::create_typing_event_route)
		.ruma_route(client_server::create_room_route)
		.ruma_route(client_server::redact_event_route)
		.ruma_route(client_server::report_event_route)
		.ruma_route(client_server::create_alias_route)
		.ruma_route(client_server::delete_alias_route)
		.ruma_route(client_server::get_alias_route)
		.ruma_route(client_server::join_room_by_id_route)
		.ruma_route(client_server::join_room_by_id_or_alias_route)
		.ruma_route(client_server::joined_members_route)
		.ruma_route(client_server::leave_room_route)
		.ruma_route(client_server::forget_room_route)
		.ruma_route(client_server::joined_rooms_route)
		.ruma_route(client_server::kick_user_route)
		.ruma_route(client_server::ban_user_route)
		.ruma_route(client_server::unban_user_route)
		.ruma_route(client_server::invite_user_route)
		.ruma_route(client_server::set_room_visibility_route)
		.ruma_route(client_server::get_room_visibility_route)
		.ruma_route(client_server::get_public_rooms_route)
		.ruma_route(client_server::get_public_rooms_filtered_route)
		.ruma_route(client_server::search_users_route)
		.ruma_route(client_server::get_member_events_route)
		.ruma_route(client_server::get_protocols_route)
		.ruma_route(client_server::send_message_event_route)
		.ruma_route(client_server::send_state_event_for_key_route)
		.ruma_route(client_server::get_state_events_route)
		.ruma_route(client_server::get_state_events_for_key_route)
		// Ruma doesn't have support for multiple paths for a single endpoint yet, and these routes
		// share one Ruma request / response type pair with {get,send}_state_event_for_key_route
		.route(
			"/_matrix/client/r0/rooms/:room_id/state/:event_type",
			get(client_server::get_state_events_for_empty_key_route)
				.put(client_server::send_state_event_for_empty_key_route),
		)
		.route(
			"/_matrix/client/v3/rooms/:room_id/state/:event_type",
			get(client_server::get_state_events_for_empty_key_route)
				.put(client_server::send_state_event_for_empty_key_route),
		)
		// These two endpoints allow trailing slashes
		.route(
			"/_matrix/client/r0/rooms/:room_id/state/:event_type/",
			get(client_server::get_state_events_for_empty_key_route)
				.put(client_server::send_state_event_for_empty_key_route),
		)
		.route(
			"/_matrix/client/v3/rooms/:room_id/state/:event_type/",
			get(client_server::get_state_events_for_empty_key_route)
				.put(client_server::send_state_event_for_empty_key_route),
		)
		.ruma_route(client_server::sync_events_route)
		.ruma_route(client_server::sync_events_v4_route)
		.ruma_route(client_server::get_context_route)
		.ruma_route(client_server::get_message_events_route)
		.ruma_route(client_server::search_events_route)
		.ruma_route(client_server::turn_server_route)
		.ruma_route(client_server::send_event_to_device_route)
		.ruma_route(client_server::get_media_config_route)
		.ruma_route(client_server::get_media_preview_route)
		.ruma_route(client_server::create_content_route)
		// legacy v1 media routes
		.route(
			"/_matrix/media/v1/preview_url",
			get(client_server::get_media_preview_v1_route)
		)
		.route(
			"/_matrix/media/v1/config",
			get(client_server::get_media_config_v1_route)
		)
		.route(
			"/_matrix/media/v1/upload",
			post(client_server::create_content_v1_route)
		)
		.route(
			"/_matrix/media/v1/download/:server_name/:media_id",
			get(client_server::get_content_v1_route)
		)
		.route(
			"/_matrix/media/v1/download/:server_name/:media_id/:file_name",
			get(client_server::get_content_as_filename_v1_route)
		)
		.route(
			"/_matrix/media/v1/thumbnail/:server_name/:media_id",
			get(client_server::get_content_thumbnail_v1_route)
		)
		.ruma_route(client_server::get_content_route)
		.ruma_route(client_server::get_content_as_filename_route)
		.ruma_route(client_server::get_content_thumbnail_route)
		.ruma_route(client_server::get_devices_route)
		.ruma_route(client_server::get_device_route)
		.ruma_route(client_server::update_device_route)
		.ruma_route(client_server::delete_device_route)
		.ruma_route(client_server::delete_devices_route)
		.ruma_route(client_server::get_tags_route)
		.ruma_route(client_server::update_tag_route)
		.ruma_route(client_server::delete_tag_route)
		.ruma_route(client_server::upload_signing_keys_route)
		.ruma_route(client_server::upload_signatures_route)
		.ruma_route(client_server::get_key_changes_route)
		.ruma_route(client_server::get_pushers_route)
		.ruma_route(client_server::set_pushers_route)
		// .ruma_route(client_server::third_party_route)
		.ruma_route(client_server::upgrade_room_route)
		.ruma_route(client_server::get_threads_route)
		.ruma_route(client_server::get_relating_events_with_rel_type_and_event_type_route)
		.ruma_route(client_server::get_relating_events_with_rel_type_route)
		.ruma_route(client_server::get_relating_events_route)
		.ruma_route(client_server::get_hierarchy_route)
		.ruma_route(server_server::get_server_version_route)
		.route("/_matrix/key/v2/server", get(server_server::get_server_keys_route))
		.route(
			"/_matrix/key/v2/server/:key_id",
			get(server_server::get_server_keys_deprecated_route),
		)
		.ruma_route(server_server::get_public_rooms_route)
		.ruma_route(server_server::get_public_rooms_filtered_route)
		.ruma_route(server_server::send_transaction_message_route)
		.ruma_route(server_server::get_event_route)
		.ruma_route(server_server::get_backfill_route)
		.ruma_route(server_server::get_missing_events_route)
		.ruma_route(server_server::get_event_authorization_route)
		.ruma_route(server_server::get_room_state_route)
		.ruma_route(server_server::get_room_state_ids_route)
		.ruma_route(server_server::create_join_event_template_route)
		.ruma_route(server_server::create_join_event_v1_route)
		.ruma_route(server_server::create_join_event_v2_route)
		.ruma_route(server_server::create_invite_route)
		.ruma_route(server_server::get_devices_route)
		.ruma_route(server_server::get_room_information_route)
		.ruma_route(server_server::get_profile_information_route)
		.ruma_route(server_server::get_keys_route)
		.ruma_route(server_server::claim_keys_route)
        .ruma_route(server_server::get_hierarchy_route)
        .route("/_conduwuit/server_version", get(client_server::conduwuit_server_version))
		.route("/_matrix/client/r0/rooms/:room_id/initialSync", get(initial_sync))
		.route("/_matrix/client/v3/rooms/:room_id/initialSync", get(initial_sync))
		.route("/client/server.json", get(client_server::syncv3_client_server_json))
		.route("/.well-known/matrix/client", get(client_server::well_known_client_route))
		.route("/.well-known/matrix/server", get(server_server::well_known_server_route))
		.route("/", get(it_works))
		.fallback(not_found)
}

async fn not_found(uri: Uri) -> impl IntoResponse {
	if uri.path().contains("_matrix/") {
		warn!("Not found: {uri}");
	} else {
		info!("Not found: {uri}");
	}

	Error::BadRequest(ErrorKind::Unrecognized, "Unrecognized request")
}

async fn initial_sync(_uri: Uri) -> impl IntoResponse {
	Error::BadRequest(ErrorKind::GuestAccessForbidden, "Guest access not implemented")
}

async fn it_works() -> &'static str { "hewwo from conduwuit woof!" }

trait RouterExt {
	fn ruma_route<H, T>(self, handler: H) -> Self
	where
		H: RumaHandler<T>,
		T: 'static;
}

impl RouterExt for Router {
	fn ruma_route<H, T>(self, handler: H) -> Self
	where
		H: RumaHandler<T>,
		T: 'static,
	{
		handler.add_to_router(self)
	}
}

pub trait RumaHandler<T> {
	// Can't transform to a handler without boxing or relying on the nightly-only
	// impl-trait-in-traits feature. Moving a small amount of extra logic into the
	// trait allows bypassing both.
	fn add_to_router(self, router: Router) -> Router;
}

macro_rules! impl_ruma_handler {
    ( $($ty:ident),* $(,)? ) => {
        #[axum::async_trait]
        #[allow(non_snake_case)]
        impl<Req, E, F, Fut, $($ty,)*> RumaHandler<($($ty,)* Ruma<Req>,)> for F
        where
            Req: IncomingRequest + Send + 'static,
            F: FnOnce($($ty,)* Ruma<Req>) -> Fut + Clone + Send + 'static,
            Fut: Future<Output = Result<Req::OutgoingResponse, E>>
                + Send,
            E: IntoResponse,
            $( $ty: FromRequestParts<()> + Send + 'static, )*
        {
            fn add_to_router(self, mut router: Router) -> Router {
                let meta = Req::METADATA;
                let method_filter = method_to_filter(meta.method);

                for path in meta.history.all_paths() {
                    let handler = self.clone();

                    router = router.route(path, on(method_filter, |$( $ty: $ty, )* req| async move {
                        handler($($ty,)* req).await.map(RumaResponse)
                    }))
                }

                router
            }
        }
    };
}

impl_ruma_handler!();
impl_ruma_handler!(T1);
impl_ruma_handler!(T1, T2);
impl_ruma_handler!(T1, T2, T3);
impl_ruma_handler!(T1, T2, T3, T4);
impl_ruma_handler!(T1, T2, T3, T4, T5);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6, T7);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6, T7, T8);

fn method_to_filter(method: Method) -> MethodFilter {
	match method {
		Method::DELETE => MethodFilter::DELETE,
		Method::GET => MethodFilter::GET,
		Method::HEAD => MethodFilter::HEAD,
		Method::OPTIONS => MethodFilter::OPTIONS,
		Method::PATCH => MethodFilter::PATCH,
		Method::POST => MethodFilter::POST,
		Method::PUT => MethodFilter::PUT,
		Method::TRACE => MethodFilter::TRACE,
		m => panic!("Unsupported HTTP method: {m:?}"),
	}
}
