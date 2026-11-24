//! The payee's requests and receipts through the bindings (Paykit spec 6, 8.1, 9).

use super::{
    method::Account,
    refusal::categorized,
    signer,
    wallet::{enrolled_payer, enrolled_wallet, run},
    TestResult,
};
use crate::{
    Amount, ConversionRate, ListEndpoint, MessageDelivery, NativeWallet, PayeeRequest, PayeeStatus,
    ReceiptDetails, Recurrence as RecurrenceRule, RecurrenceUnit, RequestConversion,
    RequestDestination, RequestDraft, ResendSelection, RuntimeTimer, StorageError, WalletConfig,
    WalletError, WalletScopes,
};
use paykit_lib::{
    bodies::Conversion,
    periods::{AnchoredRule, Recurrence, Unit},
};
use paykit_sdk::{self as sdk, AppScopes, Paykit, RequestRef};
use pubky_switch_client::{app::LocalWrite, store::MemoryStore};
use pubky_switch_core::json::check_uuid_v4;
use pubky_switch_harness::{parties::Asking, Backend, World};
use std::{
    error::Error,
    sync::{atomic::Ordering, Arc},
};

const ENDPOINT: &str = "btc-lightning-bolt11";

fn draft() -> RequestDraft {
    RequestDraft {
        amount: Amount {
            asset: "btc".into(),
            value: "7".into(),
        },
        destination: RequestDestination::Endpoints {
            endpoints: vec![ListEndpoint {
                identifier: ENDPOINT.into(),
                payload: "lnbc1checkout".into(),
            }],
        },
        payment_reference: "checkout-7".into(),
        proposal_expires_at_ms: None,
        payment_deadline_ms: None,
        accepted_endpoint_identifiers: Some(vec![ENDPOINT.into()]),
        conversion: None,
        recurrence: None,
        period_deadline_s: None,
        per_period_endpoints: false,
        metadata_json: Some(br#"{"tax":1.25}"#.to_vec()),
        invite_code: None,
    }
}

fn refused<T>(result: Result<T, WalletError>, case: &str, code: &str) -> TestResult {
    let (actual, failure) = result
        .err()
        .and_then(categorized)
        .ok_or("expected a categorized refusal")?;
    assert_eq!(actual, case);
    assert_eq!(failure.code(), code);
    Ok(())
}

fn invalid<T>(result: Result<T, WalletError>, expected: &str) {
    assert!(matches!(result, Err(WalletError::InvalidInput { field }) if field == expected));
}

#[test]
fn test_payee_inputs_preserve_choices_and_reject_malformed_fields() -> TestResult {
    let input = RequestDraft {
        destination: RequestDestination::RequiredApp {
            app_id: vec![4; 16],
        },
        proposal_expires_at_ms: Some(123),
        payment_deadline_ms: Some(456),
        conversion: Some(RequestConversion::Fixed {
            rates: vec![ConversionRate {
                asset: "btc-lightning".into(),
                value: "0.25".into(),
            }],
        }),
        invite_code: Some("invitation".into()),
        ..draft()
    };
    let value = sdk::RequestDraft::try_from(input)?;
    assert_eq!(
        value.destination,
        sdk::RequestDestination::RequiredApp([4; 16])
    );
    assert_eq!(value.proposal_expires_at_ms, Some(123));
    assert_eq!(value.payment_deadline_ms, Some(456));
    assert_eq!(value.invite_code.as_deref(), Some("invitation"));
    assert_eq!(
        value.metadata.as_ref().and_then(|m| m.get("tax")),
        Some(&serde_json::json!(1.25))
    );
    assert!(
        matches!(value.conversion, Some(Conversion::Fixed(rates)) if matches!(rates.as_slice(), [rate] if rate.asset().as_str() == "btc-lightning" && rate.value().as_str() == "0.25"))
    );
    assert!(
        matches!(value.accepted_endpoint_identifiers.as_deref(), Some([id]) if id.as_str() == ENDPOINT)
    );
    assert!(value.recurrence.is_none() && !value.per_period_endpoints);
    let rule = RecurrenceRule {
        every: 2,
        unit: RecurrenceUnit::Week,
        anchor: 1_000,
        starts_at: 5_000,
        ends_at: Some(9_000_000_000),
    };
    let recurring = sdk::RequestDraft::try_from(RequestDraft {
        recurrence: Some(rule.clone()),
        period_deadline_s: Some(3_600),
        per_period_endpoints: true,
        conversion: Some(RequestConversion::PerPeriod),
        ..draft()
    })?;
    assert_eq!(recurring.recurrence.map(RecurrenceRule::from), Some(rule));
    assert_eq!(recurring.period_deadline_s, Some(3_600));
    assert!(recurring.per_period_endpoints);
    assert!(matches!(recurring.conversion, Some(Conversion::PerPeriod)));
    assert_eq!(
        sdk::RequestDestination::try_from(RequestDestination::Any)?,
        sdk::RequestDestination::Any
    );

    let details = sdk::ReceiptDetails::try_from(ReceiptDetails {
        store_only: true,
        maybe_credited_confirmed: true,
        unmarked_credit_confirmed: true,
        payment_reference: Some("counter sale".into()),
        metadata_json: Some(b"{}".to_vec()),
    })?;
    assert!(
        details.store_only
            && details.maybe_credited_confirmed.is_some()
            && details.unmarked_credit_confirmed.is_some()
    );
    assert_eq!(
        details.payment_reference.as_ref().map(|r| r.as_str()),
        Some("counter sale")
    );
    assert!(details.metadata.as_ref().is_some_and(|m| m.is_empty()));
    assert_eq!(
        sdk::ReceiptDetails::try_from(ReceiptDetails::default())?,
        sdk::ReceiptDetails::new()
    );
    for (unmarked, maybe) in [(true, false), (false, true)] {
        let choices = sdk::ReceiptDetails::try_from(ReceiptDetails {
            unmarked_credit_confirmed: unmarked,
            maybe_credited_confirmed: maybe,
            ..Default::default()
        })?;
        assert_eq!(choices.unmarked_credit_confirmed.is_some(), unmarked);
        assert_eq!(choices.maybe_credited_confirmed.is_some(), maybe);
    }

    let selection = sdk::ResendSelection::try_from(ResendSelection {
        msg_ids: vec!["3198762f-3816-4cd4-86e2-4a8dc40fc737".into()],
        accepted_from_ms: Some(2),
        accepted_until_ms: Some(7),
        request_hashes: vec![vec![9; 32]],
    })?;
    assert_eq!(selection.accepted_from_ms, Some(2));
    assert_eq!(selection.accepted_until_ms, Some(7));
    assert_eq!(selection.request_hashes, [[9; 32]]);
    assert_eq!(selection.msg_ids, ["3198762f-3816-4cd4-86e2-4a8dc40fc737"]);

    for (input, field) in [
        (
            RequestDraft {
                amount: Amount {
                    asset: "btc".into(),
                    value: "-1".into(),
                },
                ..draft()
            },
            "amount",
        ),
        (
            RequestDraft {
                payment_reference: String::new(),
                ..draft()
            },
            "payment_reference",
        ),
        (
            RequestDraft {
                proposal_expires_at_ms: Some(u64::MAX),
                ..draft()
            },
            "proposal_expires_at_ms",
        ),
        (
            RequestDraft {
                payment_deadline_ms: Some(u64::MAX),
                ..draft()
            },
            "payment_deadline_ms",
        ),
        (
            RequestDraft {
                destination: RequestDestination::RequiredApp {
                    app_id: vec![3; 15],
                },
                ..draft()
            },
            "app_id",
        ),
        (
            RequestDraft {
                recurrence: Some(RecurrenceRule {
                    every: 0,
                    unit: RecurrenceUnit::Day,
                    anchor: 0,
                    starts_at: 0,
                    ends_at: None,
                }),
                ..draft()
            },
            "recurrence",
        ),
        (
            // A first period past the wire's time range (Paykit wire 4.4).
            RequestDraft {
                recurrence: Some(RecurrenceRule {
                    every: 10_000,
                    unit: RecurrenceUnit::Year,
                    anchor: 0,
                    starts_at: 0,
                    ends_at: None,
                }),
                ..draft()
            },
            "recurrence",
        ),
        (
            RequestDraft {
                accepted_endpoint_identifiers: Some(vec!["bad/id".into()]),
                ..draft()
            },
            "accepted_endpoint_identifiers",
        ),
        (
            RequestDraft {
                conversion: Some(RequestConversion::Fixed {
                    rates: vec![ConversionRate {
                        asset: "btc".into(),
                        value: "0".into(),
                    }],
                }),
                ..draft()
            },
            "conversion",
        ),
    ] {
        invalid(sdk::RequestDraft::try_from(input), field);
    }
    for bytes in [
        b"[]".as_slice(),
        b"{",
        br#"{"nested":{"x":1,"x":2}}"#,
        br#"{"order":18446744073709551617}"#,
    ] {
        invalid(
            sdk::RequestDraft::try_from(RequestDraft {
                metadata_json: Some(bytes.to_vec()),
                ..draft()
            }),
            "metadata_json",
        );
        invalid(
            sdk::ReceiptDetails::try_from(ReceiptDetails {
                metadata_json: Some(bytes.to_vec()),
                ..Default::default()
            }),
            "metadata_json",
        );
    }
    invalid(
        sdk::ReceiptDetails::try_from(ReceiptDetails {
            payment_reference: Some(String::new()),
            ..Default::default()
        }),
        "payment_reference",
    );
    for (from, until, field) in [
        (Some(u64::MAX), None, "accepted_from_ms"),
        (None, Some(u64::MAX), "accepted_until_ms"),
    ] {
        invalid(
            sdk::ResendSelection::try_from(ResendSelection {
                accepted_from_ms: from,
                accepted_until_ms: until,
                ..Default::default()
            }),
            field,
        );
    }
    invalid(
        sdk::ResendSelection::try_from(ResendSelection {
            msg_ids: vec!["not-a-message-id".into()],
            ..Default::default()
        }),
        "msg_ids",
    );
    // Debug shows lengths and choices, never the host's references, payloads or metadata.
    let shown = format!(
        "{:?} {:?}",
        draft(),
        ReceiptDetails {
            metadata_json: Some(br#"{"buyer":"private"}"#.to_vec()),
            payment_reference: Some("private-reference".into()),
            ..Default::default()
        }
    );
    for secret in ["checkout-7", "lnbc1checkout", "tax", "private"] {
        assert!(!shown.contains(secret), "{shown}");
    }
    Ok(())
}

async fn join(
    world: &World,
    signer: &mut pubky_switch_signer::Signer,
    identity: &str,
    app_name: &str,
    scopes: sdk::RequestedScopes,
    wallet: &NativeWallet,
) -> Result<Paykit, Box<dyn Error>> {
    let owner = world.identity(identity)?;
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let asking = Asking::for_scopes(world.app_keys(&owner, app_name)?, &names)?;
    let review = signer.review(asking.request.signed(), &asking.auth)?;
    let approval = Box::pin(signer.approve(review)).await?;
    let app = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(owner.public(), &approval)?,
        wallet.rail(),
        Arc::new(RuntimeTimer),
    ))
    .await?;
    signer.claim_delivered(&approval.request).await?;
    Ok(app)
}

