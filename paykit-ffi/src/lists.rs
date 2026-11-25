//! Sharing and receiving Private Payment Lists (Paykit spec 8.4.1).

use crate::{fixed, NativeWallet, PaymentRefusal, WalletError};
use paykit_lib::records::ListPublicationRecord as Publication;
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use paykit_sdk::PublicationConflict;
use paykit_sdk::Refusal;
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_core::enc::b64;
use serde_json::{json, Value};
use std::sync::Arc;
use std::{collections::BTreeMap, fmt};

/// A reusable Payment Endpoint in a Private Payment List (Paykit wire 4.13).
#[derive(uniffi::Record)]
pub struct ListEndpoint {
    /// The Payment Endpoint Identifier (Paykit wire 1.4).
    pub identifier: String,
    /// The Payment Endpoint Payload supplied by the payee (Paykit wire 1.4).
    pub payload: String,
}

impl fmt::Debug for ListEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListEndpoint")
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

/// Validate the endpoint pairs before installing the reusable list (Paykit wire 1.4, 4.13).
pub(crate) fn list_endpoints(
    endpoints: Vec<ListEndpoint>,
) -> Result<BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>, WalletError> {
    let mut values = BTreeMap::new();
    for endpoint in endpoints {
        let identifier = PaymentEndpointIdentifier::new(&endpoint.identifier)
            .map_err(|_| WalletError::invalid("identifier"))?;
        let payload = PaymentEndpointPayload::new(&endpoint.payload)
            .map_err(|_| WalletError::invalid("payload"))?;
        if values.insert(identifier, payload).is_some() {
            return Err(WalletError::invalid("identifier"));
        }
    }
    Ok(values)
}

/// The ordered version of a Private Payment List (Paykit wire 4.13).
#[derive(uniffi::Record)]
pub struct ListVersion {
    /// The publishing identity's generation (Paykit wire 4.13).
    pub generation: u64,
    /// The publishing app's reservation counter for this recipient (Paykit spec 8.4.1).
    pub counter: u64,
}

/// A Private Payment List the payer holds until it can place it in its record (Paykit wire 5.4).
#[derive(uniffi::Record)]
pub struct HeldList {
    /// The payee's identity (Paykit spec 8.4).
    pub payee: Vec<u8>,
    /// The App ID that published the list (Paykit spec 8.4.1).
    pub app_id: Vec<u8>,
    /// The version reserved for the held list (Paykit wire 4.13).
    pub version: ListVersion,
    /// The held list's message ID (Paykit wire 4.13).
    pub msg_id: String,
}

impl From<paykit_sdk::HeldList> for HeldList {
    fn from(list: paykit_sdk::HeldList) -> Self {
        Self {
            payee: list.payee.into(),
            app_id: list.app_id.into(),
            version: ListVersion {
                generation: list.version.generation(),
                counter: list.version.counter(),
            },
            msg_id: list.msg_id,
        }
    }
}

/// A publishing app's reservation for a Private Payment List (Paykit wire 5.8).
#[derive(Clone, uniffi::Record)]
pub struct ListPublicationRecord {
    /// The publishing App ID (Paykit wire 5.8).
    pub publisher: Vec<u8>,
    /// The recipient identity (Paykit wire 5.8).
    pub recipient: Vec<u8>,
    /// The publishing identity's generation (Paykit wire 5.8).
    pub generation: u64,
    /// The counter reserved for the list (Paykit wire 5.8).
    pub counter: u64,
    /// The reserved list's message ID (Paykit wire 5.8).
    pub msg_id: String,
    /// The reserved list's message hash (Paykit wire 5.8).
    pub message_hash: Vec<u8>,
}

impl From<Publication> for ListPublicationRecord {
    fn from(record: Publication) -> Self {
        Self {
            publisher: record.publisher.into(),
            recipient: record.recipient.into(),
            generation: record.generation,
            counter: record.counter,
            msg_id: record.msg_id,
            message_hash: record.message_hash.into(),
        }
    }
}

