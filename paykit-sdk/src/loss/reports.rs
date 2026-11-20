//! What the wallet can verify about the original participants in a provider loss.

use paykit_lib::{
    recovery::RecoveryCommitments,
    scopes::{certificate_in_force_at_write, verify_certificate, RECOVERY},
};
use pubky_switch_client::app::{RecordApplication, RecordTarget};
use pubky_switch_core::{enc::lp, records::IdentityDoc, shared::ApplicationRef};

use super::{LossBoundary, Paykit, Refusal, Result};

/// The App IDs on the identity-signed opening. An explicit signed removal
/// resolves membership for this loss. It carries no payment or credit outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryMembers {
    /// The loss covered by this opening.
    pub loss: LossBoundary,
    /// The hash of the identity document that opened the recovered generation.
    pub opening: [u8; 32],
    /// Original apps for which the opening has no verified removal.
    pub originals: Vec<[u8; 16]>,
    /// Current reports whose original signature and publishing app were verified.
    /// A removed original can have a verified earlier report. The application
    /// identifies the exact write. Its commitments can still
    /// describe unresolved work.
    pub reports: Vec<RecoveryReport>,
    /// Original apps explicitly removed by a verified successor of this opening.
    pub removed: Vec<[u8; 16]>,
}

/// An original app's historical commitments, delivered by an authorized publisher.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// The opening member that signed the report.
    pub original: [u8; 16],
    /// The recovery-authorized app that published the report.
    pub publisher: [u8; 16],
    /// The exact accepted write at the service.
    pub application: ApplicationRef,
    /// The original app's signed inventory cutoff.
    pub cutoff: [u8; 32],
    /// Commitment to the inventory at the cutoff.
    pub inventory: [u8; 32],
    /// Commitment to the dispositions of the items in that inventory.
    pub dispositions: [u8; 32],
}

impl RecoveryReport {
    /// Whether the disclosed inputs reproduce the three commitments this original signed.
    /// The caller checks the authority and meaning of the evidence for the item it concerns.
    ///
    /// # Errors
    ///
    /// The evidence does not form a valid disclosure under Paykit wire 5.10.
    pub fn matches_evidence(
        &self,
        anchors: &[paykit_lib::recovery::RecoveryWitness<'_>],
        items: &[paykit_lib::recovery::RecoveryItem<'_>],
        decisions: &[paykit_lib::recovery::RecoveryDecision<'_>],
    ) -> Result<bool> {
        let actual = RecoveryCommitments::from_evidence(anchors, items, decisions)?;
        Ok(actual.cutoff == self.cutoff
            && actual.inventory == self.inventory
            && actual.dispositions == self.dispositions)
    }
}

impl Paykit {
    async fn original_report(
        &mut self,
        opening: &IdentityDoc,
        original: &[u8; 16],
    ) -> Result<Option<RecoveryReport>> {
        let family = crate::activity::family(paykit_lib::collections::Collection::RecoveryUploads);
        let target = RecordTarget::new(family, &lp(&[original, &opening.hash()]));
        let Some(RecordApplication {
            record,
            application: applied,
            evidence: operation,
            ..
        }) = self.app.read_record_application(&target).await?
        else {
            return Ok(None);
        };
        let Some(written) = record.written_at_ms else {
            return Ok(None);
        };
        if !record.authenticated
            || record.flagged
            || record.not_taken
            || record.divergent
            || record.doubtful
            || record.different_write_at_held_version
            || record.collection != target.collection.name
            || record.policy != target.collection.policy
            || record.key != self.app.record_key(&target)?
            || applied.generation < opening.generation()
        {
            return Ok(None);
        }
        let Ok(writer) = verify_certificate(&operation.cert, &opening.identity()) else {
            return Ok(None);
        };
        if writer.app_id() != record.writer
            || !writer.holds_scope(RECOVERY)
            || !certificate_in_force_at_write(self.app.own_identity(), &writer, written)
        {
            return Ok(None);
        }
        let Some(value) = record.content.value() else {
            return Ok(None);
        };
        let Ok(commitments) =
            RecoveryCommitments::verify(value, opening, self.app.own_identity(), original, written)
        else {
            return Ok(None);
        };
        Ok(Some(RecoveryReport {
            original: *original,
            publisher: record.writer,
            application: applied,
            cutoff: commitments.cutoff,
            inventory: commitments.inventory,
            dispositions: commitments.dispositions,
        }))
    }

    fn require_recovery_reader(&self) -> Result<()> {
        self.refresh_own()?;
        let cert = self.app.cert();
        if !self.own_identity_fresh()
            || !self.holds(RECOVERY)
            || !self
                .app
                .own_identity()
                .current_now(&self.app_id(), &cert.hash())
            || cert.expired_at(self.now())
            || self
                .app
                .scope_key(self.app.data_epoch(), RECOVERY)
                .is_none()
        {
            return Err(Refusal::NotNow("the wallet needs current recovery access".into()).into());
        }
        Ok(())
    }

    async fn refresh_recovery_reader(&mut self) -> Result<()> {
        let refreshed = self.app.refresh_identity().await;
        self.note_own_record_write(&refreshed);
        if refreshed?.confirmed_at().is_none() {
            return Err(
                Refusal::NotNow("current recovery access cannot be verified".into()).into(),
            );
        }
        self.require_recovery_reader()
    }

    async fn current_opening(&mut self, loss: &LossBoundary) -> Result<IdentityDoc> {
        let current = self.decisive_loss().await?.map(|item| item.boundary);
        if current.as_ref() != Some(loss) {
            return Err(Refusal::LossChanged { decisive: current }.into());
        }
        self.app
            .recovery_roster()
            .await?
            .filter(|doc| {
                doc.identity() == self.identity()
                    && doc.generation() <= loss.generation()
                    && doc.body().recovery_from.is_some()
                    && doc.body().recovery_from == self.app.own_identity().body().recovery_from
            })
            .ok_or_else(|| Refusal::NotNow("the recovery opening is not available".into()).into())
    }

    /// Read the opening and the explicit removals for the provider loss the wallet
    /// follows. An original app can subsequently supply its own report. This read
    /// never confirms a payment or a settlement.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] until the current grant, key and opening are available.
    /// [`Refusal::LossChanged`] if a different loss now decides. Identity and store
    /// errors propagate.
    pub async fn recovery_members(&mut self) -> Result<Option<RecoveryMembers>> {
        self.refresh_recovery_reader().await?;
        let Some(current) = self.decisive_loss().await? else {
            return Ok(None);
        };
        let loss = current.boundary;
        let opening = self.current_opening(&loss).await?;
        let identity_version = self.app.own_identity().version();
        let mut members = RecoveryMembers {
            loss,
            opening: opening.hash(),
            originals: Vec::new(),
            reports: Vec::new(),
            removed: Vec::new(),
        };
        for member in &opening.body().apps {
            if self.app.recovery_removal(&member.app_id).await?.is_some() {
                members.removed.push(member.app_id);
            } else {
                members.originals.push(member.app_id);
            }
            if let Some(report) = Box::pin(self.original_report(&opening, &member.app_id)).await? {
                members.reports.push(report);
            }
        }
        self.refresh_recovery_reader().await?;
        if self.current_opening(&loss).await?.hash() != members.opening {
            return Err(
                Refusal::NotNow("the recovery opening changed during the read".into()).into(),
            );
        }
        if self.app.own_identity().version() != identity_version {
            return Err(Refusal::NotNow(
                "the app directory changed during the recovery read".into(),
            )
            .into());
        }
        self.require_recovery_reader()?;
        Ok(Some(members))
    }
}
