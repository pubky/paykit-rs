//! The executor grant (Paykit spec 8.3; Paykit wire 2, 3.3).
//!
//! An executor app's signed grant, stored in the Allowance record, names the app that pays
//! automatically under the Allowance. It is signed under Paykit's own label
//! `paykit/v1/executor-grant` with the Switch's algorithm (Switch wire 2.1), over the
//! identity, the counterparty, the Allowance, the terms hash, `confirmed_generation`, and the
//! Allowance record version the grant's write builds on.
//!
//! Security: the signed version ties a grant to its place in the grant history, so an own app
//! with `paykit_allowance` can't move a grant to make a ledger write look like another executor's,
//! and a grant can't be moved to another counterparty or Allowance. Changing the terms voids
//! it.
//!
//! The ledger handoff (Paykit spec 8.3, "Handoff"; Paykit wire 5.4) is the previous
//! executor's word of the last ledger write it wrote or accepted, signed under
//! `paykit/v1/ledger-handoff` over the identity, the counterparty, the Allowance, the grant it
//! answers with the version that grant's write built on, and the ledger version and item
//! hash it names, so no other app can name a ledger write for a new executor to build on.

use pubky_switch_core::enc::{b64, u64be, zbase32_encode};
use pubky_switch_core::keys::AppKeys;
use pubky_switch_core::ops::RecordVersion;
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sig::{self, SignedDoc};
use serde_json::{Map, Value};

use crate::error::{invalid, Result};
use crate::identifiers::AllowanceId;
use crate::json::Obj;
use crate::scopes::{self, PaykitScope};

/// The executor grant's signature label (Paykit wire 2).
pub const EXECUTOR_GRANT_LABEL: &str = "paykit/v1/executor-grant";

const GRANT: &str = "Paykit wire 3.3: executor grant";

/// The fields an executor grant signs after its label (Paykit wire 3.3): identity,
/// counterparty identity, `allowance_id` text, terms hash, `u64(confirmed_generation)`,
/// `u64(generation)` and `u64(seq)` of the Allowance record version its write builds on.
pub fn grant_fields(
    identity: &[u8; 32],
    counterparty: &[u8; 32],
    allowance_id: &AllowanceId,
    terms_hash: &[u8; 32],
    confirmed_generation: u64,
    built_on: RecordVersion,
) -> Vec<Vec<u8>> {
    vec![
        identity.to_vec(),
        counterparty.to_vec(),
        allowance_id.as_str().as_bytes().to_vec(),
        terms_hash.to_vec(),
        u64be(confirmed_generation).to_vec(),
        u64be(built_on.generation).to_vec(),
        u64be(built_on.version).to_vec(),
    ]
}

/// An executor grant (Paykit wire 3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorGrant {
    /// The executor app.
    pub app_id: [u8; 16],
    /// The Allowance's terms hash.
    pub terms_hash: [u8; 32],
    /// The generation in which the executor's user last confirmed the Allowance.
    pub confirmed_generation: u64,
    /// The executor's certificate, holding `paykit_allowance_execute`.
    pub cert: SignedDoc,
    /// The signature.
    pub sig: [u8; 64],
    /// Where it was read, for the members its writer doesn't know (Paykit wire 5); none for
    /// a grant signed or built in code.
    pub origin: crate::records::Origin,
}

