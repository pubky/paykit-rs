use paykit_lib::{serialize_allowance_event, AllowanceEvent, PrivateApplicationMessage};

/// Wrap a typed Allowance event as the private message its intake would see.
pub(crate) fn allowance_application_message(event: &AllowanceEvent) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some(event.kind().as_str().to_owned()),
        raw_json: serialize_allowance_event(event).unwrap(),
    }
}

pub(crate) const ALLOWANCE_EVENT_FIXTURES: [(&str, &str); 4] = [
    (
        "paykit.allowance_proposal",
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
    ),
    (
        "paykit.allowance_acceptance",
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202",
    ),
    (
        "paykit.allowance_rejection",
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204",
    ),
    (
        "paykit.allowance_end",
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203",
    ),
];

pub(crate) fn allowance_event_json(kind: &str, event_id: &str) -> String {
    let common = format!(
        r#""version":1,"kind":"{kind}","event_id":"{event_id}","allowance_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44""#
    );
    match kind {
        "paykit.allowance_proposal" => format!(
            r#"{{{common},"proposer_role":"allower","terms":{{"asset":"btc","per_payment_amount":{{"minimum":"1","maximum":"2"}},"period_limits":[],"lifetime_amount_limit":null,"active_from":null,"expires_at":null,"allowed_payment_endpoint_identifiers":null}}}}"#
        ),
        "paykit.allowance_acceptance" | "paykit.allowance_rejection" => {
            format!(r#"{{{common},"proposal_event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201"}}"#)
        }
        "paykit.allowance_end" => format!(
            r#"{{{common},"proposal_event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201","acceptance_event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202"}}"#
        ),
        _ => unreachable!("test fixture uses only Allowance event kinds"),
    }
}

/// Recognized Allowance acceptance missing its required `proposal_event_id`.
///
/// Its Event ID does not collide with [`ALLOWANCE_EVENT_FIXTURES`].
pub(crate) fn malformed_allowance_event_json() -> String {
    r#"{"version":1,"kind":"paykit.allowance_acceptance","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d205","allowance_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44"}"#
        .into()
}