impl ListPublicationRecord {
    fn into_wire_value(self, field: &str) -> Result<Value, WalletError> {
        let member = |name: &str| format!("{field}.{name}");
        // A reservation names a positive Switch integer for each part of its version
        // and a lower-case UUID v4 for its message (Paykit wire 5.8).
        for (name, value) in [("generation", self.generation), ("counter", self.counter)] {
            if !(1..=pubky_switch_core::json::MAX_INT).contains(&value) {
                return Err(WalletError::invalid(&member(name)));
            }
        }
        pubky_switch_core::json::check_uuid_v4(&self.msg_id)
            .map_err(|_| WalletError::invalid(&member("msg_id")))?;
        let record = Publication {
            publisher: fixed(self.publisher, &member("publisher"))?,
            recipient: fixed(self.recipient, &member("recipient"))?,
            generation: self.generation,
            counter: self.counter,
            msg_id: self.msg_id,
            message_hash: fixed(self.message_hash, &member("message_hash"))?,
            unknown: Default::default(),
        };
        let bytes = record.to_json().map_err(|_| WalletError::invalid(field))?;
        serde_json::from_slice(&bytes).map_err(|_| WalletError::invalid(field))
    }
}

/// Conflicting reservations observed at the same publication key (Paykit wire 5.8).
#[derive(Clone, uniffi::Record)]
pub struct ListPublicationConflict {
    /// Hash of the authenticated service application that showed the conflict (Paykit wire 5.8).
    pub application_hash: Vec<u8>,
    /// Whether another distinct conflicting application was observed at this key (Paykit wire 5.8).
    pub other_applications: bool,
    /// The reservation the publisher held (Paykit wire 5.8).
    pub held: ListPublicationRecord,
    /// The later reservation the publisher refused (Paykit wire 5.8).
    pub refused: ListPublicationRecord,
}

impl From<PublicationConflict> for ListPublicationConflict {
    fn from(conflict: PublicationConflict) -> Self {
        Self {
            application_hash: conflict.application_hash.into(),
            other_applications: conflict.other_applications,
            held: conflict.held.into(),
            refused: conflict.refused.into(),
        }
    }
}

impl TryFrom<ListPublicationConflict> for PublicationConflict {
    type Error = WalletError;

    fn try_from(conflict: ListPublicationConflict) -> Result<Self, Self::Error> {
        let hash = fixed::<32>(conflict.application_hash, "application_hash")?;
        // The SDK reads the exact observation back with its publication record codec.
        serde_json::from_value(json!({
            "application_hash": b64(&hash),
            "other_applications": conflict.other_applications,
            "held": conflict.held.into_wire_value("held")?,
            "refused": conflict.refused.into_wire_value("refused")?,
        }))
        .map_err(|_| WalletError::invalid("publication conflict"))
    }
}

