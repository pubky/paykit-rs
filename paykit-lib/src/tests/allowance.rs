use super::*;
use crate::allowance::test_fixtures::{allowance_id, event_id, minimal_proposal};

#[tokio::test]
async fn test_allowance_send_helpers_preserve_fifo_event_order() {
    let mut setup = PrivateTestSetup::new().await;
    let proposal = minimal_proposal();
    let proposal_event_id = proposal.event_id().clone();
    let acceptance_event_id = event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202");
    let acceptance = AllowanceAcceptance::new(
        acceptance_event_id.clone(),
        allowance_id(),
        proposal_event_id.clone(),
    )
    .unwrap();
    let rejection = AllowanceRejection::new(
        event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203"),
        allowance_id(),
        proposal_event_id.clone(),
    )
    .unwrap();
    let end = AllowanceEnd::accepted(
        event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204"),
        allowance_id(),
        proposal_event_id,
        acceptance_event_id,
    )
    .unwrap();

    send_allowance_proposal(&mut setup.sender_link, &proposal)
        .await
        .unwrap();
    send_allowance_acceptance(&mut setup.sender_link, &acceptance)
        .await
        .unwrap();
    send_allowance_rejection(&mut setup.sender_link, &rejection)
        .await
        .unwrap();
    send_allowance_end(&mut setup.sender_link, &end)
        .await
        .unwrap();

    let received = setup
        .receiver_link
        .receive_private_application_messages()
        .await
        .unwrap();
    let events = received
        .iter()
        .filter_map(parse_allowance_event_message)
        .collect::<Vec<_>>();

    assert_eq!(events.len(), 4);
    assert_eq!(
        events
            .iter()
            .map(AllowanceEventMessage::kind)
            .collect::<Vec<_>>(),
        vec![
            PrivateMessageKind::AllowanceProposal,
            PrivateMessageKind::AllowanceAcceptance,
            PrivateMessageKind::AllowanceRejection,
            PrivateMessageKind::AllowanceEnd,
        ]
    );
    assert_eq!(
        events[0].parsed_event(),
        Some(&AllowanceEvent::Proposal(proposal))
    );
    assert_eq!(
        events[1].parsed_event(),
        Some(&AllowanceEvent::Acceptance(acceptance))
    );
    assert_eq!(
        events[2].parsed_event(),
        Some(&AllowanceEvent::Rejection(rejection))
    );
    assert_eq!(events[3].parsed_event(), Some(&AllowanceEvent::End(end)));

    close_encrypted_link(setup.receiver_link).await.unwrap();
    close_encrypted_link(setup.sender_link).await.unwrap();
    setup.sender_session.signout().await.unwrap();
    setup.receiver_session.signout().await.unwrap();
}
