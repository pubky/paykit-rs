//! The original app's signed report of a recovery upload (Paykit wire 5.10).

mod evidence;
pub use evidence::{
    RecoveryDecision, RecoveryItem, RecoveryState, RecoverySubject, RecoveryWitness,
};

use pubky_switch_core::{
    enc::{lp, unb64_array},
    json::{parse_json_as, Doc},
    records::IdentityDoc,
    sig::{self, SignedDoc},
};
use serde::Deserialize;

use crate::error::{ensure, Error, Result};
use crate::scopes::{certificate_in_force_at_write, verify_certificate};

/// The signature domain for an original app's recovery report.
pub const RECOVERY_UPLOAD_LABEL: &str = "paykit/v1/recovery-upload";

/// The historical commitments that one original app signed for a loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCommitments {
    /// The original app's inventory cutoff.
    pub cutoff: [u8; 32],
    /// The inventory at that cutoff.
    pub inventory: [u8; 32],
    /// The dispositions the original app recorded for the inventory.
    pub dispositions: [u8; 32],
}

#[derive(Deserialize)]
struct Report {
    v: u64,
    cutoff: String,
    inventory: String,
    dispositions: String,
    signature: String,
    author_certificate: SignedDoc,
}

impl RecoveryCommitments {
    /// The field signed under [`RECOVERY_UPLOAD_LABEL`] for the original opening member.
    ///
    /// # Errors
    ///
    /// The document names no provider loss or does not list the original.
    pub fn signing_field(&self, opening: &IdentityDoc, original: &[u8; 16]) -> Result<Vec<u8>> {
        let lost = opening.body().recovery_from.ok_or_else(|| {
            Error::Invalid("Paykit wire 5.10: the identity document names no provider loss".into())
        })?;
        ensure(
            opening.listed_cert(original).is_some(),
            "Paykit wire 5.10: the opening does not list the original app",
        )?;
        Ok(lp(&[
            &1u64.to_be_bytes(),
            &opening.identity(),
            original,
            &lost.to_be_bytes(),
            &opening.generation().to_be_bytes(),
            &opening.inbox().service_key,
            &opening.hash(),
            &self.cutoff,
            &self.inventory,
            &self.dispositions,
        ]))
    }

    /// Verify the original signature in the decrypted value, at the authenticated publication
    /// time. The reader checks the current application and the publisher's authority separately.
    ///
    /// # Errors
    ///
    /// The report is malformed or its original app, opening, certificate or signature fails verification.
    pub fn verify(
        value: &[u8],
        opening: &IdentityDoc,
        identity: &IdentityDoc,
        original: &[u8; 16],
        written: u64,
    ) -> Result<Self> {
        let report: Report = parse_json_as(value, Doc::Plain)?;
        ensure(
            report.v == 1,
            "Paykit wire 5.10: unsupported recovery report version",
        )?;
        let cert = verify_certificate(&report.author_certificate, &opening.identity())?;
        ensure(
            identity.identity() == opening.identity()
                && cert.app_id() == *original
                && certificate_in_force_at_write(identity, &cert, written),
            "Paykit wire 5.10: invalid original app certificate",
        )?;
        let commitments = Self {
            cutoff: unb64_array(&report.cutoff)?,
            inventory: unb64_array(&report.inventory)?,
            dispositions: unb64_array(&report.dispositions)?,
        };
        let field = commitments.signing_field(opening, original)?;
        sig::verify(
            &cert.signing_key(),
            RECOVERY_UPLOAD_LABEL,
            &[&field],
            &unb64_array(&report.signature)?,
        )?;
        Ok(commitments)
    }
}
