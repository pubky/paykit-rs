//! Signing, sealing and envelopes with fresh keys: what one side makes, the other side
//! accepts, and any other key, label, record or byte is refused.

use paykit_lib::grant::{EXECUTOR_GRANT_LABEL, LEDGER_HANDOFF_LABEL};
use pubky_switch_core::envelope::{open_framed, seal_framed, Recipient};
use pubky_switch_core::keys::{AppKeys, IdentityKey, ReceiveKey};
use pubky_switch_core::sealed::{RecordValue, SealedValue};
use pubky_switch_core::sig::verify;

/// A small strict JSON document (Switch wire 1.1).
const JSON: &[u8] = br#"{"note":"from a browser"}"#;

conformance_test! {
    /// An app's signature over Paykit's fields verifies under its key and label, and no
    /// other key, label or field (Switch wire 2.1, Paykit wire 5.4).
    fn test_signature_round_trip_verifies() {
        let (app, other) = (AppKeys::generate().unwrap(), AppKeys::generate().unwrap());
        let fields: [&[u8]; 3] = [b"identity", b"counterparty", &7u64.to_be_bytes()];
        let signature = app.sign(EXECUTOR_GRANT_LABEL, &fields);
        let public = app.signing_public();
        verify(&public, EXECUTOR_GRANT_LABEL, &fields, &signature).unwrap();

        assert!(verify(&other.signing_public(), EXECUTOR_GRANT_LABEL, &fields, &signature).is_err());
        assert!(verify(&public, LEDGER_HANDOFF_LABEL, &fields, &signature).is_err());
        let changed: [&[u8]; 3] = [b"identity", b"counterparty", &8u64.to_be_bytes()];
        assert!(verify(&public, EXECUTOR_GRANT_LABEL, &changed, &signature).is_err());

        let identity = IdentityKey::generate().unwrap();
        let signature = identity.sign(EXECUTOR_GRANT_LABEL, &fields);
        verify(&identity.public(), EXECUTOR_GRANT_LABEL, &fields, &signature).unwrap();
        assert!(verify(&public, EXECUTOR_GRANT_LABEL, &fields, &signature).is_err());
    }
}

conformance_test! {
    /// A record value sealed under a scope key opens to its JSON for the same identity and
    /// record, under a fresh nonce each time; another key, record or identity, or a changed
    /// byte, doesn't open it (Switch wire 6.2).
    fn test_sealed_value_round_trip_opens() {
        let identity = IdentityKey::generate().unwrap();
        let owner = identity.public();
        let scope = "paykit_pay";
        let key = identity.scope_key(1, scope);
        let record = [7u8; 32];

        let sealed = SealedValue::seal(&key, &owner, 1, scope, &record, JSON).unwrap();
        assert_eq!(sealed.open(&key, &owner, &record).unwrap(), RecordValue::Json(JSON.to_vec()));
        let again = SealedValue::seal(&key, &owner, 1, scope, &record, JSON).unwrap();
        assert_ne!(sealed.nonce, again.nonce);
        assert_ne!(sealed.ct, again.ct);

        assert!(sealed.open(&identity.scope_key(2, scope), &owner, &record).is_err());
        assert!(sealed.open(&key, &owner, &[8u8; 32]).is_err());
        assert!(sealed.open(&key, &[9u8; 32], &record).is_err());
        let mut changed = sealed.clone();
        changed.ct[0] ^= 1;
        assert!(changed.open(&key, &owner, &record).is_err());

        let delete = SealedValue::delete(&key, &owner, 1, scope, &record).unwrap();
        assert!(delete.looks_like_delete());
        assert_eq!(delete.open(&key, &owner, &record).unwrap(), RecordValue::Delete);
    }
}

conformance_test! {
    /// A framed document sealed in an envelope to a receive key opens with that key's secret
    /// for the same identity, and not with another key or identity (Switch wire 4.2, 4.3).
    fn test_envelope_round_trip_opens() {
        let identity = IdentityKey::generate().unwrap().public();
        let (key, other) = (ReceiveKey::generate(1).unwrap(), ReceiveKey::generate(2).unwrap());
        let recipient = Recipient::new(key.public);

        let envelope = seal_framed(&identity, &recipient, JSON).unwrap();
        assert_eq!(open_framed(&envelope, &identity, &key.secret()).unwrap().as_slice(), JSON);
        assert_ne!(seal_framed(&identity, &recipient, JSON).unwrap(), envelope);

        assert!(open_framed(&envelope, &identity, &other.secret()).is_err());
        assert!(open_framed(&envelope, &[3u8; 32], &key.secret()).is_err());
        let mut changed = envelope.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(open_framed(&changed, &identity, &key.secret()).is_err());
    }
}
