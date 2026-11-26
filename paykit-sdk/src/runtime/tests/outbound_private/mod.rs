mod discovery;
mod enqueue;
mod processing;
mod reservations;

#[test]
fn test_invalid_prepared_packet_does_not_retry_as_transport() {
    use crate::runtime::outbound_private::private_send_error_requires_recovery;
    use paykit_lib::PaykitError;

    assert!(private_send_error_requires_recovery(
        &PaykitError::InvalidData {
            context: "prepared private send ciphertext has an invalid length".into(),
            source: None,
        }
    ));
    assert!(!private_send_error_requires_recovery(
        &PaykitError::Transport {
            context: "publication unavailable".into(),
            source: anyhow::anyhow!("connection closed"),
        }
    ));
}

#[test]
fn test_write_slot_check_frees_only_the_first_matching_preparation() {
    use crate::runtime::outbound_private::WriteSlotCheck;

    let free = "/pub/paykit/v0/private/link/7";
    let mut check = WriteSlotCheck::Free(free.into());
    assert!(check.take_free_slot(free));
    // Later preparations in the run check their own slot.
    assert!(!check.take_free_slot(free));

    let mut check = WriteSlotCheck::Free(free.into());
    assert!(!check.take_free_slot("/pub/paykit/v0/private/link/8"));
    assert!(!check.take_free_slot(free));

    // A prepared send restored from storage leaves the skip for a later send.
    let mut check = WriteSlotCheck::AfterStoredSend;
    assert!(!check.take_free_slot(free));
    assert!(matches!(check, WriteSlotCheck::AfterStoredSend));
}
