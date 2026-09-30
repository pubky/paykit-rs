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
