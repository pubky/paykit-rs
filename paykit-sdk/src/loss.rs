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
//! An uncertain obligation stays in doubt, whichever own app attempted it or may have, this
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
//! An obligation the app can't place before or after the loss, such as one whose request it
//! can't date, is in doubt.

use paykit_lib::records::StoredMessage;
use pubky_switch_client::app::{LocalView, LocalWrite, ProviderLoss, StoreFailure, UserConfirmed};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde::{Deserialize, Serialize};

use crate::error::{Refusal, Result};
use crate::journal::JournalEntry;
use crate::paykit::Paykit;
use crate::state::{self, key};

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

    /// Whether the obligations of `request` are uncertain after the loss (Paykit spec 7.3),
    /// or, with `period_starts_at_ms`, those of its Billing Period starting then. A request
    /// counts as made before the loss when the app held it in local state then, its inbox
    /// having accepted it before the end of the residual window, or when its signed
    /// `created_at` is earlier than the end of the residual window plus the clock tolerance:
    /// the old service could still have taken an attempt on it until then. A Billing Period
    /// starting after the end of the residual window is never uncertain. What the app can't
    /// place is: a request it holds no copy of, one with no acceptance time, and every
    /// request while the declaration time isn't known.
    pub(crate) fn uncertain(
        &self,
        request: Option<&StoredMessage>,
        period_starts_at_ms: Option<u64>,
    ) -> bool {
        if period_starts_at_ms.is_some_and(|start| start > self.uncertain_until_ms) {
            return false;
        }
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
    Ok(match kept {
        Some(kept) if kept.generation > boundary.generation => Vec::new(),
        Some(kept) if kept.generation == boundary.generation => vec![put()?],
        _ => vec![
            put()?,
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

    /// The provider loss that leaves `obligation`, of `request` or of its Billing Period
    /// starting at `period_starts_at_ms`, in doubt (Paykit spec 7.3), its claim at the new
    /// service not SETTLED, which the caller judges first: the loss that decides now, when
    /// it makes the obligation uncertain ([`LossBoundary::uncertain`]) and the user hasn't
    /// stated it wasn't paid before it. `request` is `None` for one the app holds no copy
    /// of, which it can't place.
    pub(crate) async fn loss_doubt(
        &self,
        obligation: &[u8; 32],
        request: Option<&StoredMessage>,
        period_starts_at_ms: Option<u64>,
    ) -> Result<Option<LossBoundary>> {
        let Some(loss) = self.decisive_loss().await? else {
            return Ok(None);
        };
        if !loss.boundary.uncertain(request, period_starts_at_ms) {
            return Ok(None);
        }
        if loss.statements_count
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
        period_starts_at_ms: Option<u64>,
    ) -> Result<bool> {
        Ok(self
            .loss_doubt(obligation, request, period_starts_at_ms)
            .await?
            .is_some())
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

    /// The user states that `obligation` wasn't paid before `loss`, the provider loss its
    /// doubt named when the user was shown it (Paykit spec 7.3): its doubt after the loss
    /// ends, for it alone, so a
    /// payment of it goes as any other, an automatic one included, the claim's own doubt
    /// still counting. The host asks only after showing the user that another own app may
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
    ///
    /// # Errors
    ///
    /// [`crate::Refusal::LossChanged`] when `loss` isn't the loss that decides the doubt now,
    /// with the one that does; [`crate::Refusal::NotNow`] while the app keeps no provider
    /// loss: none followed, or the one followed, `loss`, has no step run yet.
    /// [`crate::PaykitError::Lib`] for a certificate Paykit refuses; a store failure.
    pub async fn confirm_not_paid_before_loss(
        &mut self,
        obligation: &[u8; 32],
        loss: &LossBoundary,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        self.refresh_own()?;
        match self.decisive_loss().await? {
            Some(Decisive {
                boundary,
                statements_count: true,
            }) if boundary == *loss => {}
            Some(Decisive { boundary, .. }) if boundary != *loss => {
                return Err(Refusal::LossChanged {
                    decisive: Some(boundary),
                }
                .into());
            }
            _ => {
                return Err(Refusal::NotNow(
                    "Paykit spec 7.3: no provider loss is kept, so nothing is in doubt after one"
                        .into(),
                )
                .into());
            }
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
        assert!(loss.uncertain(Some(&request(UNTIL, Some(UNTIL - 1))), None));
        // Created before the declaration, accepted after the residual window.
        assert!(loss.uncertain(Some(&request(DECLARED - 1, Some(UNTIL + 5))), None));
        // Created and accepted after: not uncertain.
        let created = UNTIL + CLOCK_TOLERANCE_MS;
        assert!(!loss.uncertain(Some(&request(created, Some(UNTIL))), None));
    }

    #[test]
    fn test_loss_uncertain_request_created_inside_the_residual_window() {
        let loss = boundary();
        // Created after the declaration plus the clock tolerance, but while the old service
        // could still take an attempt, and accepted only after the residual window.
        let created = DECLARED + CLOCK_TOLERANCE_MS;
        assert!(loss.uncertain(Some(&request(created, Some(UNTIL + 5))), None));
        let created = UNTIL + CLOCK_TOLERANCE_MS - 1;
        assert!(loss.uncertain(Some(&request(created, Some(UNTIL + 5))), None));
        // From the end of the residual window plus the clock tolerance on: not uncertain.
        let created = UNTIL + CLOCK_TOLERANCE_MS;
        assert!(!loss.uncertain(Some(&request(created, Some(UNTIL + 5))), None));
    }

    #[test]
    fn test_loss_uncertain_what_can_not_be_placed() {
        let loss = boundary();
        let after = UNTIL + CLOCK_TOLERANCE_MS + 1;
        assert!(loss.uncertain(None, None));
        assert!(loss.uncertain(Some(&request(after, None)), None));
        let unknown = LossBoundary {
            declared_at_ms: 0,
            ..loss
        };
        assert!(unknown.uncertain(Some(&request(after, Some(UNTIL + 1))), None));
    }

    #[test]
    fn test_loss_uncertain_billing_period_after_the_window() {
        let loss = boundary();
        let before = request(DECLARED - 1, Some(DECLARED - 1));
        assert!(loss.uncertain(Some(&before), Some(UNTIL)));
        assert!(!loss.uncertain(Some(&before), Some(UNTIL + 1)));
        assert!(!loss.uncertain(None, Some(UNTIL + 1)));
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

    fn statements_dropped() -> LocalWrite {
        LocalWrite::DeletePrefix {
            prefix: key::NOT_PAID_BEFORE_LOSS.as_bytes().to_vec(),
        }
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
            vec![put(newer), statements_dropped()]
        );
        // The first loss the app keeps drops any statement as well.
        assert_eq!(
            writes_over(None, at(2)).unwrap(),
            vec![put(at(2)), statements_dropped()]
        );
    }

    #[test]
    fn test_loss_writes_same_loss_keeps_the_statements() {
        let again = LossBoundary {
            uncertain_until_ms: UNTIL + 1,
            ..at(2)
        };
        assert_eq!(writes_over(Some(at(2)), again).unwrap(), vec![put(again)]);
    }

    #[test]
    fn test_loss_writes_older_loss_writes_nothing() {
        assert!(writes_over(Some(at(3)), at(2)).unwrap().is_empty());
    }
}
