//! The payee's requests, payment checks, receipts and resend requests (Paykit spec 6, 8.1, 9).

use crate::{
    fixed, lists::list_endpoints, Amount, BillingPeriod, ListEndpoint, MessageDelivery,
    NativeWallet, Receipt, RequestId, Version, WalletError,
};
use paykit_lib::{
    bodies::Conversion,
    periods::{self, AnchoredRule, Unit},
    Asset, Decimal, PaymentEndpointIdentifier, PaymentReference, Rate,
};
use paykit_sdk::{self as sdk, UserConfirmed};
use serde_json::{Map, Value};

/// Where a request may be paid (Paykit wire 4.5).
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

impl From<Rate> for ConversionRate {
    fn from(rate: Rate) -> Self {
        Self {
            asset: rate.asset().as_str().into(),
            value: rate.value().as_str().into(),
        }
    }
}

/// How a request prices endpoints in other assets (Paykit wire 4.4).
#[derive(uniffi::Enum)]
pub enum RequestConversion {
    /// Fixed Conversion Rates for the whole request (Paykit wire 4.4).
    Fixed {
        /// The rates, one per asset or asset and rail (Paykit wire 4.4).
        rates: Vec<ConversionRate>,
    },
    /// A recurring request's rates come with each period's details (Paykit spec 8.2,
    /// Paykit wire 4.4, 4.9).
    PerPeriod,
}

/// The rule defining a recurring request's Billing Periods (Paykit wire 4.4).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Recurrence {
    /// The number of units between boundaries (Paykit wire 4.4).
    pub every: u64,
    /// The recurrence unit, from minute through year (Paykit wire 4.4).
    pub unit: RecurrenceUnit,
    /// The anchor of the boundaries, in Unix milliseconds (Paykit wire 4.4).
    pub anchor: u64,
    /// An instant in the first period, in Unix milliseconds (Paykit wire 4.4).
    pub starts_at: u64,
    /// The exclusive end of the recurrence, in Unix milliseconds, if given (Paykit wire 4.4).
    pub ends_at: Option<u64>,
}

/// A fixed or calendar unit of a recurrence (Paykit wire 4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RecurrenceUnit {
    /// Sixty seconds (Paykit wire 4.4).
    Minute,
    /// Sixty minutes (Paykit wire 4.4).
    Hour,
    /// Twenty-four hours (Paykit wire 4.4).
    Day,
    /// Seven days (Paykit wire 4.4).
    Week,
    /// A calendar month (Paykit wire 4.4).
    Month,
    /// A calendar year (Paykit wire 4.4).
    Year,
}

impl From<periods::Recurrence> for Recurrence {
    fn from(value: periods::Recurrence) -> Self {
        let rule = value.rule();
        Self {
            every: rule.every(),
            unit: match rule.unit() {
                Unit::Minute => RecurrenceUnit::Minute,
                Unit::Hour => RecurrenceUnit::Hour,
                Unit::Day => RecurrenceUnit::Day,
                Unit::Week => RecurrenceUnit::Week,
                Unit::Month => RecurrenceUnit::Month,
                Unit::Year => RecurrenceUnit::Year,
            },
            anchor: rule.anchor(),
            starts_at: value.starts_at(),
            ends_at: value.ends_at(),
        }
    }
}

impl TryFrom<Recurrence> for periods::Recurrence {
    type Error = WalletError;

    fn try_from(value: Recurrence) -> Result<Self, Self::Error> {
        let unit = match value.unit {
            RecurrenceUnit::Minute => Unit::Minute,
            RecurrenceUnit::Hour => Unit::Hour,
            RecurrenceUnit::Day => Unit::Day,
            RecurrenceUnit::Week => Unit::Week,
            RecurrenceUnit::Month => Unit::Month,
            RecurrenceUnit::Year => Unit::Year,
        };
        AnchoredRule::new(value.anchor, value.every, unit)
            .and_then(|rule| Self::new(rule, value.starts_at, value.ends_at))
            .map_err(|_| WalletError::invalid("recurrence"))
    }
}

