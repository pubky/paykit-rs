//! The refusals of payments under an Allowance a host's scheduler acts on, between enrolled
//! wallets on both backends: an end refuses for good at every retry, and the user's
//! confirmation after a restore that isn't clean is asked once and taken.

use super::*;

/// The enrolled payer and seller, the Allowance granted to the payer's wallet.
async fn granted(world: &World) -> TestResult<GrantedWallets> {
    let wallets = Box::pin(grant(world, AllowanceRole::Allower, GrantFlow::FirstGrant)).await?;
    wallets.rail.set_history_payer(wallets.payer.identity());
    Ok(wallets)
}

#[tokio::test]
async fn test_enrolled_allowance_end_is_final_at_every_retry() -> TestResult {
    use crate::common::{draft, request_from};
    use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};

    tokio::task::LocalSet::new()
        .run_until(async {
            for backend in [Backend::Scripted, Backend::Service] {
                let Some(world) = World::start(backend)? else {
                    continue;
                };
                let GrantedWallets {
                    mut payer,
                    mut seller,
                    rail,
                    key,
                    ..
                } = Box::pin(granted(&world)).await?;
                let start = world.now_ms() / 1_000 * 1_000;
                let schedule =
                    Recurrence::new(AnchoredRule::new(start, 1, Unit::Hour)?, start, None)?;
                let periods = schedule.periods(2)?;
                let request = Box::pin(request_from(
                    &mut seller,
                    payer.identity(),
                    draft("1", start).with_recurrence(schedule),
                ))
                .await;
                Box::pin(finish_listing(&mut payer)).await;
                Box::pin(payer.accept_subscription_under_allowance(&request, &key)).await?;
                let first = Box::pin(
                    payer.pay_period_under_allowance(&request, &PeriodRef::from(periods[0])),
                )
                .await
                .outcome?;
                assert!(
                    matches!(first, paykit_sdk::PayOutcome::Paid { .. }),
                    "{first:?}"
                );
                Box::pin(payer.end_allowance(&key)).await?;
                let next = PeriodRef::from(periods[1]);
                world.advance_ms(next.starts_at.saturating_sub(world.now_ms()) + 1_000);
                for _ in 0..3 {
                    let again = Box::pin(payer.pay_period_under_allowance(&request, &next))
                        .await
                        .outcome
                        .unwrap_err();
                    assert_eq!(
                        again.refusal(),
                        Some(&paykit_sdk::Refusal::AllowanceEnded),
                        "{again:?}"
                    );
                    world.advance_ms(60_000);
                }
                assert_eq!(rail.all_executions().len(), 1);
                world.close().await?;
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn test_enrolled_allowance_confirmation_after_a_restore_that_isnt_clean() -> TestResult {
    tokio::task::LocalSet::new()
        .run_until(async {
            for backend in [Backend::Scripted, Backend::Service] {
                let Some(world) = World::start(backend)? else {
                    continue;
                };
                let GrantedWallets {
                    mut payer,
                    rail,
                    key,
                    ..
                } = Box::pin(granted(&world)).await?;
                let backup = Box::pin(payer.backup()).await?;
                drop(payer);
                let mut restored = Box::pin(Paykit::restore(
                    app_env(&world, Arc::new(MemoryStore::new())),
                    Default::default(),
                    &backup,
                    rail.clone(),
                    Arc::new(TestTimer {
                        clock: world.clock().clone(),
                    }),
                ))
                .await?;
                crate::common::restore_with_unknown_cursor(&mut restored).await;
                Box::pin(finish_restore(&mut restored)).await?;
                let view = restored.allowance(&key).await?;
                assert!(view.confirmation_needed, "{view:?}");
                for _ in 0..2 {
                    let asked = Box::pin(restored.resume_allowance_executor(&key))
                        .await
                        .unwrap_err();
                    assert_eq!(
                        asked.refusal(),
                        Some(&paykit_sdk::Refusal::AllowanceConfirmationNeeded { record: key }),
                        "{asked:?}"
                    );
                }
                Box::pin(restored.confirm_allowance_executor(&key, UserConfirmed::new())).await?;
                let view = restored.allowance(&key).await?;
                assert!(view.consented && !view.confirmation_needed, "{view:?}");
                Box::pin(restored.resume_allowance_executor(&key)).await?;
                world.close().await?;
            }
            Ok(())
        })
        .await
}
