//! The payee's requests, payment checks, receipts and resend requests (Paykit spec 6, 8.1, 9).

use crate::{
    fixed, lists::list_endpoints, Amount, BillingPeriod, ListEndpoint, MessageDelivery,
    NativeWallet, RequestId, Version, WalletError,
};
use paykit_lib::{
    bodies::Conversion, Asset, Decimal, PaymentEndpointIdentifier, PaymentReference, Rate,
};
use paykit_sdk::{self as sdk, UserConfirmed};
use serde_json::{Map, Value};

/// Where a one-time request may be paid (Paykit wire 4.5).
#[derive(uniffi::Enum)]
pub enum RequestDestination {
    /// Endpoints supplied by the sending app (Paykit wire 4.5).
    Endpoints {
        /// The request's identifiers and payloads (Paykit wire 1.4).
        endpoints: Vec<ListEndpoint>,
    },
    /// Endpoints from a particular current app of the payee (Paykit spec 8.1).
    RequiredApp {
        /// The required App ID (Paykit wire 4.5).
        app_id: Vec<u8>,
    },
    /// Endpoints from any current app of the payee (Paykit spec 8.1).
    Any,
}

impl TryFrom<RequestDestination> for sdk::RequestDestination {
    type Error = WalletError;

    fn try_from(destination: RequestDestination) -> Result<Self, Self::Error> {
        Ok(match destination {
            RequestDestination::Endpoints { endpoints } => {
                Self::Endpoints(list_endpoints(endpoints)?)
            }
            RequestDestination::RequiredApp { app_id } => {
                Self::RequiredApp(fixed(app_id, "app_id")?)
            }
            RequestDestination::Any => Self::Any,
        })
    }
}

/// A fixed Conversion Rate for an asset or an asset and rail (Paykit wire 4.4).
#[derive(uniffi::Record)]
pub struct ConversionRate {
    /// The asset selector (Paykit wire 4.4).
    pub asset: String,
    /// The exact positive decimal rate (Paykit wire 1.2, 4.4).
    pub value: String,
}

impl TryFrom<ConversionRate> for Rate {
    type Error = WalletError;

    fn try_from(rate: ConversionRate) -> Result<Self, Self::Error> {
        let asset = Asset::new(&rate.asset).map_err(|_| WalletError::invalid("conversion"))?;
        let value = Decimal::new(&rate.value).map_err(|_| WalletError::invalid("conversion"))?;
        Self::new(asset, value).map_err(|_| WalletError::invalid("conversion"))
    }
}

/// A one-time Payment Request. The wallet's selling label applies (Paykit spec 3.1, Paykit wire 4.5).
#[derive(uniffi::Record)]
pub struct RequestDraft {
    /// The requested amount and asset (Paykit wire 1.2).
    pub amount: Amount,
    /// Where the payer may pay (Paykit wire 4.5).
    pub destination: RequestDestination,
    /// Reference to copy into the payment where the method allows it (Paykit wire 4.5).
    pub payment_reference: String,
    /// Last time the payer may act on the proposal, in Unix milliseconds (Paykit wire 4.5).
    #[uniffi(default = None)]
    pub proposal_expires_at_ms: Option<u64>,
    /// Deadline for the payment itself, in Unix milliseconds (Paykit wire 4.4).
    #[uniffi(default = None)]
    pub payment_deadline_ms: Option<u64>,
    /// Restrict the payer to these Payment Endpoint Identifiers (Paykit wire 4.5).
    #[uniffi(default = None)]
    pub accepted_endpoint_identifiers: Option<Vec<String>>,
    /// Fixed rates for pricing endpoints in other assets (Paykit wire 4.4).
    #[uniffi(default = None)]
    pub conversion: Option<Vec<ConversionRate>>,
    /// Opaque metadata encoded as a JSON object (Paykit wire 1.1, 4.5).
    #[uniffi(default = None)]
    pub metadata_json: Option<Vec<u8>>,
    /// Invite code for delivery to the payer's Switch (Switch spec 7).
    #[uniffi(default = None)]
    pub invite_code: Option<String>,
}