impl ExecutorGrant {
    /// Sign a grant with the executor app's signing key (Paykit spec 8.3, Paykit wire 3.3).
    /// `built_on` is the version of the Allowance record the write adding the grant reads: a
    /// grant goes into an Allowance record that holds its proposal already (Paykit wire 5.4).
    ///
    /// The caller keeps its signing key; storing it is the Switch's rule (Switch spec 3.1).
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        app: &AppKeys,
        cert: SignedDoc,
        identity: &[u8; 32],
        counterparty: &[u8; 32],
        allowance_id: &AllowanceId,
        terms_hash: [u8; 32],
        confirmed_generation: u64,
        built_on: RecordVersion,
    ) -> Self {
        let fields = grant_fields(
            identity,
            counterparty,
            allowance_id,
            &terms_hash,
            confirmed_generation,
            built_on,
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        Self {
            app_id: app.app_id_bytes(),
            terms_hash,
            confirmed_generation,
            cert,
            sig: app.sign(EXECUTOR_GRANT_LABEL, &refs),
            origin: crate::records::Origin::default(),
        }
    }

    /// Read a grant (Paykit wire 3.3).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] naming the member.
    pub fn from_json(value: &Value) -> Result<Self> {
        let obj = Obj::of(value, GRANT, "grant")?;
        let cert = obj.req_obj("cert")?;
        Ok(Self {
            app_id: obj.req_app_id("app_id")?,
            terms_hash: obj.req_b64::<32>("terms_hash")?,
            confirmed_generation: obj.req_int("confirmed_generation")?,
            cert: SignedDoc {
                body: cert.req_str("body")?.to_owned(),
                sig: cert.req_str("sig")?.to_owned(),
            },
            sig: obj.req_b64::<64>("sig")?,
            origin: crate::records::Origin::default(),
        })
    }

    /// Write the grant.
    pub fn to_json(&self) -> Value {
        let mut cert = Map::new();
        cert.insert("body".into(), Value::String(self.cert.body.clone()));
        cert.insert("sig".into(), Value::String(self.cert.sig.clone()));
        let mut map = Map::new();
        map.insert("app_id".into(), Value::String(zbase32_encode(&self.app_id)));
        map.insert("cert".into(), Value::Object(cert));
        map.insert(
            "confirmed_generation".into(),
            Value::from(self.confirmed_generation),
        );
        map.insert("sig".into(), Value::String(b64(&self.sig)));
        map.insert("terms_hash".into(), Value::String(b64(&self.terms_hash)));
        Value::Object(map)
    }

    /// Verify the grant (Paykit wire 3.3): `cert` verifies as a Paykit certificate of the own
    /// `identity`, names `app_id` and holds `paykit_allowance_execute`; `terms_hash` is
    /// `expected_terms`; and the signature verifies with the certificate's signing key for
    /// this identity, counterparty and Allowance at `built_on`, the version its grant history
    /// entry names. Returns the executor's certificate.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Switch`] for a certificate or signature the Switch's checks refuse,
    /// [`crate::Error::Invalid`] for Paykit's rules.
    pub fn verify(
        &self,
        identity: &[u8; 32],
        counterparty: &[u8; 32],
        allowance_id: &AllowanceId,
        expected_terms: &[u8; 32],
        built_on: RecordVersion,
    ) -> Result<AppCert> {
        let cert = scopes::verify_certificate(&self.cert, identity)?;
        if cert.app_id() != self.app_id {
            return invalid("Paykit wire 3.3: the grant's certificate names another app");
        }
        if !scopes::holds(cert.scopes(), &PaykitScope::AllowanceExecute) {
            return invalid(
                "Paykit wire 3.3: the grant's certificate lacks paykit_allowance_execute",
            );
        }
        if &self.terms_hash != expected_terms {
            return invalid("Paykit wire 3.3: the grant names other terms");
        }
        let fields = grant_fields(
            identity,
            counterparty,
            allowance_id,
            &self.terms_hash,
            self.confirmed_generation,
            built_on,
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        sig::verify(&cert.signing_key(), EXECUTOR_GRANT_LABEL, &refs, &self.sig)?;
        Ok(cert)
    }
}

/// The ledger handoff's signature label (Paykit wire 2).
pub const LEDGER_HANDOFF_LABEL: &str = "paykit/v1/ledger-handoff";

const HANDOFF: &str = "Paykit wire 5.4: ledger_handoff";

/// The fields a ledger handoff signs after its label (Paykit wire 2, 5.4): identity,
/// counterparty identity, `allowance_id` text, `u64(generation)` and `u64(version)` of the
/// grant history entry that handed the Allowance on, `u64(generation)` and `u64(version)` of
/// the previous executor's last ledger version, and that write's item hash.
pub fn handoff_fields(
    identity: &[u8; 32],
    counterparty: &[u8; 32],
    allowance_id: &AllowanceId,
    grant: RecordVersion,
    ledger: RecordVersion,
    item_hash: &[u8; 32],
) -> Vec<Vec<u8>> {
    vec![
        identity.to_vec(),
        counterparty.to_vec(),
        allowance_id.as_str().as_bytes().to_vec(),
        u64be(grant.generation).to_vec(),
        u64be(grant.version).to_vec(),
        u64be(ledger.generation).to_vec(),
        u64be(ledger.version).to_vec(),
        item_hash.to_vec(),
    ]
}

/// A ledger handoff (Paykit spec 8.3, "Handoff"; Paykit wire 5.4 `ledger_handoff`): the
/// previous executor's last ledger version, signed with its certificate, for the grant that
/// handed the Allowance on. Later writers of the record carry it along as read, so its own
/// signature, not the write's, says who wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerHandoff {
    /// The previous executor: the app of the grant history entry just before `grant`.
    pub app_id: [u8; 16],
    /// The version of the grant history entry that handed the Allowance on.
    pub grant: RecordVersion,
    /// The version of the previous executor's last ledger version.
    pub ledger: RecordVersion,
    /// That ledger write's item hash (Switch wire 6.6).
    pub item_hash: [u8; 32],
    /// The previous executor's certificate, holding `paykit_allowance_execute`.
    pub cert: SignedDoc,
    /// The signature.
    pub sig: [u8; 64],
    /// Where it was read, for the members its writer doesn't know (Paykit wire 5); none for
    /// one signed or built in code.
    pub origin: crate::records::Origin,
}

