//! The original signer and the publisher of a recovery report (Paykit wire 5.10).

use std::sync::Arc;

use paykit_lib::collections::Collection;
use paykit_lib::recovery::{RecoveryCommitments, RECOVERY_UPLOAD_LABEL};
use paykit_sdk::{AppScopes, PaykitConfig};
use pubky_dispatch_client::app::{
    App, Collection as DispatchCollection, RecordApplication, RecordTarget,
};
use pubky_dispatch_client::testing::{
    issue_cert, AppSpec, ProtocolSpec, ScriptedDispatch, TestProtocol,
};
use pubky_dispatch_core::enc::{b64, lp};
use pubky_dispatch_core::ops::OpKind;
use pubky_dispatch_core::records::{RemovedApp, RenewedCert};

use crate::common::{spec, store, World};
use crate::provider_loss::{declare_loss, follow_loss};

/// Every party has the contacts scope. A certificate missing its list permission
/// still verifies under Dispatch rules (Paykit spec 3.2).
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

#[derive(Clone, Copy, Debug)]
enum Role {
    Original,
    Publisher,
}

#[derive(Clone, Copy, Debug)]
enum Change {
    Removal { millis_before_write: u64 },
    ClosedRenewal { millis_after_write: u64 },
}

/// Publish signed successors on the recovered service after the report's write.
fn change_standing(
    w: &World,
    service: &ScriptedDispatch,
    app: &App,
    name: &str,
    change: Change,
    written: u64,
) {
    let id = app.app_id();
    w.clock.advance_ms(2_000);
    let doc = match change {
        Change::Removal {
            millis_before_write,
        } => {
            let distrust_from = written - millis_before_write;
            w.alice
                .sign_next(|body| {
                    body.apps.retain(|member| member.app_id != id);
                    body.removed.push(RemovedApp {
                        app_id: id,
                        distrust_from,
                    });
                })
                .unwrap()
        }
        Change::ClosedRenewal { millis_after_write } => {
            let until = written + millis_after_write;
            let scopes: Vec<_> = app.cert().scopes().iter().map(String::as_str).collect();
            let renewed = issue_cert(
                w.alice.identity().key(),
                &w.alice.identity().app_keys(name),
                &AppSpec::new(&scopes).with_permissions(app.cert().permissions()),
                written,
            )
            .unwrap();
            service
                .signer_op(OpKind::CertPut {
                    target_app_id: id,
                    cert: renewed.signed().clone(),
                })
                .unwrap();
            let open = w
                .alice
                .sign_next(|body| {
                    body.apps
                        .iter_mut()
                        .find(|member| member.app_id == id)
                        .unwrap()
                        .cert = renewed.hash();
                    body.renewed.push(RenewedCert {
                        app_id: id,
                        cert: app.cert().hash(),
                        until: None,
                    });
                })
                .unwrap();
            service.deliver_identity(&open).unwrap();
            w.alice
                .sign_next(|body| {
                    body.renewed
                        .iter_mut()
                        .find(|entry| entry.cert == app.cert().hash())
                        .unwrap()
                        .until = Some(until);
                })
                .unwrap()
        }
    };
    service.deliver_identity(&doc).unwrap();
}

async fn reads_report(
    original_lists_read: bool,
    publisher_lists_read: bool,
    change: Option<(Role, Change)>,
) -> bool {
    let w = World::new();
    let mut original = party(&w, "original", AppScopes::new(), original_lists_read).await;
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
    let service = declare_loss(&w);
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
        DispatchCollection::new(family.name(), family.fixed_policy()),
        &lp(&[&id, &opening.hash()]),
    );
    w.clock.advance_ms(3_000);
    publisher
        .put_record(&target, &payload, vec![])
        .await
        .unwrap();
    let RecordApplication {
        record,
        application: applied,
        evidence: operation,
        ..
    } = reader
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
    let mut removed = None;
    if let Some((role, change)) = change {
        original.refresh_identity().await.unwrap();
        original.follow_provider().await.unwrap();
        let (app, name) = match role {
            Role::Publisher => (&publisher, "publisher"),
            Role::Original => (&original, "original"),
        };
        change_standing(&w, &service, app, name, change, written);
        if matches!(change, Change::Removal { .. }) {
            removed = Some(app.app_id());
        }
    }
    let members = reader.recovery_members().await.unwrap().unwrap();
    for member in [id, publisher.app_id()] {
        assert_eq!(members.removed.contains(&member), removed == Some(member));
        assert_eq!(members.originals.contains(&member), removed != Some(member));
    }
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
    assert!(Box::pin(reads_report(true, true, None)).await);
}

#[tokio::test]
async fn test_recovery_reports_omits_an_original_missing_a_required_permission() {
    assert!(!Box::pin(reads_report(false, true, None)).await);
}

#[tokio::test]
async fn test_recovery_reports_omits_a_publisher_missing_a_required_permission() {
    assert!(!Box::pin(reads_report(true, false, None)).await);
}

#[tokio::test]
async fn test_recovery_reports_removal_includes_the_cutoff_for_each_signer() {
    for role in [Role::Original, Role::Publisher] {
        assert!(
            Box::pin(reads_report(
                true,
                true,
                Some((
                    role,
                    Change::Removal {
                        millis_before_write: 0
                    }
                ))
            ))
            .await,
            "{role:?}"
        );
        assert!(
            !Box::pin(reads_report(
                true,
                true,
                Some((
                    role,
                    Change::Removal {
                        millis_before_write: 1
                    }
                ))
            ))
            .await,
            "{role:?}"
        );
    }
}

#[tokio::test]
async fn test_recovery_reports_closed_renewal_excludes_its_end_for_each_signer() {
    for role in [Role::Original, Role::Publisher] {
        assert!(
            Box::pin(reads_report(
                true,
                true,
                Some((
                    role,
                    Change::ClosedRenewal {
                        millis_after_write: 1
                    }
                ))
            ))
            .await,
            "{role:?}"
        );
        assert!(
            !Box::pin(reads_report(
                true,
                true,
                Some((
                    role,
                    Change::ClosedRenewal {
                        millis_after_write: 0
                    }
                ))
            ))
            .await,
            "{role:?}"
        );
    }
}