#[uniffi::export]
impl NativeWallet {
    /// Set the reusable endpoints shared with chosen recipients (Paykit spec 8.4.1).
    pub async fn set_list_endpoints(
        &self,
        endpoints: Vec<ListEndpoint>,
    ) -> Result<(), WalletError> {
        let endpoints = list_endpoints(endpoints)?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.set_list_endpoints(endpoints).await?) })
        })
        .await
    }

    /// Share the reusable Private Payment List with the chosen recipient (Paykit spec 8.4.1).
    pub async fn publish_list_to(&self, recipient: Vec<u8>) -> Result<(), WalletError> {
        let recipient = fixed(recipient, "recipient")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.publish_list_to(recipient).await?) })
        })
        .await
    }

    /// Stop sharing the list with this recipient (Paykit spec 8.4.1).
    pub async fn withdraw_list_from(&self, recipient: Vec<u8>) -> Result<(), WalletError> {
        let recipient = fixed(recipient, "recipient")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.withdraw_list_from(recipient).await?) })
        })
        .await
    }

    /// Read Private Payment Lists held over unreadable or deleted records (Paykit wire 5.4).
    pub async fn held_lists(&self) -> Result<Vec<HeldList>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .held_lists()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }

    /// Read the publisher's outstanding reports of conflicting reservations (Paykit wire 5.8).
    pub async fn publication_conflicts(&self) -> Result<Vec<ListPublicationConflict>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .publication_conflicts()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }

    /// Acknowledge the displayed conflict, retaining the counter and any newer report (Paykit wire 5.8).
    pub async fn acknowledge_publication_conflict(
        &self,
        conflict: ListPublicationConflict,
    ) -> Result<(), WalletError> {
        let report = conflict.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.acknowledge_publication_conflict(&report).await?) })
        })
        .await
    }

    /// Send another list to a counterparty whose current list was given up with an unknown
    /// outcome, once the user confirmed it (Paykit spec 8.4, 9). `refusal` is the exact
    /// `ListDeliveryUnknown` refusal a list call returned, which the host showed. Any other
    /// refusal is `InvalidInput` naming `refusal`; a list that changed since comes back as a
    /// new `ListDeliveryUnknown`, and nothing is sent. It returns once nothing of unknown
    /// outcome is left for the counterparty: the new list is queued, or the shown one was
    /// accepted, superseded or withdrawn meanwhile, or can no longer be paid, and then nothing
    /// more is sent. `NotNow` keeps the confirmation, and a later call or receive sends the
    /// list once it can.
    pub async fn send_list_again(&self, refusal: Arc<PaymentRefusal>) -> Result<(), WalletError> {
        let Refusal::ListDeliveryUnknown(shown) = &refusal.0 else {
            return Err(WalletError::invalid("refusal"));
        };
        let shown = (**shown).clone();
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.send_list_again(&shown, UserConfirmed::new()).await?) })
        })
        .await
    }
}

/// Restore the test seller and give up its first list after the recipient's inbox accepts
/// it and the answer is lost (Paykit spec 8.4.1, 9, Switch spec 11, 12.4).
#[cfg(any(test, feature = "test-wallet"))]
pub(crate) async fn restored_list_seller(
    world: &pubky_switch_harness::World,
    host: &crate::WalletHost,
    app_id: Vec<u8>,
    backup: Vec<u8>,
    recipient: [u8; 32],
) -> Result<Arc<NativeWallet>, WalletError> {
    let seller = host
        .restore(app_id, backup, crate::BackupDevice::InPossession)
        .await?;
    // Read until the feed is done and every record the restored scopes read is listed
    // (Paykit spec 6).
    let mut listed = false;
    for _ in 0..8 {
        let report = seller.receive().await?;
        if !report.more && report.records_unlisted.is_none() {
            listed = true;
            break;
        }
    }
    if !listed {
        return Err(WalletError::invalid("restored test seller listing"));
    }
    // A restored app may list its shared records over several reconcile passes, each refused
    // as not now until the listing is done (Paykit spec 6, 7.3).
    let mut reconciled = false;
    for _ in 0..8 {
        match seller.reconcile().await {
            Ok(_) => {
                reconciled = true;
                break;
            }
            Err(WalletError::RetryLater { failure })
                if failure
                    .refusal
                    .as_ref()
                    .is_some_and(|r| matches!(r.0, paykit_sdk::Refusal::NotNow(_))) => {}
            Err(error) => return Err(error),
        }
    }
    if !reconciled {
        return Err(WalletError::invalid("restored test seller reconciliation"));
    }
    seller.end_confirmed_reconciliation().await?;
    seller
        .set_list_endpoints(vec![ListEndpoint {
            identifier: "btc-lightning-bolt11".into(),
            payload: "lnbc1example".into(),
        }])
        .await?;
    world.lose_answer("POST", "/inbox");
    seller.publish_list_to(recipient.into()).await?;
    seller
        .with_wallet(move |app| {
            Box::pin(async move {
                let list = app
                    .app()
                    .queued_messages("paykit")
                    .await?
                    .into_iter()
                    .find(|message| {
                        message.to == recipient && message.kind == "paykit.private_payment_list"
                    })
                    .ok_or_else(|| WalletError::invalid("queued test list"))?;
                if !app
                    .app_mut()
                    .withdraw_message("paykit", &list.msg_id)
                    .await?
                {
                    return Err(WalletError::invalid("queued test list"));
                }
                Ok(())
            })
        })
        .await?;
    Ok(seller)
}
