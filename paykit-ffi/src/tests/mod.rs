use crate::*;
use pubky_switch_core::{records::AuthRequest, sig::SignedDoc};
use pubky_switch_harness::{Backend, World};
use std::{error::Error, sync::Arc};

mod method;
#[path = "../../tests/hosts/signer.rs"]
mod signer;
mod storage;
mod wallet;

type TestResult = Result<(), Box<dyn Error>>;

async fn approval(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let open = || WalletEnrollment::new(Arc::new(storage::DeviceStore::open(root.path())));
    let mut buyer = signer::serving(world, "buyer").await?;
    let mut other = signer::serving(world, "other").await?;
    let buyer_identity = world.identity("buyer")?.public();
    let other_identity = world.identity("other")?.public();
    let host = open();
    let reference = vec![1; 16];
    let first = host.begin_enrollment(reference.clone()).await?;
    let auth = AuthRequest {
        client_id: "wallet.example".into(),
        client_key: [8; 32],
        caps: format!("{}:rw", first.app_record_path),
        secret: [9; 32],
    };
    let prepare = async |host: &WalletEnrollment, reference: Vec<u8>, auth: &AuthRequest| {
        host.prepare_enrollment(
            reference,
            auth.client_id.clone(),
            auth.client_key.into(),
            auth.caps.clone(),
            auth.secret.into(),
        )
        .await
    };
    let original = prepare(&host, reference.clone(), &auth).await?;
    drop(host);
    let host = open();
    let recovered = prepare(&host, reference.clone(), &auth).await?;
    assert_eq!(first.app_id, recovered.app_id);
    assert_eq!(original.signed_request, recovered.signed_request);
    let document: SignedDoc = serde_json::from_slice(&original.signed_request)?;
    let restored = buyer.review(&document, &auth)?;
    let accepted = buyer.approve(restored).await?;
    let other_ref = vec![2; 16];
    let other_info = host.begin_enrollment(other_ref.clone()).await?;
    let other_auth = AuthRequest {
        caps: format!("{}:rw", other_info.app_record_path),
        ..auth.clone()
    };
    let wrong = prepare(&host, other_ref, &other_auth).await?;
    let separate_doc = serde_json::from_slice(&wrong.signed_request)?;
    let separate = other.review(&separate_doc, &other_auth)?;
    let separate = other.approve(separate).await?;
    assert!(matches!(
        host.accept_approval(reference.clone(), other_identity.into(), separate.claim)
            .await,
        Err(WalletError::ApprovalRefused)
    ));
    other.claim_delivered(&separate.request).await?;
    host.accept_approval(
        reference.clone(),
        buyer_identity.into(),
        accepted.claim.clone(),
    )
    .await?;
    buyer.claim_delivered(&accepted.request).await?;
    let alternate = other.review(&document, &auth)?;
    let alternate = other.approve(alternate).await?;

    let failed = root.path().join("hold-release");
    std::fs::write(&failed, [])?;
    open()
        .accept_approval(
            reference.clone(),
            buyer_identity.into(),
            accepted.claim.clone(),
        )
        .await?;
    assert!(matches!(
        open().begin_enrollment(reference.clone()).await,
        Err(WalletError::Storage {
            failure: StorageError::InUse
        })
    ));
    std::fs::remove_file(failed)?;
    let reopened = open();
    assert!(matches!(
        reopened
            .accept_approval(reference.clone(), other_identity.into(), alternate.claim)
            .await,
        Err(WalletError::EnrollmentConflict)
    ));
    reopened
        .accept_approval(reference, buyer_identity.into(), accepted.claim)
        .await?;
    Ok(())
}

fn run(backend: Backend) {
    let thread = std::thread::Builder::new()
        .stack_size(1_835_008)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                if let Some(world) = World::start(backend).unwrap() {
                    let result = Box::pin(approval(&world)).await;
                    world.close().await.unwrap();
                    result.unwrap();
                }
            });
        })
        .unwrap();
    thread.join().unwrap();
}

#[test]
fn test_enrollment_keeps_the_selected_approval_across_reopen_on_scripted() {
    run(Backend::Scripted);
}
#[test]
fn test_enrollment_keeps_the_selected_approval_across_reopen_on_postgresql() {
    run(Backend::Service);
}
