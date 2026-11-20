//! The rig's fault route (`rig/switch-rig.mjs`): one fault armed for a request the rig
//! forwards to the service (`/rig/fault`). The rig's counts per path (`/rig/hits`) are
//! `switch_rig`'s.

use std::future::Future;

use serde_json::json;

use crate::switch_rig::{disarm_fault, rig_json, rig_token};

/// What an armed fault does to the request it fires on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// The rig forwards the request, reads the service's whole answer, then closes the
    /// browser's connection without a byte of it: the service acted, and the browser hears
    /// nothing.
    Lose,
    /// The rig closes the browser's connection at once and forwards nothing.
    Refuse,
    /// The rig forwards the request once `ms` milliseconds (1 to 30000) have passed, or never
    /// when the browser closes its connection first.
    Hold {
        /// How long the rig holds the request, in milliseconds.
        ms: u32,
    },
}

impl FaultKind {
    /// The kind's name in a `/rig/fault` body.
    fn name(self) -> &'static str {
        match self {
            Self::Lose => "lose",
            Self::Refuse => "refuse",
            Self::Hold { .. } => "hold",
        }
    }
}

/// A fault for the rig to arm: `kind`, on the forwarded `method` request to exactly `path`
/// (no query), once `after` such requests went through unchanged.
#[derive(Clone, Debug)]
pub struct Fault {
    /// The request's method.
    pub method: &'static str,
    /// The request's raw path, without its query.
    pub path: String,
    /// What the fault does.
    pub kind: FaultKind,
    /// How many matching requests go through before it fires, at most 16.
    pub after: u8,
}

/// What the rig reports about its fault (`GET /rig/fault`).
struct FaultState {
    /// Whether a fault is armed.
    armed: bool,
    /// How many faults fired since the rig started.
    fired: u64,
    /// How many requests repeated the one the last fault fired on, since it was armed: the
    /// browser's own resends of it.
    resent: u64,
}

/// What the rig reports about its fault now.
async fn fault_state() -> FaultState {
    let state = rig_json("GET", &format!("/rig/fault?token={}", rig_token()), None).await;
    FaultState {
        armed: state["armed"]
            .as_bool()
            .expect("the rig says whether one is armed"),
        fired: state["fired"]
            .as_u64()
            .expect("the rig counts the fired faults"),
        resent: state["resent"]
            .as_u64()
            .expect("the rig counts the resends of the faulted request"),
    }
}

/// Arms `fault` on the rig, replacing any armed one.
async fn arm(fault: &Fault) {
    let mut body = json!({
        "method": fault.method,
        "path": fault.path,
        "kind": fault.kind.name(),
        "after": fault.after,
    });
    if let FaultKind::Hold { ms } = fault.kind {
        body["ms"] = json!(ms);
    }
    let armed = rig_json(
        "POST",
        &format!("/rig/fault?token={}", rig_token()),
        Some(body),
    )
    .await;
    assert_eq!(armed, json!({ "armed": true }));
}

/// Runs `call` with `fault` armed, and returns its output. Any fault left armed, such as by a
/// test whose call panicked, is disarmed first, and `fault` is disarmed before the check, so
/// the rig is left clean for the next test either way.
///
/// # Panics
///
/// When the fault did not fire exactly once during `call`, or the browser sent the request it
/// fired on again on its own, which would hide the fault.
pub async fn with_fault<T>(fault: &Fault, call: impl Future<Output = T>) -> T {
    disarm_fault().await;
    let before = fault_state().await;
    arm(fault).await;
    let output = call.await;
    let after = fault_state().await;
    disarm_fault().await;
    assert!(
        !after.armed && after.fired == before.fired + 1,
        "the {:?} fault on {} {} fires once: armed {}, fired {} before and {} after",
        fault.kind,
        fault.method,
        fault.path,
        after.armed,
        before.fired,
        after.fired
    );
    assert_eq!(
        after.resent, 0,
        "the browser resent the faulted request on its own: the {:?} fault on {} {}",
        fault.kind, fault.method, fault.path
    );
    output
}
