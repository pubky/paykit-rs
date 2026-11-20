//! `switch_reach`: one request to the rig's origin through `FetchTransport`.

use pubky_switch_client::transport::{Header, Transport};
use serde_json::{json, Value};

use crate::failure::{ensure, Args, Failure, Outcome};
use crate::switch_rig::wait::now_ms;
use crate::switch_rig::{fresh_tag, request, transport};

/// The longest tag, in bytes.
const MAX_TAG_LEN: usize = 64;

/// `switch_reach`, with the argument `switch`, the rig's origin, and optionally `tag`, 1 to
/// 64 ASCII letters, digits or `-`, which the rig counts the request under
/// (`/rig/count?tag=<tag>`); without it, a tag no other request used.
///
/// Sends one `POST /rig/echo?tag=<tag>` through the loopback test transport
/// (`FetchTransport::allowing_localhost_for_loopback_tests`), with a Switch request header
/// and a JSON body, as a Switch call carries: from another origin, the browser preflights it.
/// It checks that the rig answers 200 and echoes the method and path sent.
///
/// Returns `{"status": 200, "tag", "elapsed_ms"}`. A `fetch` the page's policy blocks fails
/// it with the code `switch`.
pub async fn run(args: &Args) -> Outcome {
    args.check_switch()?;
    let tag = match args.optional_text("tag")? {
        Some(tag) => {
            let allowed = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-';
            if !(1..=MAX_TAG_LEN).contains(&tag.len()) || !tag.bytes().all(allowed) {
                return Err(Failure::bad_args(
                    "tag must be 1 to 64 ASCII letters, digits or '-'",
                ));
            }
            tag.to_owned()
        }
        None => fresh_tag("reach"),
    };

    let path = format!("/rig/echo?tag={tag}");
    let mut call = request("POST", &path);
    let started = now_ms();
    call.headers
        .push(Header::new("switch-timestamp", &started.to_string()));
    call.headers
        .push(Header::new("content-type", "application/json"));
    call.body = b"{}".to_vec();
    let response = transport()
        .send(call)
        .await
        .map_err(|error| Failure::transport("POST /rig/echo", &error))?;
    let elapsed_ms = now_ms() - started;
    ensure!(
        response.status == 200,
        "/rig/echo answers {}",
        response.status
    );
    let echo: Value = serde_json::from_slice(&response.body)
        .map_err(|_| Failure::assertion("/rig/echo answers JSON"))?;
    ensure!(echo["method"] == "POST", "/rig/echo echoes the method");
    ensure!(echo["path"] == path.as_str(), "/rig/echo echoes the path");
    Ok(json!({ "status": response.status, "tag": tag, "elapsed_ms": elapsed_ms }))
}
