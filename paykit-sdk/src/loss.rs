//! After a provider loss (Paykit spec 7.3; Switch spec 12.6): which obligations stay in
//! doubt at the new service.
//!
//! An obligation is uncertain when its request or Billing Period was payable before the
//! declaration plus the residual window: another own app may have paid it at the old service,
//! and nothing of that reaches the new one. The app keeps the loss it followed last
//! ([`LossBoundary`], `p/pl`): the generation recovered at, the declaration time and the end
//! of the residual window. The protocol's loss step writes it with the loss's re-sends, in
//! their one commit ([`loss_writes`]); a newer loss replaces it and drops the user's
//! statements about the older one, an older one leaves it. A backup carries it, with those
//! statements; an app restored from a backup taken before it learned of the loss writes it
//! again when it follows the loss. While the client follows a newer loss than the one kept,
//! before its loss step has run, the client's view of that loss counts instead, with no
//! statement of the user's.
//!
//! An uncertain one-time request stays in doubt, whichever own app attempted it or may have, this
//! app included, until its claim at the new service shows it SETTLED or the user states that
//! it wasn't paid before the loss ([`Paykit::confirm_not_paid_before_loss`]):
//!
//! - a payment by hand is refused [`crate::Refusal::MayBePaid`], its doubt showing
//!   [`crate::PaymentDoubt::before_loss`], and goes ahead only once the user has seen it and
//!   confirmed it ([`crate::PayOptions::with_may_be_paid_confirmed`]); a confirmed payment
//!   that fails leaves the doubt as it was;
//! - no automatic payment is made on it, with no confirmation passing it, and a resume never
//!   repeats a dispatch made before the loss;
//! - the request's status reads [`crate::RequestStatus::MayBePaid`].
//!
//! A period of an earlier subscription stays held while its payment history is incomplete,
//! even if the user confirms paying it early or previously stated it was unpaid.
//! An obligation the app can't place before or after the loss, such as one whose request it
//! can't date, is in doubt.
//!
//! **The shared post-loss hold** (Paykit spec 8.1, Blocked counterparties; Paykit wire 5.6):
//! apart from that doubt, once the newest own `identity.json` the app has verified carries a
//! non-null `recovery_from`, or the app holds a recorded loss (`p/rl`, kept from the
//! installation over an in-place restore, `p/pl`, or a loss the client follows before its
//! step has run), no own app pays a one-time request automatically or
//! repeats a one-time payment on a resume, the repeat of an attempt its user started included
//! ([`Paykit::shared_loss_hold`]). A higher block floor may have existed only at the lost
//! provider, and nothing at the replacement bounds it. `recovery_from` stays in every later
//! `identity.json` until a later loss replaces it (Switch spec 4.3), so the hold holds through
//! moves and key changes, at every own app, one that joins or is restored later included.
//! Nothing in v0.2 lifts it: a user's statement that an obligation wasn't paid before the loss
//! ends only that obligation's doubt for a payment by hand. A payment the user starts by hand
//! still goes, as do Billing Periods, payments without a request, stops and receipts, and a
//! history lookup by attempt ID, which only reads. A cooperative move or a key change in place
//! with `recovery_from: null` and no recorded loss holds nothing.

use paykit_lib::periods::PeriodRef;
use paykit_lib::records::StoredMessage;
use pubky_switch_client::app::{LocalView, LocalWrite, ProviderLoss, StoreFailure, UserConfirmed};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::journal::JournalEntry;
use crate::paykit::Paykit;
use crate::state::{self, key};

mod reports;
pub use reports::{RecoveryMembers, RecoveryReport};

/// The reason of a [`crate::Refusal::NotAutomatic`] under the shared post-loss hold.
pub(crate) const SHARED_LOSS_HOLD: &str =
    "Paykit spec 8.1: automatic one-time payments are held for good after a provider loss";

/// The reason of a [`crate::Refusal::MayBePaid`] for an obligation a provider loss leaves
/// in doubt.
pub(crate) const LOSS_DOUBT: &str =
    "Paykit spec 7.3: payable before a provider loss, and not SETTLED at the new service";

/// A provider loss, as Paykit keeps the one the app followed last (`p/pl`) for the doubt it
/// leaves (Paykit spec 7.3), and as a doubt after a loss names the loss that decides it
/// ([`crate::PaymentDoubt::before_loss`], [`crate::Doubtful::before_loss`]). The user's answer
/// to that doubt carries it back ([`crate::PayOptions::with_may_be_paid_confirmed`],
/// [`Paykit::confirm_not_paid_before_loss`]), and is refused once another loss decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LossBoundary {
    /// The generation recovered at: the newer of two losses is the one at the later
    /// generation.
    pub(crate) generation: u64,
    /// When the loss was declared: the recovery record's service time, in Unix
    /// milliseconds; 0 when the client doesn't know it yet.
    pub(crate) declared_at_ms: u64,
    /// Until when an attempt could have been authorized at the old service: the declaration
    /// plus the residual window and the offset between the services' clocks.
    pub(crate) uncertain_until_ms: u64,
}