impl LedgerHandoff {
    /// Sign a ledger handoff with the previous executor's signing key (Paykit wire 5.4): for
    /// the grant history entry at version `grant`, naming the ledger write at `ledger` with
    /// item hash `item_hash`.
    ///
    /// The caller keeps its signing key; storing it is the Switch's rule (Switch spec 3.1).
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        app: &AppKeys,
        cert: SignedDoc,
        identity: &[u8; 32],
        counterparty: &[u8; 32],
        allowance_id: &AllowanceId,
        grant: RecordVersion,
        ledger: RecordVersion,
        item_hash: [u8; 32],
    ) -> Self {
        let fields = handoff_fields(
            identity,
            counterparty,
            allowance_id,
            grant,
            ledger,
            &item_hash,
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        Self {
            app_id: app.app_id_bytes(),
            grant,
            ledger,
            item_hash,
            cert,
            sig: app.sign(LEDGER_HANDOFF_LABEL, &refs),
            origin: crate::records::Origin::default(),
        }
    }

    /// Read a ledger handoff (Paykit wire 5.4): every member, and a ledger version a write
    /// gives (`generation` and `version` at least 1).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] naming the member or the rule.
    pub fn from_json(value: &Value) -> Result<Self> {
        let obj = Obj::of(value, HANDOFF, "ledger_handoff")?;
        let version = |member: &str| -> Result<RecordVersion> {
            let at = obj.req_obj(member)?;
            Ok(RecordVersion {
                generation: at.req_int("generation")?,
                version: at.req_int("version")?,
            })
        };
        let cert = obj.req_obj("cert")?;
        let handoff = Self {
            app_id: obj.req_app_id("app_id")?,
            grant: version("grant")?,
            ledger: version("ledger")?,
            item_hash: obj.req_b64::<32>("item_hash")?,
            cert: SignedDoc {
                body: cert.req_str("body")?.to_owned(),
                sig: cert.req_str("sig")?.to_owned(),
            },
            sig: obj.req_b64::<64>("sig")?,
            origin: crate::records::Origin::default(),
        };
        handoff.check()?;
        Ok(handoff)
    }

    /// The rule of its shape, the same for the reader and the writer: the ledger version
    /// named is one a write gives.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] naming the rule.
    pub(crate) fn check(&self) -> Result<()> {
        if self.ledger.generation == 0 || self.ledger.version == 0 {
            return invalid("Paykit wire 5.4: ledger_handoff names a ledger version a write gives");
        }
        Ok(())
    }

    /// Write the ledger handoff.
    pub fn to_json(&self) -> Value {
        let version = |at: RecordVersion| {
            let mut map = Map::new();
            map.insert("generation".into(), Value::from(at.generation));
            map.insert("version".into(), Value::from(at.version));
            Value::Object(map)
        };
        let mut cert = Map::new();
        cert.insert("body".into(), Value::String(self.cert.body.clone()));
        cert.insert("sig".into(), Value::String(self.cert.sig.clone()));
        let mut map = Map::new();
        map.insert("app_id".into(), Value::String(zbase32_encode(&self.app_id)));
        map.insert("cert".into(), Value::Object(cert));
        map.insert("grant".into(), version(self.grant));
        map.insert("item_hash".into(), Value::String(b64(&self.item_hash)));
        map.insert("ledger".into(), version(self.ledger));
        map.insert("sig".into(), Value::String(b64(&self.sig)));
        Value::Object(map)
    }

    /// Verify the ledger handoff's signature (Paykit wire 5.4): `cert` verifies as a Paykit
    /// certificate of the own `identity`, names `app_id` and holds
    /// `paykit_allowance_execute`, and the signature verifies with the certificate's signing
    /// key for this identity, counterparty and Allowance. Where it sits in the grant history
    /// is the record's check ([`crate::records::AllowanceRecord::ledger_handoff_counts`]).
    /// Returns the previous executor's certificate.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Switch`] for a certificate or signature the Switch's checks refuse,
    /// [`crate::Error::Invalid`] for Paykit's rules.
    pub fn verify(
        &self,
        identity: &[u8; 32],
        counterparty: &[u8; 32],
        allowance_id: &AllowanceId,
    ) -> Result<AppCert> {
        self.check()?;
        let cert = scopes::verify_certificate(&self.cert, identity)?;
        if cert.app_id() != self.app_id {
            return invalid("Paykit wire 5.4: ledger_handoff's certificate names another app");
        }
        if !scopes::holds(cert.scopes(), &PaykitScope::AllowanceExecute) {
            return invalid(
                "Paykit wire 5.4: ledger_handoff's certificate lacks paykit_allowance_execute",
            );
        }
        let fields = handoff_fields(
            identity,
            counterparty,
            allowance_id,
            self.grant,
            self.ledger,
            &self.item_hash,
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        sig::verify(&cert.signing_key(), LEDGER_HANDOFF_LABEL, &refs, &self.sig)?;
        Ok(cert)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff_json(generation: u64, version: u64) -> Value {
        serde_json::json!({
            "app_id": zbase32_encode(&[6; 16]),
            "grant": { "generation": 1, "version": 4 },
            "ledger": { "generation": generation, "version": version },
            "item_hash": b64(&[4; 32]),
            "cert": { "body": "b", "sig": "s" },
            "sig": b64(&[5; 64]),
        })
    }

    #[test]
    fn test_ledger_handoff_shape_round_trips() {
        let read = LedgerHandoff::from_json(&handoff_json(1, 3)).unwrap();
        assert_eq!(read.app_id, [6; 16]);
        assert_eq!(read.ledger.version, 3);
        assert_eq!(read.item_hash, [4; 32]);
        assert_eq!(LedgerHandoff::from_json(&read.to_json()).unwrap(), read);
    }

    #[test]
    fn test_ledger_handoff_shape_refuses_a_version_no_write_gives_and_a_missing_member() {
        assert!(LedgerHandoff::from_json(&handoff_json(0, 3)).is_err());
        assert!(LedgerHandoff::from_json(&handoff_json(1, 0)).is_err());
        let mut short = handoff_json(1, 3);
        short["item_hash"] = Value::String(b64(&[4; 31]));
        assert!(LedgerHandoff::from_json(&short).is_err());
        for member in ["app_id", "grant", "ledger", "item_hash", "cert", "sig"] {
            let mut missing = handoff_json(1, 3);
            missing.as_object_mut().unwrap().remove(member);
            assert!(LedgerHandoff::from_json(&missing).is_err(), "{member}");
        }
    }

    #[test]
    fn test_ledger_handoff_fields_layout() {
        let at = |version| RecordVersion {
            generation: 1,
            version,
        };
        let fields = handoff_fields(
            &[1; 32],
            &[2; 32],
            &AllowanceId::parse("d0efffb7-191b-4709-b6c7-e343af5500b4").unwrap(),
            at(4),
            at(9),
            &[3; 32],
        );
        assert_eq!(fields.len(), 8);
        assert_eq!(fields[4], 4u64.to_be_bytes());
        assert_eq!(fields[6], 9u64.to_be_bytes());
        assert_eq!(fields[7], [3; 32]);
    }

    #[test]
    fn test_ledger_handoff_label_is_paykits_own() {
        assert!(LEDGER_HANDOFF_LABEL.starts_with("paykit/v1/"));
        assert_ne!(LEDGER_HANDOFF_LABEL, EXECUTOR_GRANT_LABEL);
    }

    #[test]
    fn test_grant_label_is_paykits_own() {
        assert!(EXECUTOR_GRANT_LABEL.starts_with("paykit/v1/"));
    }

    #[test]
    fn test_grant_fields_layout() {
        let fields = grant_fields(
            &[1; 32],
            &[2; 32],
            &AllowanceId::parse("d0efffb7-191b-4709-b6c7-e343af5500b4").unwrap(),
            &[3; 32],
            1,
            RecordVersion {
                generation: 1,
                version: 12,
            },
        );
        assert_eq!(fields.len(), 7);
        assert_eq!(fields[6], 12u64.to_be_bytes());
    }

    #[test]
    fn test_grant_from_json_requires_members() {
        assert!(ExecutorGrant::from_json(&serde_json::json!({ "app_id": "x" })).is_err());
    }
}
