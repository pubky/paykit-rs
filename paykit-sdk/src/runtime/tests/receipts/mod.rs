use super::*;

mod access;
mod issuance;
mod retrieval;

fn receipt_draft(receipt_id: &str) -> ReceiptDraft {
    ReceiptDraft {
        receipt_id: Some(paykit_lib::ReceiptId::new(receipt_id).unwrap()),
        payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some(
            paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        ),
        amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
        metadata: serde_json::json!({"settlement_id": "abc-123"})
            .as_object()
            .cloned()
            .unwrap(),
    }
}
