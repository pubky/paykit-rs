//! How a scenario fails ([`Failure`], with its stable code), and reading its arguments
//! ([`Args`]).

use std::fmt;

use paykit_browser::store::OpenError;
use paykit_sdk::{MessageDelivery, PayOutcome, PaykitError};
use pubky_switch_client::app::AppError;
use pubky_switch_client::{StoreError, TransportError};
use serde_json::{Map, Value};

use crate::switch_rig::{try_origin, try_port};

/// Why a scenario failed: a stable code and a text. The text crosses to JavaScript and into
/// test logs, so it names only the step or the check that failed. An error of the SDK, the
/// Switch client, the transport, the rig or a store appears in it only as the names of its
/// variants, with the switch's error code when it has one, never with the error's own text or
/// what a variant carries; a result of the SDK appears only as the name of its state or
/// variant, or as a count.
#[derive(Debug)]
pub struct Failure {
    code: &'static str,
    text: String,
}

impl Failure {
    /// The arguments are not what the scenario takes.
    pub fn bad_args(text: impl Into<String>) -> Self {
        Self::new("bad_args", text)
    }

    /// No scenario has the name asked for.
    pub fn unknown_scenario() -> Self {
        Self::new("unknown_scenario", "no scenario has this name")
    }

    /// The bundle was built without a setting the scenario needs.
    pub fn build(text: impl Into<String>) -> Self {
        Self::new("build", text)
    }

    /// The rig refused or failed to enrol a world.
    pub fn rig(text: impl Into<String>) -> Self {
        Self::new("rig", text)
    }

    /// A call through the transport or the SDK failed.
    pub fn switch(text: impl Into<String>) -> Self {
        Self::new("switch", text)
    }

    /// A store, or the host's test key, could not be opened, read, written or destroyed.
    pub fn store(text: impl Into<String>) -> Self {
        Self::new("store", text)
    }

    /// A check of the scenario did not hold.
    pub fn assertion(text: impl Into<String>) -> Self {
        Self::new("assertion", text)
    }

    /// The SDK call at `step` failed with `error`: a `switch` failure that names the step and
    /// the error's variants only ([`sdk_kind`]).
    pub fn sdk(step: &str, error: &PaykitError) -> Self {
        Self::switch(format!("{step}: {}", sdk_kind(error)))
    }

    /// The transport call at `step` failed with `error`: a `switch` failure that names the step
    /// and the error's variant only.
    pub fn transport(step: &str, error: &TransportError) -> Self {
        Self::switch(format!("{step}: {}", transport_kind(error)))
    }

    /// The store call at `step` failed with `error`: a `store` failure that names the step and
    /// the error's variant only.
    pub fn store_call(step: &str, error: &StoreError) -> Self {
        Self::store(format!("{step}: {}", store_kind(error)))
    }

    /// Opening or destroying a store at `step` failed with `error`: a `store` failure that
    /// names the step and the error's variant only.
    pub fn store_open(step: &str, error: &OpenError) -> Self {
        Self::store(format!("{step}: {}", open_kind(error)))
    }

    fn new(code: &'static str, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.text)
    }
}

/// The variants of `error`, outermost first, without what they carry, and the switch's error
/// code (Switch wire 7.6) when the failure is an answer of the switch: for example
/// `Client::Switch (not_admitted)`.
pub fn sdk_kind(error: &PaykitError) -> String {
    let variant = match error {
        PaykitError::Client(error) => format!("Client::{}", app_kind(error)),
        PaykitError::Lib(_) => "Lib".to_owned(),
        PaykitError::Rail(_) => "Rail".to_owned(),
        PaykitError::Homeserver(error) => format!("Homeserver::{}", transport_kind(error)),
        PaykitError::Refused(_) => "Refused".to_owned(),
        PaykitError::Conflict(_) => "Conflict".to_owned(),
        _ => "another PaykitError".to_owned(),
    };
    match error.code() {
        Some(code) => format!("{variant} ({})", code.as_str()),
        None => variant,
    }
}

/// The variant of `error`, and of the error it wraps for the transport and the store.
fn app_kind(error: &AppError) -> String {
    let kind = match error {
        AppError::ExpiredUnsent => "ExpiredUnsent",
        AppError::Switch(_) => "Switch",
        AppError::Transport(error) => return format!("Transport::{}", transport_kind(error)),
        AppError::Store(error) => return format!("Store::{}", store_kind(error)),
        AppError::Protocol(_) => "Protocol",
        AppError::InvalidData(_) => "InvalidData",
        AppError::Alarm(_) => "Alarm",
        AppError::Refused(_) => "Refused",
        AppError::NotAnsweringAsAccount { .. } => "NotAnsweringAsAccount",
        AppError::Kept { error, .. } => return format!("Kept::{}", app_kind(error)),
        AppError::Ahead { error, .. } => return format!("Ahead::{}", app_kind(error)),
        _ => "another AppError",
    };
    kind.to_owned()
}

/// The variant of `error`.
fn transport_kind(error: &TransportError) -> &'static str {
    match error {
        TransportError::FirstSendUnavailable => "FirstSendUnavailable",
        TransportError::FirstSendExpired => "FirstSendExpired",
        TransportError::Refused(_) => "Refused",
        TransportError::Connect(_) => "Connect",
        TransportError::Timeout => "Timeout",
        TransportError::TooLarge(_) => "TooLarge",
        TransportError::InvalidRequest(_) => "InvalidRequest",
        TransportError::UnreadableSignedHeader { .. } => "UnreadableSignedHeader",
        TransportError::Other(_) => "Other",
        _ => "another TransportError",
    }
}

