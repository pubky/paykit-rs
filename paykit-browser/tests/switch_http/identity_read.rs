//! The service's public identity read (Switch wire 7.4.15) answers with a `cache-control`
//! that lets a cache keep it for a minute; the transport's `cache: "no-store"` sends every
//! read to the service all the same.

use std::sync::Arc;

use paykit_browser::clock::JsClock;
use pubky_switch_client::transport::Transport;
use pubky_switch_client::{Credentials, SwitchClient};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::switch_rig::{account_path, disarm_fault, origin, path_hits, request, transport};
use crate::switch_world::{SwitchWorld, PAYER};

/// The service answers the payer's public identity read, a plain `GET` of
/// `/v1/<Pubky ID>/identity` sent here through the rig's transport as it is, with
/// `max-age=60` in its `cache-control`. Then two public reads of the payer's
/// `identity.json` through the Switch client, on a `FetchTransport`, both return the document
/// the payer's signer published, and the rig forwarded two requests to the read's path:
/// neither came from the browser's cache. The read carries no Switch header, so it needs no
/// preflight; the preflights the rig saw for the path are printed. A rig fault left armed
/// would make the rig close each connection after its answer, so it is disarmed first.
#[wasm_bindgen_test]
async fn test_identity_read_reaches_the_service_on_every_call() {
    disarm_fault().await;
    let w = SwitchWorld::enrol(&[PAYER], &[]).await;
    let doc = w.identity_doc(PAYER);
    let inbox = doc.inbox();
    assert_eq!(inbox.url, origin(), "the payer's switch");
    let identity = w.identity(PAYER);
    let path = account_path(&identity.public(), "identity");

    let answer = transport()
        .send(request("GET", &path))
        .await
        .unwrap_or_else(|error| panic!("the identity read: {error:?}"));
    assert_eq!(answer.status, 200, "the identity read");
    let cache_control = answer.header("cache-control").unwrap_or_default();
    assert!(
        cache_control
            .split(',')
            .any(|part| part.trim() == "max-age=60"),
        "the answer's cache-control is {cache_control:?}"
    );

    let client = SwitchClient::new(
        &inbox.url,
        identity.public(),
        Credentials::Signer {
            identity: identity.public(),
            key: Arc::new(identity.key().clone()),
        },
        Some(inbox.service_key),
        Arc::new(transport()),
        Arc::new(JsClock),
    )
    .expect("the client is made");
    let before = path_hits(&path, None).await;
    for read in 1..=2 {
        let public = client
            .public_identity()
            .await
            .unwrap_or_else(|error| panic!("public read {read}: {error:?}"));
        assert_eq!(public.identity.hash(), doc.hash(), "public read {read}");
    }
    let after = path_hits(&path, None).await;

    console_log!(
        "[identity-read] two public reads: forwarded {} -> {}, preflights {} -> {}, \
         cache-control {cache_control:?}",
        before.forwarded,
        after.forwarded,
        before.preflights,
        after.preflights
    );
    assert_eq!(
        after.forwarded - before.forwarded,
        2,
        "a read did not reach the service"
    );
}