fn payment_reference(value: &str) -> Result<PaymentReference, WalletError> {
    PaymentReference::new(value).map_err(|_| WalletError::invalid("payment_reference"))
}

fn timestamp(ms: u64, field: &str) -> Result<u64, WalletError> {
    if ms > pubky_switch_core::time::MAX_TIME_MS {
        return Err(WalletError::invalid(field));
    }
    Ok(ms)
}

fn metadata(bytes: &[u8]) -> Result<Map<String, Value>, WalletError> {
    use paykit_lib::kinds::OPAQUE_MEMBERS;
    use pubky_switch_core::json::{check_json_as, Doc};

    // Check the original bytes for duplicate keys before decoding the opaque object (Paykit wire 1.1).
    let message = [br#"{"body":{"metadata":"#.as_slice(), bytes, b"}}"].concat();
    check_json_as(&message, Doc::Message(OPAQUE_MEMBERS))
        .map_err(|_| WalletError::invalid("metadata_json"))?;
    serde_json::from_slice(bytes).map_err(|_| WalletError::invalid("metadata_json"))
}

impl TryFrom<RequestDraft> for sdk::RequestDraft {
    type Error = WalletError;

    fn try_from(input: RequestDraft) -> Result<Self, Self::Error> {
        let mut draft = Self::new(
            input
                .amount
                .try_into()
                .map_err(|_| WalletError::invalid("amount"))?,
            payment_reference(&input.payment_reference)?,
            input.destination.try_into()?,
        );
        if let Some(at) = input.proposal_expires_at_ms {
            draft = draft.with_proposal_expires_at_ms(timestamp(at, "proposal_expires_at_ms")?);
        }
        if let Some(at) = input.payment_deadline_ms {
            draft = draft.with_payment_deadline_ms(timestamp(at, "payment_deadline_ms")?);
        }
        if let Some(ids) = input.accepted_endpoint_identifiers {
            let ids = ids
                .into_iter()
                .map(|id| PaymentEndpointIdentifier::new(&id))
                .collect::<Result<_, _>>()
                .map_err(|_| WalletError::invalid("accepted_endpoint_identifiers"))?;
            draft = draft.with_accepted_endpoint_identifiers(ids);
        }
        if let Some(rates) = input.conversion {
            let rates = rates
                .into_iter()
                .map(Rate::try_from)
                .collect::<Result<_, _>>()?;
            draft = draft.with_conversion(Conversion::Fixed(rates));
        }
        if let Some(bytes) = input.metadata_json {
            draft = draft.with_metadata(metadata(&bytes)?);
        }
        if let Some(code) = input.invite_code {
            draft = draft.with_invite_code(code);
        }
        Ok(draft)
    }
}

/// The signed request and its delivery state (Paykit spec 8.1, Switch spec 11).
#[derive(uniffi::Record)]
pub struct SentRequest {
    /// The signed request's message ID (Switch wire 5.1).
    pub msg_id: String,
    /// Where delivery stands (Switch spec 11).
    pub delivery: MessageDelivery,
    /// The request whose record is written in the batch that queues it (Paykit spec 8.1).
    pub request: Option<RequestId>,
}

impl From<sdk::SentRequest> for SentRequest {
    fn from(sent: sdk::SentRequest) -> Self {
        Self {
            msg_id: sent.msg_id,
            delivery: sent.delivery.into(),
            request: sent.request.map(Into::into),
        }
    }
}

/// The status established by the messages in the payee's activity record (Paykit wire 5.3).
#[derive(Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PayeeStatus {
    /// The payee sent the request (Paykit spec 8.1).
    Requested,
    /// The payer accepted the request or subscribed (Paykit spec 8.1, 8.2).
    Accepted,
    /// The payer rejected the request (Paykit spec 8.1).
    Rejected,
    /// Either party cancelled the request (Paykit spec 8.1).
    Cancelled,
    /// The payee holds the proof and has yet to credit it (Paykit spec 8.1, 8.4).
    ProofReceived,
    /// The payee holds both the receipt and the proof it confirms (Paykit spec 8.1).
    Receipted,
    /// A status introduced by a later SDK (Paykit wire 5.3).
    Other,
}

