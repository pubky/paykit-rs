use std::fmt;

use paykit_lib::{
    BillingPeriod, PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    ReceiptDraft, ReceiptId,
};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{PaykitSdkError, Result};

/// Builder for caller-provided receipt fields.
#[derive(Clone)]
pub struct ReceiptDraftBuilder {
    receipt_id: Option<ReceiptId>,
    payment_reference: PaymentReference,
    payment_request_id: Option<PaymentRequestId>,
    billing_period: Option<BillingPeriod>,
    payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    amount: Option<PaymentAmount>,
    metadata: JsonMap<String, JsonValue>,
}

impl fmt::Debug for ReceiptDraftBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptDraftBuilder")
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field(
                "metadata",
                &format_args!("<redacted:{} fields>", self.metadata.len()),
            )
            .finish()
    }
}

impl ReceiptDraftBuilder {
    /// Start a draft from a Payment Reference string.
    pub fn new(payment_reference: impl Into<String>) -> Result<Self> {
        Ok(Self::from_payment_reference(PaymentReference::new(
            payment_reference,
        )?))
    }

    /// Start a draft from an already validated Payment Reference.
    pub fn from_payment_reference(payment_reference: PaymentReference) -> Self {
        Self {
            receipt_id: None,
            payment_reference,
            payment_request_id: None,
            billing_period: None,
            payment_endpoint_identifier: None,
            amount: None,
            metadata: JsonMap::new(),
        }
    }

    /// Set a caller-provided Receipt ID.
    pub fn with_receipt_id(mut self, receipt_id: ReceiptId) -> Self {
        self.receipt_id = Some(receipt_id);
        self
    }

    /// Generate and set a Receipt ID before issuing.
    pub fn with_new_receipt_id(self) -> Self {
        self.with_receipt_id(ReceiptId::new_v4())
    }

    /// Set the Payment Request ID this receipt corresponds to.
    pub fn with_payment_request_id(mut self, payment_request_id: PaymentRequestId) -> Self {
        self.payment_request_id = Some(payment_request_id);
        self
    }

    /// Set the Billing Period for a recurring Payment Request receipt.
    pub fn with_billing_period(mut self, billing_period: BillingPeriod) -> Self {
        self.billing_period = Some(billing_period);
        self
    }

    /// Set the Payment Endpoint Identifier used for the payment.
    pub fn with_payment_endpoint_identifier(
        mut self,
        payment_endpoint_identifier: PaymentEndpointIdentifier,
    ) -> Self {
        self.payment_endpoint_identifier = Some(payment_endpoint_identifier);
        self
    }

    /// Validate and set the Payment Endpoint Identifier used for the payment.
    pub fn with_payment_endpoint_identifier_text(
        self,
        payment_endpoint_identifier: impl Into<String>,
    ) -> Result<Self> {
        Ok(
            self.with_payment_endpoint_identifier(PaymentEndpointIdentifier::new(
                payment_endpoint_identifier,
            )?),
        )
    }

    /// Set the Payment Amount being receipted.
    pub fn with_amount(mut self, amount: PaymentAmount) -> Self {
        self.amount = Some(amount);
        self
    }

    /// Validate and set the Payment Amount being receipted.
    pub fn with_amount_text(
        self,
        value: impl Into<String>,
        asset: impl Into<String>,
    ) -> Result<Self> {
        Ok(self.with_amount(PaymentAmount::new(value, asset)?))
    }

    /// Set caller-defined Receipt Metadata.
    pub fn with_metadata(mut self, metadata: JsonMap<String, JsonValue>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Build the Receipt Draft.
    pub fn build(self) -> Result<ReceiptDraft> {
        if self.billing_period.is_some() && self.payment_request_id.is_none() {
            return Err(PaykitSdkError::Protocol {
                context: "Receipt Draft billing_period requires payment_request_id".into(),
                source: None,
            });
        }
        Ok(ReceiptDraft {
            receipt_id: self.receipt_id,
            payment_reference: self.payment_reference,
            payment_request_id: self.payment_request_id,
            billing_period: self.billing_period,
            payment_endpoint_identifier: self.payment_endpoint_identifier,
            amount: self.amount,
            metadata: self.metadata,
        })
    }
}