impl LossBoundary {
    /// The generation the loss recovered at: of two losses, the newer is the one at the
    /// later generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// When the loss was declared, in Unix milliseconds of the service; 0 while the client
    /// doesn't know it.
    pub fn declared_at_ms(&self) -> u64 {
        self.declared_at_ms
    }

    /// Until when, in Unix milliseconds of the service, an attempt could have been
    /// authorized at the old service.
    pub fn uncertain_until_ms(&self) -> u64 {
        self.uncertain_until_ms
    }

    /// The payment method must date a settlement strictly after the residual window.
    pub(crate) fn settlement_uncertain(&self, paid_at: u64) -> bool {
        self.declared_at_ms == 0 || paid_at <= self.uncertain_until_ms
    }

    /// The boundary of `loss`, as the client tells of it.
    pub(crate) fn of(loss: &ProviderLoss) -> Self {
        Self {
            generation: loss.generation,
            declared_at_ms: loss.declared_at_ms,
            uncertain_until_ms: loss.uncertain_until_ms,
        }
    }

    /// Whether it is one the protocol's loss step writes: recovered at a generation after
    /// another, its residual window ending no earlier than its declaration.
    pub(crate) fn valid(&self) -> bool {
        self.generation > 0 && self.uncertain_until_ms >= self.declared_at_ms
    }

    /// A request could have been paid if it was accepted before the residual window
    /// ended or created within its clock tolerance. A Billing Period of such a request
    /// could also have been paid early. Missing times leave the payment uncertain
    /// (Paykit spec 7.3, 8.2).
    pub(crate) fn uncertain(&self, request: Option<&StoredMessage>) -> bool {
        let Some(request) = request else {
            return true;
        };
        if self.declared_at_ms == 0 {
            return true;
        }
        let held = request
            .accepted_at()
            .is_none_or(|at| at < self.uncertain_until_ms);
        held || request.message().created_at_ms
            < self.uncertain_until_ms.saturating_add(CLOCK_TOLERANCE_MS)
    }
}

/// The protocol's writes for `loss` (Switch spec 12.6 step 4), with its re-sends in one
/// commit: the boundary kept, `view` holding the one kept before. A newer loss replaces it
/// and drops every statement of the user's about the older one; the same loss again
/// replaces it and keeps them; an older loss writes nothing.
///
/// # Errors
///
/// A store failure, or a corrupt store for a kept boundary that doesn't read.
pub(crate) fn loss_writes(
    loss: &ProviderLoss,
    view: &LocalView<'_>,
) -> std::result::Result<Vec<LocalWrite>, StoreFailure> {
    let kept = state::view_get::<LossBoundary>(view, key::LOSS_BOUNDARY)
        .map_err(crate::protocol::view_failure)?;
    writes_over(kept, LossBoundary::of(loss)).map_err(crate::protocol::view_failure)
}

/// The writes of [`loss_writes`] for `boundary`, the loss followed, over `kept`, the boundary
/// kept before it.
fn writes_over(kept: Option<LossBoundary>, boundary: LossBoundary) -> Result<Vec<LocalWrite>> {
    let put = || state::local_put(key::LOSS_BOUNDARY, &boundary);
    // The recorded loss the shared post-loss hold reads (module docs), never lowered: an
    // older loss writes nothing at all.
    let recorded = || state::local_put(key::LOSS_RECORDED, &boundary.generation);
    Ok(match kept {
        Some(kept) if kept.generation > boundary.generation => Vec::new(),
        Some(kept) if kept.generation == boundary.generation => vec![put()?, recorded()?],
        _ => vec![
            put()?,
            recorded()?,
            LocalWrite::DeletePrefix {
                prefix: key::NOT_PAID_BEFORE_LOSS.as_bytes().to_vec(),
            },
        ],
    })
}

/// The loss that decides the doubt: the boundary kept, or the client's view of a newer loss
/// it follows; and whether the user's statements count, which they do for the kept one only.
struct Decisive {
    boundary: LossBoundary,
    statements_count: bool,
}