impl From<sdk::PayeeStatus> for PayeeStatus {
    fn from(status: sdk::PayeeStatus) -> Self {
        match status {
            sdk::PayeeStatus::Requested => Self::Requested,
            sdk::PayeeStatus::Accepted => Self::Accepted,
            sdk::PayeeStatus::Rejected => Self::Rejected,
            sdk::PayeeStatus::Cancelled => Self::Cancelled,
            sdk::PayeeStatus::ProofReceived => Self::ProofReceived,
            sdk::PayeeStatus::Receipted => Self::Receipted,
            _ => Self::Other,
        }
    }
}

/// A sent request or a payment received without a request (Paykit spec 6, 8.1, 8.4).
#[derive(uniffi::Record)]
pub struct PayeeRequest {
    /// The payee's activity record key (Paykit wire 5.3).
    pub record: Vec<u8>,
    /// The payer identity (Paykit wire 5.3).
    pub payer: Vec<u8>,
    /// The request hash, absent for a payment without a request (Paykit wire 5.3).
    pub request_hash: Option<Vec<u8>>,
    /// The amount from the request or the proof (Paykit wire 4.5, 4.10).
    pub amount: Option<Amount>,
    /// The label of the selling scope (Paykit spec 3.1).
    pub label: Option<String>,
    /// The attempt reported by the proof (Paykit wire 4.10).
    pub attempt_id: Option<String>,
    /// The status of the request or payment (Paykit wire 5.3).
    pub status: PayeeStatus,
    /// Whether this record or one of its period records was seen divergent (Paykit spec 6).
    pub divergent: bool,
    /// Whether the request is recurring (Paykit spec 8.2).
    pub recurring: bool,
    /// Billing Periods from the records held, in period order (Paykit spec 8.2).
    pub periods: Vec<BillingPeriod>,
}

impl From<sdk::PayeeActivity> for PayeeRequest {
    fn from(entry: sdk::PayeeActivity) -> Self {
        Self {
            record: entry.record.into(),
            payer: entry.payer.into(),
            request_hash: entry.request_hash.map(Vec::from),
            amount: entry.amount.as_ref().map(Into::into),
            label: entry.label,
            attempt_id: entry.attempt_id.map(|id| id.to_text()),
            status: entry.status.into(),
            divergent: entry.divergent,
            recurring: entry.recurring,
            periods: entry
                .periods
                .into_iter()
                .map(|p| p.billing_period.into())
                .collect(),
        }
    }
}

/// The payee's choices when issuing a Payment Receipt (Paykit wire 4.11, 5.7).
#[derive(Default, uniffi::Record)]
pub struct ReceiptDetails {
    /// Store the receipt in shared activity without delivering it (Paykit spec 8.1).
    #[uniffi(default = false)]
    pub store_only: bool,
    /// Opaque seller metadata encoded as a JSON object (Paykit wire 1.1, 4.11).
    #[uniffi(default = None)]
    pub metadata_json: Option<Vec<u8>>,
    /// The payee's reference for a payment without a request (Paykit wire 4.11).
    #[uniffi(default = None)]
    pub payment_reference: Option<String>,
    /// The user accepts a possible second credit when the settlement mark cannot be written (Paykit wire 5.7).
    #[uniffi(default = false)]
    pub unmarked_credit_confirmed: bool,
    /// The user accepts that another occurrence may already hold an unseen credit (Paykit spec 8.1).
    #[uniffi(default = false)]
    pub maybe_credited_confirmed: bool,
}

impl TryFrom<ReceiptDetails> for sdk::ReceiptDetails {
    type Error = WalletError;

    fn try_from(input: ReceiptDetails) -> Result<Self, Self::Error> {
        let mut details = Self::new();
        if input.store_only {
            details = details.store_only();
        }
        if let Some(bytes) = input.metadata_json {
            details = details.with_metadata(metadata(&bytes)?);
        }
        if let Some(reference) = input.payment_reference {
            details = details.with_payment_reference(payment_reference(&reference)?);
        }
        if input.unmarked_credit_confirmed {
            details = details.with_unmarked_credit_confirmed(UserConfirmed::new());
        }
        if input.maybe_credited_confirmed {
            details = details.with_maybe_credited_confirmed(UserConfirmed::new());
        }
        Ok(details)
    }
}