async fn activity(
    wallet: &NativeWallet,
    request: &RequestRef,
) -> Result<PayeeRequest, Box<dyn Error>> {
    wallet
        .payee_activity()
        .await?
        .into_iter()
        .find(|row| row.request_hash.as_deref() == Some(request.request_hash.as_slice()))
        .ok_or_else(|| "missing payee activity".into())
}

async fn receive(app: &mut Paykit) -> TestResult {
    while Box::pin(app.receive(None)).await?.report.more {}
    Ok(())
}

async fn requests_and_receipts(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    account.park.store(false, Ordering::SeqCst);
    let label = "orders";
    let (_, wallet, _, _) = Box::pin(enrolled_wallet(
        world,
        root.path(),
        account,
        WalletScopes {
            request: true,
            contacts: true,
            manage_contacts: true,
            ..Default::default()
        },
        Some(WalletConfig {
            selling_label: Some(label.into()),
            ..Default::default()
        }),
    ))
    .await?;
    refused(
        wallet.payee_activity().await,
        "retry_later",
        "records_not_listed",
    )?;
    while wallet.receive().await?.more {}
    let mut issuer = Box::pin(signer::serving(world, "shop")).await?;
    let mut payer = Box::pin(join(
        world,
        &mut issuer,
        "shop",
        "payer",
        AppScopes::new().pay().manage_contacts().build()?,
        &wallet,
    ))
    .await?;
    Box::pin(receive(&mut payer)).await?;
    let expires = world
        .now_ms()
        .checked_add(3_600_000)
        .ok_or("invite expiry overflow")?;
    let invite = payer.app_mut().register_invite_code(expires, 1).await?;
    let peer = payer.identity();
    wallet.allow_connect(peer.into()).await?;
    let sent = wallet
        .send_request(
            peer.into(),
            RequestDraft {
                invite_code: Some(invite.code),
                proposal_expires_at_ms: Some(expires),
                payment_deadline_ms: Some(expires),
                ..draft()
            },
        )
        .await?;
    check_uuid_v4(&sent.msg_id)?;
    assert!(
        matches!(sent.delivery, MessageDelivery::Accepted { accepted_at_ms } if accepted_at_ms == world.now_ms())
    );
    let request: RequestRef = sent.request.ok_or("missing signed request")?.try_into()?;
    assert_eq!(wallet.identity().await?, request.payee);
    let row = activity(&wallet, &request).await?;
    assert_eq!(row.status, PayeeStatus::Requested);
    assert_eq!(row.label.as_deref(), Some(label));
    assert_eq!(row.payer, peer);
    assert!(!row.divergent && !row.recurring && row.periods.is_empty() && row.attempt_id.is_none());
    let amount = row.amount.ok_or("missing requested amount")?;
    assert_eq!((amount.asset.as_str(), amount.value.as_str()), ("btc", "7"));
    let record = row.record;
    refused(
        wallet.verify_payment(record.clone()).await,
        "permanent",
        "not_found",
    )?;
    refused(
        wallet.confirm_payment(record.clone(), None).await,
        "permanent",
        "not_found",
    )?;

    Box::pin(receive(&mut payer)).await?;
    let outcome = Box::pin(payer.pay_request(&request, Default::default()))
        .await
        .outcome?;
    assert!(matches!(outcome, sdk::PayOutcome::Paid { .. }));
    while wallet.receive().await?.more {}
    let row = activity(&wallet, &request).await?;
    assert_eq!(row.status, PayeeStatus::ProofReceived);
    assert_eq!(row.attempt_id, Some(outcome.attempt_id().to_text()));
    assert_eq!(
        wallet.verify_payment(record.clone()).await?,
        Some(world.now_ms())
    );
    let receipt_id = wallet
        .confirm_payment(
            record.clone(),
            Some(ReceiptDetails {
                metadata_json: Some(br#"{"total":7.25}"#.to_vec()),
                ..Default::default()
            }),
        )
        .await?
        .ok_or("missing receipt ID")?;
    check_uuid_v4(&receipt_id)?;
    assert_eq!(
        wallet
            .confirm_payment(record.clone(), None)
            .await?
            .as_deref(),
        Some(receipt_id.as_str())
    );
    Box::pin(receive(&mut payer)).await?;
    let received = payer.receipts(sdk::ReceiptsOf::Request(request)).await?;
    let [receipt] = received.as_slice() else {
        return Err("expected a single receipt".into());
    };
    assert_eq!(receipt.msg_id, receipt_id);
    assert_eq!(receipt.payment_reference.as_deref(), Some("checkout-7"));
    assert_eq!(
        receipt.metadata.as_ref().and_then(|m| m.get("total")),
        Some(&serde_json::json!(7.25))
    );
    assert_eq!(
        activity(&wallet, &request).await?.status,
        PayeeStatus::Receipted
    );

    let resend = wallet
        .request_resend(
            peer.into(),
            ResendSelection {
                request_hashes: vec![request.request_hash.into()],
                ..Default::default()
            },
        )
        .await?;
    check_uuid_v4(&resend)?;
    assert!(matches!(
        wallet.message_delivery(resend).await?,
        Some(MessageDelivery::Accepted { .. })
    ));

    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_payee_request_proof_and_retained_receipt() {
    run(Backend::Scripted, requests_and_receipts);
    run(Backend::Service, requests_and_receipts);
}

async fn cancellations(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    account.park.store(false, Ordering::SeqCst);
    let (_, wallet, _, _) = Box::pin(enrolled_wallet(
        world,
        root.path(),
        account,
        WalletScopes {
            request: true,
            contacts: true,
            ..Default::default()
        },
        None,
    ))
    .await?;
    let mut issuer = Box::pin(signer::serving(world, "shop")).await?;
    let mut payer = Box::pin(join(
        world,
        &mut issuer,
        "shop",
        "payer",
        AppScopes::new().pay().build()?,
        &wallet,
    ))
    .await?;
    // The payer's first read lists its records; later reads bring the payee's messages.
    Box::pin(receive(&mut payer)).await?;
    let peer = payer.identity();
    Box::pin(payer.allow_connect(&world.identity("buyer")?.public())).await?;
    while wallet.receive().await?.more {}
    wallet.allow_connect(peer.into()).await?;
    let sent = wallet.send_request(peer.into(), draft()).await?;
    let request: RequestRef = sent.request.ok_or("missing request")?.try_into()?;
    let record = activity(&wallet, &request).await?.record;
    let cancellation = wallet
        .cancel_sent_request(record.clone(), Some("withdrawn".into()))
        .await?;
    check_uuid_v4(&cancellation)?;
    assert_eq!(
        activity(&wallet, &request).await?.status,
        PayeeStatus::Cancelled
    );
    refused(
        wallet.cancel_sent_request(record, None).await,
        "permanent",
        "cancelled",
    )?;
    Box::pin(receive(&mut payer)).await?;
    assert!(payer
        .requests()
        .await?
        .iter()
        .any(|r| r.request == request && r.status == sdk::RequestStatus::Cancelled));

    let start = world.now_ms();
    let rule = AnchoredRule::new(start, 1, Unit::Day)?;
    let period = rule.period(0)?.into();
    let terms = Recurrence::new(rule, start, None)?;
    let recurring: RequestRef = wallet
        .send_request(
            peer.into(),
            RequestDraft {
                recurrence: Some(terms.into()),
                ..draft()
            },
        )
        .await?
        .request
        .ok_or("missing recurring request")?
        .try_into()?;
    let recurring_row = activity(&wallet, &recurring).await?;
    assert!(recurring_row.recurring);
    assert_eq!(recurring_row.status, PayeeStatus::Requested);
    Box::pin(receive(&mut payer)).await?;
    Box::pin(payer.accept_subscription(&recurring)).await?;
    let paid = Box::pin(payer.pay_period(&recurring, &period, Default::default()))
        .await
        .outcome?;
    assert!(matches!(paid, sdk::PayOutcome::Paid { .. }));
    while wallet.receive().await?.more {}
    let subscribed = activity(&wallet, &recurring).await?;
    assert_eq!(subscribed.status, PayeeStatus::Accepted);
    let [held] = subscribed.periods.as_slice() else {
        return Err("missing paid Billing Period".into());
    };
    assert_eq!(
        (held.billing_period.starts_at, held.billing_period.ends_at),
        (period.starts_at, period.ends_at)
    );
    assert!(held.paid && !held.credited && held.receipt.is_none() && held.details.is_empty());
    assert_eq!(held.record.len(), 32);
    assert_ne!(held.record, subscribed.record);
    // The period's own record key verifies and confirms the period's payment (Paykit spec 8.2).
    assert_eq!(
        wallet.verify_payment(held.record.clone()).await?,
        Some(world.now_ms())
    );
    let receipt = wallet
        .confirm_payment(held.record.clone(), None)
        .await?
        .ok_or("missing period receipt")?;
    check_uuid_v4(&receipt)?;
    let credited = activity(&wallet, &recurring).await?;
    let [credited] = credited.periods.as_slice() else {
        return Err("missing credited Billing Period".into());
    };
    assert!(credited.paid && credited.credited);
    let shown = credited.receipt.as_ref().ok_or("missing shown receipt")?;
    assert_eq!(shown.message_id, receipt);
    assert_eq!(shown.attempt_id, paid.attempt_id().to_text());
    assert_eq!(
        shown.period.as_ref().map(|p| (p.starts_at, p.ends_at)),
        Some((period.starts_at, period.ends_at))
    );
    assert_eq!(wallet.payee_activity().await?.len(), 2);
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_payee_cancellation_and_recurring_activity() {
    run(Backend::Scripted, cancellations);
    run(Backend::Service, cancellations);
}

async fn refusals(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, wallet, _, _) = Box::pin(enrolled_payer(world, root.path(), account)).await?;
    let peer = world.identity("shop")?.public();
    refused(
        wallet.send_request(peer.into(), draft()).await,
        "needs_user",
        "scope_missing",
    )?;
    refused(
        wallet
            .request_resend(peer.into(), ResendSelection::default())
            .await,
        "not_sent",
        "invalid_input",
    )?;

    invalid(wallet.send_request(vec![1; 31], draft()).await, "to");
    invalid(wallet.verify_payment(vec![1; 33]).await, "record");
    invalid(wallet.confirm_payment(vec![], None).await, "record");
    invalid(
        wallet.cancel_sent_request(vec![1; 31], None).await,
        "record",
    );
    invalid(
        wallet.request_resend(vec![0; 16], Default::default()).await,
        "counterparty",
    );
    invalid(
        wallet
            .request_resend(
                peer.into(),
                ResendSelection {
                    request_hashes: vec![vec![2; 31]],
                    ..Default::default()
                },
            )
            .await,
        "request_hashes",
    );

    assert!(wallet.retained_payee_receipts().await?.is_empty());
    let unreadable = LocalWrite::Put {
        key: b"rc/unreadable".to_vec(),
        value: b"{}".to_vec(),
    };
    wallet
        .lend(async move |app| app.app_mut().protocol_commit("paykit", &[unreadable]).await)
        .await??;
    assert!(matches!(
        wallet.retained_payee_receipts().await,
        Err(WalletError::Storage {
            failure: StorageError::Corrupt
        })
    ));
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_payee_refusals_and_invalid_keys() {
    run(Backend::Scripted, refusals);
    run(Backend::Service, refusals);
}
