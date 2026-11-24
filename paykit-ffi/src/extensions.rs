//! The spam count and extension messages (Paykit spec 3.3, 8.1).

use crate::{fixed, NativeWallet, WalletError};
use paykit_lib::kinds::ExtensionKind;
use serde_json::{Map, Value};
use std::fmt;

/// An extension message kept for the host until acknowledged or evicted (Paykit spec 3.3).
#[derive(uniffi::Record)]
pub struct ExtensionMessage {
    /// The sender's 32-byte identity (Switch wire 5.1).
    pub sender: Vec<u8>,
    /// The sender's 16-byte App ID (Switch wire 5.1).
    pub sender_app: Vec<u8>,
    /// The extension kind `<prefix>.<name>` (Paykit wire 4.3).
    pub kind: String,
    /// The signed message ID (Switch wire 5.1).
    pub msg_id: String,
    /// The sender's creation time in Unix milliseconds (Switch spec 5.3).
    pub created_at_ms: u64,
    /// The inbox's acceptance time in Unix milliseconds (Switch spec 8.2).
    pub accepted_at_ms: u64,
    /// The generation of the verified inbox entry (Switch spec 8.2).
    pub generation: u64,
    /// The entry's sequence number within its generation (Switch spec 8.2).
    pub seq: u64,
    /// Base64url message hashes in the extension's order (Paykit wire 4.3).
    pub refs: Vec<String>,
    /// The extension's body as a JSON object in text form (Paykit wire 4.3).
    pub body_json: String,
}

impl From<paykit_sdk::ExtensionMessage> for ExtensionMessage {
    fn from(message: paykit_sdk::ExtensionMessage) -> Self {
        Self {
            sender: message.sender.into(),
            sender_app: message.sender_app.into(),
            kind: message.kind,
            msg_id: message.msg_id,
            created_at_ms: message.created_at_ms,
            accepted_at_ms: message.accepted_at_ms,
            generation: message.generation,
            seq: message.seq,
            refs: message.refs,
            body_json: Value::Object(message.body).to_string(),
        }
    }
}

impl fmt::Debug for ExtensionMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionMessage")
            .field("sender", &self.sender)
            .field("sender_app", &self.sender_app)
            .field("kind", &self.kind)
            .field("msg_id", &self.msg_id)
            .field("created_at_ms", &self.created_at_ms)
            .field("accepted_at_ms", &self.accepted_at_ms)
            .field("generation", &self.generation)
            .field("seq", &self.seq)
            .field("refs", &self.refs)
            .field("body_json", &self.body_json.len())
            .finish()
    }
}

#[uniffi::export]
impl NativeWallet {
    /// Read the number of messages suppressed by the spam rule for this sender (Paykit wire 4.1).
    pub async fn spam_count(&self, sender: Vec<u8>) -> Result<u64, WalletError> {
        let sender = fixed(sender, "sender")?;
        self.with_wallet(move |app| Box::pin(async move { Ok(app.spam_count(&sender).await?) }))
            .await
    }

    /// List the received extension messages still kept for the host (Paykit spec 3.3).
    pub async fn extension_messages(&self) -> Result<Vec<ExtensionMessage>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .extension_messages()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }

    /// Remove a handled extension message by sender and message ID. Acknowledging an absent
    /// message succeeds (Paykit spec 3.3).
    pub async fn acknowledge_extension_message(
        &self,
        sender: Vec<u8>,
        msg_id: String,
    ) -> Result<(), WalletError> {
        let sender = fixed(sender, "sender")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.acknowledge_extension_message(&sender, &msg_id).await?) })
        })
        .await
    }

    /// Send an extension message with ordered 32-byte reference hashes and a JSON object body.
    /// Return its signed message ID once queued (Paykit spec 3.3, Paykit wire 4.3).
    pub async fn send_extension(
        &self,
        to: Vec<u8>,
        kind: String,
        refs: Vec<Vec<u8>>,
        body_json: String,
    ) -> Result<String, WalletError> {
        let to = fixed(to, "to")?;
        let kind = ExtensionKind::new(&kind).map_err(|_| WalletError::invalid("kind"))?;
        let refs = refs
            .into_iter()
            .map(|hash| fixed(hash, "refs"))
            .collect::<Result<_, _>>()?;
        let body = extension_body(&kind, &body_json)?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.send_extension(to, &kind, refs, body).await?) })
        })
        .await
    }
}

fn extension_body(
    kind: &ExtensionKind,
    body_json: &str,
) -> Result<Map<String, Value>, WalletError> {
    // The message context lets the Switch apply Paykit's opaque metadata rule (Paykit wire 4.3).
    let kind = kind.as_string();
    crate::opaque::object(
        body_json.as_bytes(),
        crate::opaque::Place::ExtensionBody { kind: &kind },
    )
    .ok_or_else(|| WalletError::invalid("body_json"))
}