/// The original signed receipt and the deletions observed for it (Paykit spec 6).
#[derive(uniffi::Record)]
pub struct RetainedReceipt {
    /// The original activity record key (Paykit wire 5.3).
    pub record: Vec<u8>,
    /// The original verified receipt signed by the payee (Paykit wire 4.11, 5.2).
    pub receipt: SignedReceipt,
    /// The observed deletions, possibly empty after restoring a receipt (Paykit spec 6).
    pub deletions: Vec<ReceiptDeletion>,
}

/// A Payment Receipt with its signed message frame and app certificate (Paykit wire 4.11, 5.2).
#[derive(uniffi::Record)]
pub struct SignedReceipt {
    /// The original receipt's message ID (Switch wire 5.1).
    pub msg_id: String,
    /// The identity that signed the app certificate (Paykit wire 5.2).
    pub sender: Vec<u8>,
    /// The certificate's exact body bytes, base64url (Switch wire 2.2).
    pub certificate_body: String,
    /// The certificate signature, base64url (Switch wire 2.2).
    pub certificate_signature: String,
    /// The original signed message frame, including the receipt JSON (Switch wire 4.5).
    pub plaintext: Vec<u8>,
    /// The inbox acceptance time in Unix milliseconds, when recorded (Paykit wire 5.2).
    pub accepted_at_ms: Option<u64>,
}

/// A deletion observed for a retained receipt (Paykit spec 6).
#[derive(uniffi::Record)]
pub struct ReceiptDeletion {
    /// The signed deletion's item hash (Switch wire 6.6).
    pub delete: Vec<u8>,
    /// The deletion's generation and sequence (Switch wire 1.7).
    pub version: Version,
    /// The deleting App ID when the signed predecessor held the receipt (Paykit spec 6).
    pub writer: Option<Vec<u8>>,
    /// The original verified feed page's service time, in Unix milliseconds (Switch spec 8.2).
    pub page_time_ms: Option<u64>,
}

impl From<sdk::RetainedPayeeReceipt> for RetainedReceipt {
    fn from(value: sdk::RetainedPayeeReceipt) -> Self {
        let receipt = value.receipt;
        let cert = receipt.cert().signed();
        Self {
            record: value.record.into(),
            receipt: SignedReceipt {
                msg_id: receipt.message().msg_id.clone(),
                sender: receipt.sender().into(),
                certificate_body: cert.body.clone(),
                certificate_signature: cert.sig.clone(),
                plaintext: receipt.plaintext().encode(),
                accepted_at_ms: receipt.accepted_at(),
            },
            deletions: value
                .deletions
                .into_iter()
                .map(|d| ReceiptDeletion {
                    delete: d.delete.into(),
                    version: d.version.into(),
                    writer: d.writer.map(Vec::from),
                    page_time_ms: d.page_time_ms,
                })
                .collect(),
        }
    }
}

/// Messages to ask a counterparty to re-send (Paykit wire 4.12).
#[derive(Default, uniffi::Record)]
pub struct ResendSelection {
    /// Select original messages by ID (Paykit wire 4.12).
    #[uniffi(default = [])]
    pub msg_ids: Vec<String>,
    /// Select messages accepted at or after this Unix time in milliseconds (Paykit wire 4.12).
    #[uniffi(default = None)]
    pub accepted_from_ms: Option<u64>,
    /// Select messages accepted at or before this Unix time in milliseconds (Paykit wire 4.12).
    #[uniffi(default = None)]
    pub accepted_until_ms: Option<u64>,
    /// Select requests and all messages referring to their 32-byte hashes (Paykit wire 4.12).
    #[uniffi(default = [])]
    pub request_hashes: Vec<Vec<u8>>,
}