/// Why a statement about `loss` doesn't count against `decisive`, the loss that decides the
/// doubt now, or `None` when it counts (Paykit spec 7.3): another loss decides; the loss is
/// followed but its step hasn't run, which the step clears; or no loss is followed, which
/// leaves nothing to state.
fn statement_refused(decisive: Option<Decisive>, loss: &LossBoundary) -> Option<Refusal> {
    match decisive {
        Some(Decisive {
            boundary,
            statements_count: true,
        }) if boundary == *loss => None,
        Some(Decisive { boundary, .. }) if boundary != *loss => Some(Refusal::LossChanged {
            decisive: Some(boundary),
        }),
        Some(_) => Some(Refusal::NotNow(crate::NotNowReason::ReadAgain(
            "Paykit spec 7.3: the provider loss followed has no step run yet, so nothing is in \
             doubt after it",
        ))),
        None => Some(Refusal::Usage(
            "Paykit spec 7.3: no provider loss is kept, so nothing is in doubt after one".into(),
        )),
    }
}

impl Paykit {
    async fn decisive_loss(&self) -> Result<Option<Decisive>> {
        let kept = self.local_get::<LossBoundary>(key::LOSS_BOUNDARY).await?;
        let following = self
            .app
            .recovery_info()
            .await?
            .map(|info| LossBoundary::of(&info.loss));
        Ok(match (kept, following) {
            (Some(kept), Some(following)) if following.generation > kept.generation => {
                Some(Decisive {
                    boundary: following,
                    statements_count: false,
                })
            }
            (Some(kept), _) => Some(Decisive {
                boundary: kept,
                statements_count: true,
            }),
            (None, Some(following)) => Some(Decisive {
                boundary: following,
                statements_count: false,
            }),
            (None, None) => None,
        })
    }

    /// Whether the shared post-loss hold stands (module docs): the newest own `identity.json`
    /// the app has verified carries a non-null `recovery_from`, or the app holds a recorded
    /// loss, kept or followed. Having none gives no clearance by itself: the identity decides.
    ///
    /// # Errors
    ///
    /// A store failure, or a kept loss that doesn't read.
    pub(crate) async fn shared_loss_hold(&self) -> Result<bool> {
        if self.app.own_identity().body().recovery_from.is_some()
            || self.local_get::<u64>(key::LOSS_RECORDED).await?.is_some()
        {
            return Ok(true);
        }
        Ok(self.decisive_loss().await?.is_some())
    }

    /// The newest declared loss known to this wallet or its current service.
    pub(crate) async fn settlement_loss(&self) -> Result<Option<LossBoundary>> {
        Ok(self.decisive_loss().await?.map(|loss| loss.boundary))
    }

    /// The provider loss that leaves `obligation`, of `request` or of its Billing Period
    /// named by `period`, in doubt (Paykit spec 7.3), its claim at the new
    /// service not SETTLED, which the caller judges first: the loss that decides now, when
    /// it makes the obligation uncertain ([`LossBoundary::uncertain`]) and the user hasn't
    /// stated a one-time request wasn't paid before it. An unresolved Billing Period
    /// remains held even after user confirmation. `request` is `None` for one the app holds no copy
    /// of, which it can't place.
    pub(crate) async fn loss_doubt(
        &self,
        obligation: &[u8; 32],
        request: Option<&StoredMessage>,
        period: Option<PeriodRef>,
    ) -> Result<Option<LossBoundary>> {
        let Some(loss) = self.decisive_loss().await? else {
            return Ok(None);
        };
        if !loss.boundary.uncertain(request) {
            return Ok(None);
        }
        if period.is_none()
            && loss.statements_count
            && self
                .local_get::<bool>(&key::not_paid_before_loss(obligation))
                .await?
                .is_some()
        {
            return Ok(None);
        }
        Ok(Some(loss.boundary))
    }

    /// Whether a provider loss leaves `obligation` in doubt ([`Paykit::loss_doubt`]).
    pub(crate) async fn in_doubt_after_loss(
        &self,
        obligation: &[u8; 32],
        request: Option<&StoredMessage>,
        period: Option<PeriodRef>,
    ) -> Result<bool> {
        Ok(self
            .loss_doubt(obligation, request, period)
            .await?
            .is_some())
    }