/// A Payment Request, one-time or recurring. The wallet's selling label applies (Paykit spec
/// 3.1, 8.1, 8.2, Paykit wire 4.5).
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
    /// How endpoints in other assets are priced (Paykit wire 4.4).
    #[uniffi(default = None)]
    pub conversion: Option<RequestConversion>,
    /// The Billing Periods of a recurring request; none for a one-time request (Paykit spec 8.2,
    /// Paykit wire 4.4).
    #[uniffi(default = None)]
    pub recurrence: Option<Recurrence>,
    /// A recurring request's payment deadline, in seconds after each period's start (Paykit wire 4.4).
    #[uniffi(default = None)]
    pub period_deadline_s: Option<u64>,
    /// A recurring request whose endpoints serve only its first period; each later period is
    /// paid to the endpoints of its period details (Paykit spec 8.2, Paykit wire 4.4).
    #[uniffi(default = false)]
    pub per_period_endpoints: bool,
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
    crate::opaque::object(bytes, crate::opaque::Place::Metadata)
        .ok_or_else(|| WalletError::invalid("metadata_json"))
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
        if let Some(conversion) = input.conversion {
            draft = draft.with_conversion(match conversion {
                RequestConversion::Fixed { rates } => Conversion::Fixed(
                    rates
                        .into_iter()
                        .map(Rate::try_from)
                        .collect::<Result<_, _>>()?,
                ),
                RequestConversion::PerPeriod => Conversion::PerPeriod,
            });
        }
        if let Some(recurrence) = input.recurrence {
            draft = draft.with_recurrence(recurrence.try_into()?);
        }
        if let Some(seconds) = input.period_deadline_s {
            draft = draft.with_period_deadline_s(seconds);
        }
        if input.per_period_endpoints {
            draft = draft.with_per_period_endpoints();
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

/// One Billing Period's record as this app holds it (Paykit spec 8.2, Paykit wire 5.3).
#[derive(uniffi::Record)]
pub struct PeriodView {
    /// The period record key (Paykit wire 5.3). `verify_payment` and `confirm_payment` take
    /// the key a `PayeeRequest` lists; the payer's key for the same period is another one.
    pub record: Vec<u8>,
    /// The Billing Period (Paykit wire 4.4).
    pub billing_period: BillingPeriod,
    /// The payee's signed period details valid against the request, latest first. Each stays
    /// valid until its own expiry (Paykit wire 4.9, 5.3).
    pub details: Vec<PeriodDetailsView>,
    /// Whether the record holds a proof of the period's payment, credited or not (Paykit wire 4.10).
    pub paid: bool,
    /// Whether the payee issued a receipt for this period (Paykit wire 4.11).
    pub credited: bool,
    /// The receipt shown for the credit, when present (Paykit wire 4.11). Its Payment Reference
    /// is unset here: the request's is on the request itself.
    pub receipt: Option<Receipt>,
}

impl TryFrom<sdk::PeriodView> for PeriodView {
    type Error = WalletError;

    fn try_from(period: sdk::PeriodView) -> Result<Self, Self::Error> {
        Ok(Self {
            record: period.record.into(),
            billing_period: period.billing_period.into(),
            details: period.details.into_iter().map(Into::into).collect(),
            paid: period.paid,
            credited: period.credited,
            receipt: period.receipt.map(TryInto::try_into).transpose()?,
        })
    }
}

/// A payee's signed period details (Paykit wire 4.9).
#[derive(uniffi::Record)]
pub struct PeriodDetailsView {
    /// The signed message ID (Switch wire 5.1).
    pub msg_id: String,
    /// The App ID that owns the destinations (Paykit wire 4.9).
    pub app_id: Vec<u8>,
    /// The signed creation time, in Unix milliseconds (Paykit wire 4.9).
    pub created_at_ms: u64,
    /// The inclusive expiry, in Unix milliseconds (Paykit wire 4.9).
    pub expires_at_ms: u64,
    /// Identifiers of the supplied Payment Endpoints (Paykit wire 4.9).
    pub endpoint_ids: Vec<String>,
    /// Conversion Rates supplied for the period (Paykit wire 4.9).
    pub rates: Vec<ConversionRate>,
}

impl From<sdk::PeriodDetailsView> for PeriodDetailsView {
    fn from(details: sdk::PeriodDetailsView) -> Self {
        Self {
            msg_id: details.msg_id,
            app_id: details.app_id.into(),
            created_at_ms: details.created_at_ms,
            expires_at_ms: details.expires_at_ms,
            endpoint_ids: details
                .endpoint_ids
                .into_iter()
                .map(|id| id.as_str().into())
                .collect(),
            rates: details.rates.into_iter().map(Into::into).collect(),
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
    /// A recurring request's period records, in period order. A period's own record key is
    /// what `verify_payment` and `confirm_payment` take (Paykit spec 8.2, Paykit wire 5.3).
    pub periods: Vec<PeriodView>,
}

impl TryFrom<sdk::PayeeActivity> for PayeeRequest {
    type Error = WalletError;

    fn try_from(entry: sdk::PayeeActivity) -> Result<Self, Self::Error> {
        Ok(Self {
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
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
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
        if selection
            .msg_ids
            .iter()
            .any(|id| pubky_switch_core::json::check_uuid_v4(id).is_err())
        {
            return Err(WalletError::invalid("msg_ids"));
        }
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

// Debug shows the shape of what the host gave, never its text: references, payloads and
// metadata can identify a buyer.
impl std::fmt::Debug for RequestDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoints { endpoints } => f
                .debug_struct("Endpoints")
                .field("endpoints", endpoints)
                .finish(),
            Self::RequiredApp { .. } => f.write_str("RequiredApp"),
            Self::Any => f.write_str("Any"),
        }
    }
}

impl std::fmt::Debug for RequestDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestDraft")
            .field("destination", &self.destination)
            .field("payment_reference_bytes", &self.payment_reference.len())
            .field("recurring", &self.recurrence.is_some())
            .field("metadata_bytes", &self.metadata_json.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ReceiptDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptDetails")
            .field("store_only", &self.store_only)
            .field("metadata_bytes", &self.metadata_json.as_ref().map(Vec::len))
            .field("unmarked_credit_confirmed", &self.unmarked_credit_confirmed)
            .field("maybe_credited_confirmed", &self.maybe_credited_confirmed)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SignedReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedReceipt")
            .field("msg_id", &self.msg_id)
            .field("plaintext_bytes", &self.plaintext.len())
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl NativeWallet {
    /// Sign a one-time or recurring request under the wallet's selling label, record and queue
    /// it together, then deliver it to the admitting payer. Held sends create nothing. A kept
    /// batch retries automatically. After a store failure, check activity after the next write
    /// before retrying (Paykit spec 8.1, 8.2, Paykit wire 4.5).
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
                app.payee_activity()
                    .await?
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect()
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
