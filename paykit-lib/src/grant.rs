//! The executor grant (Paykit spec 8.3; Paykit wire 2, 3.3).
//!
//! An executor app's signed grant, stored in the Allowance record, names the app that pays
//! automatically under the Allowance. It is signed under Paykit's own label
//! `paykit/v1/executor-grant` with the Switch's algorithm (Switch wire 2.1), over the
//! identity, the counterparty, the Allowance, the terms hash, `confirmed_generation`, and the
//! Allowance record version the grant's write builds on.
//!
//! Security: the signed version ties a grant to its place in the grant history, so an own app
//! with `allowance` can't move a grant to make a ledger write look like another executor's,
//! and a grant can't be moved to another counterparty or Allowance. Changing the terms voids
//! it.

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
    /// The executor's certificate, holding `allowance_execute`.
    pub cert: SignedDoc,
    /// The signature.
    pub sig: [u8; 64],
}

impl ExecutorGrant {
    /// Sign a grant with the executor app's signing key (Paykit spec 8.3, Paykit wire 3.3).
    /// `built_on` is the version of the Allowance record the write adding the grant reads,
    /// `{0, 0}` for a new record.
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
    /// `identity`, names `app_id` and holds `allowance_execute`; `terms_hash` is
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
            return invalid("Paykit wire 3.3: the grant's certificate lacks allowance_execute");
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

#[cfg(test)]
mod tests {
    use super::*;

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