    /// Recheck the signed subscription of a journaled payment before reserving or
    /// dispatching its exact Billing Period (Paykit spec 7.3, 8.2).
    pub(crate) async fn period_recovery_held(&self, entry: &JournalEntry) -> Result<bool> {
        let Some(period) = entry.period()? else {
            return Ok(false);
        };
        let Some(loss) = self.decisive_loss().await? else {
            return Ok(false);
        };
        let request = entry.request.as_ref().ok_or_else(PaykitError::corrupt)?;
        let stored = request.stored(&self.identity())?;
        let paykit_lib::Body::PaymentRequest(body) = stored.body() else {
            return Err(PaykitError::corrupt());
        };
        if stored.sender() != entry.payee
            || entry.request_hash()? != Some(stored.hash())
            || body.named_period(&period).is_err()
            || self.obligation(&entry.payee, &stored.hash(), period.start_secs())
                != entry.obligation
        {
            return Err(PaykitError::corrupt());
        }
        Ok(loss.boundary.uncertain(Some(&stored)))
    }

    /// Whether `entry`'s dispatch was made before a provider loss the app follows or followed:
    /// its attempt written at a generation before the one the loss recovered at, or one whose
    /// proof the entry doesn't hold or that doesn't read. A resume never repeats it
    /// (Paykit spec 7.3).
    pub(crate) async fn dispatched_before_loss(&self, entry: &JournalEntry) -> Result<bool> {
        let Some(loss) = self.decisive_loss().await? else {
            return Ok(false);
        };
        let generation = entry
            .attempt_proof
            .as_ref()
            .and_then(|signed| TransitionProof::peek(signed).ok())
            .map(|proof| proof.envelope().generation);
        Ok(generation.is_none_or(|generation| generation < loss.boundary.generation))
    }