/// The variant of `error`.
fn store_kind(error: &StoreError) -> &'static str {
    match error {
        StoreError::InUse => "InUse",
        StoreError::NotOwner => "NotOwner",
        StoreError::Unavailable => "Unavailable",
        StoreError::Unknown => "Unknown",
        StoreError::Full => "Full",
        StoreError::Corrupt => "Corrupt",
        StoreError::TooLarge { .. } => "TooLarge",
        StoreError::Other(_) => "Other",
        _ => "another StoreError",
    }
}

/// The variant of `error`.
fn open_kind(error: &OpenError) -> &'static str {
    match error {
        OpenError::InvalidName => "InvalidName",
        OpenError::NotDedicatedWorker => "NotDedicatedWorker",
        OpenError::InUse => "InUse",
        OpenError::Corrupt => "Corrupt",
        OpenError::NewerFormat { .. } => "NewerFormat",
        OpenError::UnsealedFormat => "UnsealedFormat",
        OpenError::KeyLost => "KeyLost",
        OpenError::Unavailable { .. } => "Unavailable",
        _ => "another OpenError",
    }
}

/// The variant of `outcome`, without what it carries.
pub fn outcome_kind(outcome: &PayOutcome) -> &'static str {
    match outcome {
        PayOutcome::Paid { .. } => "Paid",
        PayOutcome::PaidUnreported { .. } => "PaidUnreported",
        PayOutcome::Failed { .. } => "Failed",
        PayOutcome::Pending { .. } => "Pending",
        PayOutcome::Uncertain { .. } => "Uncertain",
        PayOutcome::PaidOutcomeUnwritten { .. } => "PaidOutcomeUnwritten",
        PayOutcome::RecordHeld { .. } => "RecordHeld",
        PayOutcome::RecordDivergent { .. } => "RecordDivergent",
        _ => "another PayOutcome",
    }
}

/// The variant of `delivery`, without what it carries.
pub fn delivery_kind(delivery: &MessageDelivery) -> &'static str {
    match delivery {
        MessageDelivery::Kept { .. } => "Kept",
        MessageDelivery::Queued { .. } => "Queued",
        MessageDelivery::WaitingForAdmission { .. } => "WaitingForAdmission",
        MessageDelivery::CertificateNotCurrent { .. } => "CertificateNotCurrent",
        MessageDelivery::Accepted { .. } => "Accepted",
        MessageDelivery::Failed { .. } => "Failed",
        MessageDelivery::Other(_) => "Other",
        _ => "another MessageDelivery",
    }
}

/// What a scenario returns: its result as a JSON object, or why it failed.
pub type Outcome = Result<Value, Failure>;

/// Fails the scenario with an `assertion` failure, `format!`ted from the rest, unless the
/// condition holds.
macro_rules! ensure {
    ($holds:expr, $($text:tt)+) => {
        if !$holds {
            return Err($crate::failure::Failure::assertion(format!($($text)+)));
        }
    };
}
pub(crate) use ensure;

/// A scenario's arguments: a JSON object.
pub struct Args(Map<String, Value>);

impl Args {
    /// Reads `json`, which must be a JSON object.
    pub fn parse(json: &str) -> Result<Self, Failure> {
        match serde_json::from_str(json) {
            Ok(Value::Object(map)) => Ok(Self(map)),
            _ => Err(Failure::bad_args("the arguments are not a JSON object")),
        }
    }

    /// The text member `name`.
    pub fn text(&self, name: &str) -> Result<&str, Failure> {
        self.0
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::bad_args(format!("the text argument {name} is missing")))
    }

    /// The text member `name`, or `None` when it is missing.
    pub fn optional_text(&self, name: &str) -> Result<Option<&str>, Failure> {
        match self.0.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .map(Some)
                .ok_or_else(|| Failure::bad_args(format!("{name} is not text"))),
        }
    }

    /// The bytes member `name`, base64url without padding (Switch wire 1.2), of exactly `N`
    /// bytes.
    pub fn bytes<const N: usize>(&self, name: &str) -> Result<[u8; N], Failure> {
        pubky_switch_core::enc::unb64_array(self.text(name)?)
            .map_err(|_| Failure::bad_args(format!("{name} is not {N} bytes in base64url")))
    }

    /// The run's identifier, `run`: 1 to 32 ASCII letters, digits, `-` or `_`, so the names
    /// of its stores are valid store names.
    pub fn run(&self) -> Result<&str, Failure> {
        let run = self.text("run")?;
        let allowed = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_';
        if (1..=32).contains(&run.len()) && run.bytes().all(allowed) {
            Ok(run)
        } else {
            Err(Failure::bad_args(
                "run must be 1 to 32 ASCII letters, digits, '-' or '_'",
            ))
        }
    }

    /// Checks the `switch` member: the Switch origin the page means, which must be the rig's
    /// origin the bundle was built with, since the rig's token and the transport's allowed
    /// port come from the same build. Also checks that the build's origin names a port.
    pub fn check_switch(&self) -> Result<(), Failure> {
        let asked = self.text("switch")?;
        let built = try_origin().map_err(Failure::build)?;
        if asked != built {
            return Err(Failure::bad_args(
                "switch is not the rig origin the bundle was built with",
            ));
        }
        try_port().map_err(Failure::build)?;
        Ok(())
    }
}
