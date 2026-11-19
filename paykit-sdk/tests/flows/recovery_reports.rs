//! The original signer and the publisher of a recovery report (Paykit wire 5.10).

use std::sync::Arc;

use paykit_lib::collections::Collection;
use paykit_lib::recovery::{RecoveryCommitments, RECOVERY_UPLOAD_LABEL};
use paykit_sdk::{AppScopes, PaykitConfig};
use pubky_switch_client::app::{App, Collection as SwitchCollection, RecordTarget};
use pubky_switch_client::testing::{ProtocolSpec, TestProtocol};
use pubky_switch_core::enc::{b64, lp};

use crate::common::{spec, store, World};
use crate::provider_loss::{declare_loss, follow_loss};

/// Every party has the contacts scope. A certificate missing its list permission
/// still verifies under Switch rules (Paykit spec 3.2).
async fn party(w: &World, name: &str, scopes: AppScopes, lists_read: bool) -> App {
    let mut grant = scopes.contacts().build().unwrap();
    if !lists_read {
        grant.permissions.clear();
    }
    let app = App::join(
        w.env(&w.alice, store()),
        Arc::new(TestProtocol::new(ProtocolSpec {
            name: "paykit".into(),
            ..spec()
        })),
        PaykitConfig::default().app,
        w.approve(&w.alice, name, &grant),
    )
    .await
    .unwrap();
    assert_eq!(
        paykit_lib::scopes::check_certificate(app.cert()).is_ok(),
        lists_read
    );
    app
}

async fn reads_report(original_lists_read: bool, publisher_lists_read: bool) -> bool {
    let w = World::new();
    let original = party(&w, "original", AppScopes::new(), original_lists_read).await;
    let mut publisher = party(
        &w,
        "publisher",
        AppScopes::new().recovery(),
        publisher_lists_read,
    )
    .await;
    let mut reader = w
        .join(
            &w.alice,
            "reader",
            AppScopes::new().recovery().build().unwrap(),
        )
        .await;
    declare_loss(&w);
    follow_loss(&mut reader).await;
    publisher.refresh_identity().await.unwrap();
    publisher.follow_provider().await.unwrap();

    let opening = reader.app_mut().recovery_roster().await.unwrap().unwrap();
    let commitments = RecoveryCommitments::from_evidence(
        &[paykit_lib::recovery::RecoveryWitness::Identity(
            original.own_identity(),
        )],
        &[],
        &[],
    )
    .unwrap();
    let id = original.app_id();
    let field = commitments.signing_field(&opening, &id).unwrap();
    let signature = original
        .sign_protocol(RECOVERY_UPLOAD_LABEL, &[&field])
        .unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({
        "v": 1,
        "cutoff": b64(&commitments.cutoff),
        "inventory": b64(&commitments.inventory),
        "dispositions": b64(&commitments.dispositions),
        "signature": b64(&signature),
        "author_certificate": original.cert().signed(),
    }))
    .unwrap();
    let family = Collection::RecoveryUploads;
    let target = RecordTarget::new(
        SwitchCollection::new(family.name(), family.fixed_policy()),
        &lp(&[&id, &opening.hash()]),
    );
    publisher
        .put_record(&target, &payload, vec![])
        .await
        .unwrap();
    let (record, applied, operation) = reader
        .app_mut()
        .read_record_application(&target)
        .await
        .unwrap()
        .unwrap();
    assert!(record.authenticated && !record.flagged && !record.not_taken);
    assert_eq!(&operation.cert, publisher.cert().signed());
    let written = record.written_at_ms.unwrap();
    for cert in [original.cert(), publisher.cert()] {
        assert!(reader
            .app()
            .own_identity()
            .counts_at(&cert.app_id(), &cert.hash(), written));
    }
    let members = reader.recovery_members().await.unwrap().unwrap();
    assert!(members.originals.contains(&id));
    match members.reports.as_slice() {
        [] => false,
        [report] => {
            assert_eq!(report.original, id);
            assert_eq!(report.publisher, publisher.app_id());
            assert_eq!(report.application, applied);
            assert_eq!(report.inventory, commitments.inventory);
            true
        }
        reports => panic!("expected at most one report, got {reports:?}"),
    }
}

#[tokio::test]
async fn test_recovery_reports_accepts_a_relay_for_a_contacts_only_original() {
    assert!(Box::pin(reads_report(true, true)).await);
}

#[tokio::test]
async fn test_recovery_reports_omits_an_original_missing_a_required_permission() {
    assert!(!Box::pin(reads_report(false, true)).await);
}

#[tokio::test]
async fn test_recovery_reports_omits_a_publisher_missing_a_required_permission() {
    assert!(!Box::pin(reads_report(true, false)).await);
}
