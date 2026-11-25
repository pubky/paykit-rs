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
        let reader = RecoveryReader {
            fresh: self.own_identity_fresh(),
            current: self
                .app
                .own_identity()
                .current_now(&self.app_id(), &cert.hash())
                && !cert.expired_at(self.now()),
            scope: self.holds(RECOVERY),
            key: self
                .app
                .scope_key(self.app.data_epoch(), RECOVERY)
                .is_some(),
        };
        reader
            .refusal()
            .map_or(Ok(()), |refusal| Err(refusal.into()))
    }

    async fn refresh_recovery_reader(&mut self) -> Result<()> {
        let refreshed = self.app.refresh_identity().await;
        self.note_own_record_write(&refreshed);
        if refreshed?.confirmed_at().is_none() {
            return Err(Refusal::NotNow(crate::NotNowReason::ReadAgain(
                "current recovery access cannot be verified",
            ))
            .into());
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
            .ok_or_else(|| {
                // A pruned chain prefix has no way back here: shown, not waited on.
                Refusal::HeldForUser(crate::HeldForUserReason::Unresolved(
                    "the recovery opening is not available",
                ))
                .into()
            })
    }

    /// Read the opening and the explicit removals for the provider loss the wallet
    /// follows. An original app can subsequently supply its own report. This read
    /// never confirms a payment or a settlement.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] until the own `identity.json` is confirmed fresh
    /// ([`crate::NotNowReason::ReadAgain`]) or the recovery scope's key is stored
    /// ([`crate::NotNowReason::ScopeKeysAwaited`]); [`Refusal::HeldForUser`] while the opening
    /// isn't available ([`crate::HeldForUserReason::Unresolved`]); [`Refusal::NotCurrent`]
    /// while the app's certificate isn't current or has expired; [`Refusal::ScopeMissing`]
    /// while the certificate lacks `paykit_recovery`.
    /// [`Refusal::LossChanged`] if a different loss now decides. Identity and store
    /// errors propagate.
    pub async fn recovery_members(&mut self) -> Result<Option<RecoveryMembers>> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
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
            return Err(Refusal::NotNow(crate::NotNowReason::ReadAgain(
                "the recovery opening changed during the read",
            ))
            .into());
        }
        if self.app.own_identity().version() != identity_version {
            return Err(Refusal::NotNow(crate::NotNowReason::ReadAgain(
                "the app directory changed during the recovery read",
            ))
            .into());
        }
        self.require_recovery_reader()?;
        Ok(Some(members))
    }
}

/// What the app holds for a recovery read: its own `identity.json` confirmed fresh, a
/// certificate listed as current and not expired, and the recovery scope with its key.
#[derive(Clone, Copy, Debug)]
struct RecoveryReader {
    fresh: bool,
    current: bool,
    scope: bool,
    key: bool,
}

impl RecoveryReader {
    /// The refusal of a recovery read, by what clears it: a read of the own identity, a
    /// current certificate, a certificate holding the recovery scope, which the user
    /// approves, or its key, which the signer stores when it is next online (Switch spec 3.5).
    fn refusal(self) -> Option<Refusal> {
        if !self.fresh {
            Some(Refusal::NotNow(crate::NotNowReason::ReadAgain(
                "the own identity.json is not confirmed fresh for the recovery read",
            )))
        } else if !self.current {
            Some(Refusal::NotCurrent(
                "the app's certificate is not current for the recovery read".into(),
            ))
        } else if !self.scope {
            Some(Refusal::ScopeMissing(RECOVERY.into()))
        } else if !self.key {
            Some(Refusal::NotNow(crate::NotNowReason::ScopeKeysAwaited))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READER: RecoveryReader = RecoveryReader {
        fresh: true,
        current: true,
        scope: true,
        key: true,
    };

    #[test]
    fn test_recovery_reader_with_current_access_reads() {
        assert_eq!(READER.refusal(), None);
    }

    #[test]
    fn test_recovery_reader_waits_for_a_fresh_identity() {
        let stale = RecoveryReader {
            fresh: false,
            ..READER
        };
        assert!(matches!(
            stale.refusal(),
            Some(Refusal::NotNow(crate::NotNowReason::ReadAgain(_)))
        ));
    }

    #[test]
    fn test_recovery_reader_not_current_without_a_current_certificate() {
        let expired = RecoveryReader {
            current: false,
            ..READER
        };
        assert!(matches!(expired.refusal(), Some(Refusal::NotCurrent(_))));
    }

    #[test]
    fn test_recovery_reader_needs_the_recovery_scope_then_its_key() {
        let missing = RecoveryReader {
            scope: false,
            key: false,
            ..READER
        };
        assert_eq!(
            missing.refusal(),
            Some(Refusal::ScopeMissing(RECOVERY.into()))
        );
        let awaited = RecoveryReader {
            key: false,
            ..READER
        };
        assert_eq!(
            awaited.refusal(),
            Some(Refusal::NotNow(crate::NotNowReason::ScopeKeysAwaited))
        );
    }
}