    /// Record the user's statement about a one-time obligation before `loss`, the provider loss its
    /// doubt named when the user was shown it (Paykit spec 7.3): its doubt after the loss
    /// ends, for it alone, so a payment of it by hand goes as any other, the claim's own doubt
    /// still counting. It lifts no automatic payment: the shared post-loss hold stands while
    /// the app holds that loss (module docs). The host asks only after showing the user that another own app may
    /// have paid it at the old service: the obligation is the one a
    /// [`crate::Refusal::MayBePaid`] showed with [`crate::PaymentDoubt::before_loss`]. A
    /// payment the user makes over that doubt instead
    /// ([`crate::PayOptions::with_may_be_paid_confirmed`]) states nothing: one that fails
    /// leaves the obligation in doubt. The statement holds for the loss it was given after;
    /// a newer loss puts the obligation in doubt again. Without a loss kept, it is refused
    /// and nothing is written: no loss followed leaves nothing in doubt, and a statement given
    /// before the loss's step has run would be dropped by that step, the loss being newer
    /// than any kept. The statement is of `loss` alone: an answer that comes once another
    /// loss decides, a newer one kept or one followed before its step has run, is refused
    /// and writes nothing, so it never ends the doubt of a loss the user wasn't shown.
    /// A Billing Period of an earlier subscription stays held pending its payment history.
    ///
    /// # Errors
    ///
    /// [`crate::Refusal::LossChanged`] when `loss` isn't the loss that decides the doubt now,
    /// with the one that does; [`crate::Refusal::Usage`] while the app follows no provider
    /// loss, so nothing is in doubt after one; [`crate::Refusal::NotNow`] while the one
    /// followed, `loss`, has no step run yet.
    /// [`crate::PaykitError::Lib`] for a certificate Paykit refuses; a store failure.
    pub async fn confirm_not_paid_before_loss(
        &mut self,
        obligation: &[u8; 32],
        loss: &LossBoundary,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.refresh_own()?;
        if let Some(refusal) = statement_refused(self.decisive_loss().await?, loss) {
            return Err(refusal.into());
        }
        self.local_put(&key::not_paid_before_loss(obligation), &true)
            .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::{object, Party, NOW};

    const DECLARED: u64 = NOW + 1_000_000;
    const UNTIL: u64 = DECLARED + 70_000;

    fn boundary() -> LossBoundary {
        LossBoundary {
            generation: 2,
            declared_at_ms: DECLARED,
            uncertain_until_ms: UNTIL,
        }
    }

    /// A request from bob to alice, signed as created at `created_at_ms` and stored as
    /// accepted at `accepted_at`.
    fn request(created_at_ms: u64, accepted_at: Option<u64>) -> StoredMessage {
        let alice = Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_contacts", "paykit_settlements"],
        );
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_contacts", "paykit_settlements"],
        );
        let body = object(json!({
            "amount": { "asset": "btc", "value": "1" },
            "payment_reference": "order-1",
            "proposal_expires_at": null
        }));
        bob.message_accepted(
            &alice,
            "paykit.payment_request",
            body,
            created_at_ms,
            accepted_at,
        )
    }

    #[test]
    fn test_loss_uncertain_request_held_or_created_before() {
        let loss = boundary();
        // Held in local state: accepted before the end of the residual window.
        assert!(loss.uncertain(Some(&request(UNTIL, Some(UNTIL - 1)))));
        // Created before the declaration, accepted after the residual window.
        assert!(loss.uncertain(Some(&request(DECLARED - 1, Some(UNTIL + 5)))));
        // Created and accepted after: not uncertain.
        let created = UNTIL + CLOCK_TOLERANCE_MS;
        assert!(!loss.uncertain(Some(&request(created, Some(UNTIL)))));
    }

    #[test]
    fn test_loss_uncertain_request_created_inside_the_residual_window() {
        let loss = boundary();
        // Created after the declaration plus the clock tolerance, but while the old service
        // could still take an attempt, and accepted only after the residual window.
        let created = DECLARED + CLOCK_TOLERANCE_MS;
        assert!(loss.uncertain(Some(&request(created, Some(UNTIL + 5)))));
        let created = UNTIL + CLOCK_TOLERANCE_MS - 1;
        assert!(loss.uncertain(Some(&request(created, Some(UNTIL + 5)))));
        // From the end of the residual window plus the clock tolerance on: not uncertain.
        let created = UNTIL + CLOCK_TOLERANCE_MS;
        assert!(!loss.uncertain(Some(&request(created, Some(UNTIL + 5)))));
    }

    #[test]
    fn test_loss_uncertain_what_can_not_be_placed() {
        let loss = boundary();
        let after = UNTIL + CLOCK_TOLERANCE_MS + 1;
        assert!(loss.uncertain(None));
        assert!(loss.uncertain(Some(&request(after, None))));
        let unknown = LossBoundary {
            declared_at_ms: 0,
            ..loss
        };
        assert!(unknown.uncertain(Some(&request(after, Some(UNTIL + 1)))));
    }

    /// The boundary of a loss recovered at `generation`.
    fn at(generation: u64) -> LossBoundary {
        LossBoundary {
            generation,
            ..boundary()
        }
    }

    fn put(boundary: LossBoundary) -> LocalWrite {
        state::local_put(key::LOSS_BOUNDARY, &boundary).unwrap()
    }

    fn recorded(boundary: LossBoundary) -> LocalWrite {
        state::local_put(key::LOSS_RECORDED, &boundary.generation).unwrap()
    }

    fn statements_dropped() -> LocalWrite {
        LocalWrite::DeletePrefix {
            prefix: key::NOT_PAID_BEFORE_LOSS.as_bytes().to_vec(),
        }
    }

    #[test]
    fn test_loss_statement_counts_for_the_kept_loss() {
        let kept = Decisive {
            boundary: at(2),
            statements_count: true,
        };
        assert_eq!(statement_refused(Some(kept), &at(2)), None);
        let other = Decisive {
            boundary: at(3),
            statements_count: true,
        };
        assert_eq!(
            statement_refused(Some(other), &at(2)),
            Some(Refusal::LossChanged {
                decisive: Some(at(3))
            })
        );
    }

    #[test]
    fn test_loss_statement_waits_for_the_followed_loss_step() {
        let followed = Decisive {
            boundary: at(2),
            statements_count: false,
        };
        assert!(matches!(
            statement_refused(Some(followed), &at(2)),
            Some(Refusal::NotNow(crate::NotNowReason::ReadAgain(_)))
        ));
    }

    #[test]
    fn test_loss_statement_without_a_loss_is_a_usage_error() {
        assert!(matches!(
            statement_refused(None, &at(2)),
            Some(Refusal::Usage(_))
        ));
    }

    #[test]
    fn test_loss_writes_newer_loss_drops_the_statements() {
        let newer = LossBoundary {
            declared_at_ms: UNTIL + 1,
            uncertain_until_ms: UNTIL + 70_000,
            ..at(3)
        };
        assert_eq!(
            writes_over(Some(at(2)), newer).unwrap(),
            vec![put(newer), recorded(newer), statements_dropped()]
        );
        // The first loss the app keeps drops any statement as well.
        assert_eq!(
            writes_over(None, at(2)).unwrap(),
            vec![put(at(2)), recorded(at(2)), statements_dropped()]
        );
    }

    #[test]
    fn test_loss_writes_same_loss_keeps_the_statements() {
        let again = LossBoundary {
            uncertain_until_ms: UNTIL + 1,
            ..at(2)
        };
        assert_eq!(
            writes_over(Some(at(2)), again).unwrap(),
            vec![put(again), recorded(again)]
        );
    }

    #[test]
    fn test_loss_writes_older_loss_writes_nothing() {
        assert!(writes_over(Some(at(3)), at(2)).unwrap().is_empty());
    }
}
