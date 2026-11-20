//! The rig's guard on `POST /rig/world`: only this run's tests can make it enrol.

use pubky_switch_client::transport::{Header, Transport};
use wasm_bindgen_test::wasm_bindgen_test;

use crate::switch_rig::{request, rig_token, transport};

/// The rig refuses `POST /rig/world` with `403`, before it enrols anything, without the run's
/// token and with a wrong token of the same length. Every other test here enrols its world
/// with the right token.
#[wasm_bindgen_test]
async fn test_switch_flow_rig_world_refuses_a_missing_or_wrong_token() {
    let token = rig_token();
    let (head, last) = token.split_at(token.len().saturating_sub(1));
    let wrong = format!("{head}{}", if last == "0" { "1" } else { "0" });
    for (case, path) in [
        ("no token", "/rig/world".to_owned()),
        ("a wrong token", format!("/rig/world?token={wrong}")),
    ] {
        let mut post = request("POST", &path);
        post.headers
            .push(Header::new("content-type", "application/json"));
        post.body = br#"{"identities": ["payer"], "apps": []}"#.to_vec();
        let response = transport()
            .send(post)
            .await
            .expect("the rig answers /rig/world");
        assert_eq!(response.status, 403, "{case}");
    }
}