impl TryFrom<ResendSelection> for sdk::ResendSelection {
    type Error = WalletError;

    fn try_from(selection: ResendSelection) -> Result<Self, Self::Error> {
        let hashes = selection
            .request_hashes
            .into_iter()
            .map(|h| fixed(h, "request_hashes"))
            .collect::<Result<_, _>>()?;
        let mut value = Self::default()
            .with_msg_ids(selection.msg_ids)
            .with_request_hashes(hashes);
        if let Some(at) = selection.accepted_from_ms {
            value = value.with_accepted_from_ms(timestamp(at, "accepted_from_ms")?);
        }
        if let Some(at) = selection.accepted_until_ms {
            value = value.with_accepted_until_ms(timestamp(at, "accepted_until_ms")?);
        }
        Ok(value)
    }
}

#[uniffi::export]
impl NativeWallet {
    /// Sign a one-time request under the wallet's selling label, record and queue it together,
    /// then deliver it to the admitting payer. Held sends create nothing. A kept batch retries
    /// automatically. After a store failure, check activity after the next write before retrying
    /// (Paykit spec 8.1, Paykit wire 4.5).
    pub async fn send_request(
        &self,
        to: Vec<u8>,
        draft: RequestDraft,
    ) -> Result<SentRequest, WalletError> {
        let to = fixed(to, "to")?;
        let draft = draft.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.send_request(to, draft).await?.into()) })
        })
        .await
    }

    /// List all bound requests sent and payments received without a request. A failed read
    /// returns an error for the whole list (Paykit spec 6, Paykit wire 5.3).
    pub async fn payee_activity(&self) -> Result<Vec<PayeeRequest>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .payee_activity()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }

    /// Check the proof against its destination, amount and time limits. Return the method's
    /// verified payment time in Unix milliseconds, or no time for an unconfirmed payment
    /// or a settlement credited elsewhere (Paykit spec 8.1, 8.4).
    pub async fn verify_payment(&self, record: Vec<u8>) -> Result<Option<u64>, WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| Box::pin(async move { Ok(app.verify_payment(&record).await?) }))
            .await
    }

    /// Verify the payment, record its signed receipt and send it unless storing only. Return the
    /// existing receipt ID on repeat, or no ID for an unconfirmed or already credited settlement.
    /// A requested payment uses the request's reference (Paykit spec 8.1, Paykit wire 4.11).
    pub async fn confirm_payment(
        &self,
        record: Vec<u8>,
        details: Option<ReceiptDetails>,
    ) -> Result<Option<String>, WalletError> {
        let record = fixed(record, "record")?;
        let details = details.map(sdk::ReceiptDetails::try_from).transpose()?;
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(match details {
                    Some(details) => app.confirm_payment_with(&record, details).await?,
                    None => app.confirm_payment(&record).await?,
                })
            })
        })
        .await
    }

    /// Cancel the sent request and return the cancellation's message ID. It goes to the payer's
    /// paying apps without a label. A crossing proof still counts (Paykit spec 3.4, 8.1, Paykit wire 4.8).
    pub async fn cancel_sent_request(
        &self,
        record: Vec<u8>,
        reason: Option<String>,
    ) -> Result<String, WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.cancel_sent_request(&record, reason).await?) })
        })
        .await
    }

    /// Return each retained signed receipt once, including restored receipts, under its verified
    /// source key once the payment method can establish it (Paykit spec 6).
    pub async fn retained_payee_receipts(&self) -> Result<Vec<RetainedReceipt>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .retained_payee_receipts()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }

    /// Ask the counterparty to re-send selected messages and return the request's message ID.
    /// A reply is best effort. Holds on sends, owed floor raises and unresolved overlapping
    /// requests can prevent queuing (Paykit spec 8.1, 9, Paykit wire 4.12).
    pub async fn request_resend(
        &self,
        counterparty: Vec<u8>,
        selection: ResendSelection,
    ) -> Result<String, WalletError> {
        let counterparty = fixed(counterparty, "counterparty")?;
        let selection = selection.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.request_resend(counterparty, selection).await?) })
        })
        .await
    }
}
