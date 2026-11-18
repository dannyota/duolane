use crate::request_identity::ConversationIdentity;

use super::continuation::ContinuationReservation;

pub(super) fn agent_owner(session_id: &str, agent_id: &str) -> ConversationIdentity {
    ConversationIdentity::Agent(session_id.to_string(), agent_id.to_string())
}

pub(super) fn test_continuation(
    owner: Option<ConversationIdentity>,
    turn_id: Option<u64>,
    previous_response_id: Option<&str>,
    origin_socket_id: Option<u64>,
) -> ContinuationReservation {
    ContinuationReservation::new(
        super::continuation::ContinuationCandidate {
            turn_id,
            previous_response_id: previous_response_id.map(str::to_string),
            input_delta: Some(vec![]),
            input_delta_count: 0,
            disabled_reason: None,
        },
        owner,
        origin_socket_id,
    )
}

pub(super) fn test_websocket_client() -> reqwest::Client {
    crate::tls::client_builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}
