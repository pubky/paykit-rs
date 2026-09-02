use chrono::{DateTime, Utc};
use paykit_lib::{Receipt, ReceiptDecryptionKey};
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};
use sha2::{Digest, Sha256};

use super::records::{ReceiptAccessRecord, ReceiptRecord};
use crate::{domain::records::BillingPeriodRecord, PaykitSdkError, PubkyPublicKey, Result};

#[cfg(test)]
use crate::storage::StorageAdapter;

/// List indexed Receipt Access records for one counterparty.
#[cfg(test)]
pub(crate) async fn receipt_access_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<ReceiptAccessRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_access_records(counterparty)))
        .await
}

/// Load the latest indexed Receipt Access record for a receipt.
#[cfg(test)]
pub(crate) async fn receipt_access_record_by_receipt_id<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    receipt_id: &str,
) -> Result<Option<ReceiptAccessRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_access_record_by_receipt_id(counterparty, receipt_id)))
        .await
}

pub(crate) async fn fetch_encrypted_receipt_json(
    public_storage: &pubky::PublicStorage,
    issuer: &PubkyPublicKey,
    location: &str,
) -> Result<Option<String>> {
    let addr = format!("{}{}", issuer.to_public_key()?, location);
    match public_storage.get(addr).await {
        Ok(mut response) => {
            if response
                .content_length()
                .is_some_and(|length| length > paykit_lib::ENCRYPTED_RECEIPT_MAX_BYTES as u64)
            {
                return Err(encrypted_receipt_size_error());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) =
                response
                    .chunk()
                    .await
                    .map_err(|err| PaykitSdkError::Transport {
                        context: "read encrypted receipt bytes".into(),
                        source: Some(err.into()),
                    })?
            {
                if bytes.len().saturating_add(chunk.len()) > paykit_lib::ENCRYPTED_RECEIPT_MAX_BYTES
                {
                    return Err(encrypted_receipt_size_error());
                }
                bytes.extend_from_slice(&chunk);
            }
            let json = encrypted_receipt_json_from_bytes(&bytes)?;
            Ok(Some(json))
        }
        Err(err) if is_not_found(&err) => Ok(None),
        Err(err) => Err(PaykitSdkError::Transport {
            context: "fetch encrypted receipt".into(),
            source: Some(err.into()),
        }),
    }
}

fn encrypted_receipt_size_error() -> PaykitSdkError {
    PaykitSdkError::Protocol {
        context: format!(
            "encrypted receipt exceeds {} bytes",
            paykit_lib::ENCRYPTED_RECEIPT_MAX_BYTES
        ),
        source: None,
    }
}

pub(super) fn encrypted_receipt_json_from_bytes(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| PaykitSdkError::Protocol {
        // FromUtf8Error detail describes the fetched body; keep the context
        // static and drop the cause so none of it reaches FFI or Rust logs.
        context: "encrypted receipt is not valid UTF-8".into(),
        source: None,
    })
}

/// Build a not-found error after a Receipt Location returns 404/GONE.
///
/// The static context preserves the `NotFound` classification without exposing
/// the private Receipt Location or Receipt ID across the FFI boundary.
pub(crate) fn missing_encrypted_receipt_error(_location: &str) -> PaykitSdkError {
    PaykitSdkError::NotFound {
        context: "encrypted receipt was not found at its receipt location".into(),
        source: None,
    }
}

/// Choose which retrieval failure to keep while `retrieve_receipt` walks the
/// candidate Receipt Locations (newest Receipt Access first).
///
/// `NotFound` is the weakest signal: it is definitive absence for one location
/// only, and must not mask a transport or decrypt failure from another (often
/// newer) location that suggests the Encrypted Receipt may still be
/// retrievable on retry. A confirmed 404 therefore only surfaces when it is
/// the only kind of failure seen; any other error kind displaces it.
pub(crate) fn merge_retrieval_error(
    previous: Option<PaykitSdkError>,
    err: PaykitSdkError,
) -> PaykitSdkError {
    match (previous, err) {
        (Some(previous), PaykitSdkError::NotFound { .. }) => previous,
        (_, err) => err,
    }
}

pub(crate) fn decrypt_receipt_record_from_access(
    access: &ReceiptAccessRecord,
    encrypted_json: &str,
    retrieved_at: DateTime<Utc>,
    expected_recipient: &PubkyPublicKey,
) -> Result<ReceiptRecord> {
    let key = ReceiptDecryptionKey::new(access.key.clone())?;
    let receipt = paykit_lib::decrypt_receipt(encrypted_json, &key, &access.location)?;
    validate_receipt_matches_access(access, &receipt, expected_recipient)?;
    Ok(ReceiptRecord::from_receipt(
        access.counterparty.clone(),
        access,
        receipt,
        retrieved_at,
    ))
}

pub(crate) fn receipt_record_matches_access(
    record: &ReceiptRecord,
    access: &ReceiptAccessRecord,
) -> bool {
    record.issuer == access.counterparty
        && record.app_id == access.app_id
        && record.receipt_access_key_hash == receipt_access_key_hash(&access.key)
        && record.receipt_id == access.receipt_id
        && record.payment_reference == access.payment_reference
        && record.payment_request_id == access.payment_request_id
        && record.billing_period == access.billing_period
        && record.location == access.location
}

pub(crate) fn receipt_access_key_hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("sha256:{digest:x}")
}

fn validate_receipt_matches_access(
    access: &ReceiptAccessRecord,
    receipt: &Receipt,
    expected_recipient: &PubkyPublicKey,
) -> Result<()> {
    if receipt.receipt_id.as_str() != access.receipt_id {
        return Err(PaykitSdkError::Protocol {
            context: "decrypted Receipt ID does not match Receipt Access".into(),
            source: None,
        });
    }
    let recipient = PubkyPublicKey::from_public_key(&receipt.recipient_public_key);
    if &recipient != expected_recipient {
        return Err(PaykitSdkError::Protocol {
            context: "decrypted Receipt recipient does not match local identity".into(),
            source: None,
        });
    }
    if receipt.payment_reference.as_str() != access.payment_reference {
        return Err(PaykitSdkError::Protocol {
            context: "decrypted Receipt Payment Reference does not match Receipt Access".into(),
            source: None,
        });
    }
    let receipt_payment_request_id = receipt
        .payment_request_id
        .as_ref()
        .map(|id| id.as_str().to_owned());
    if receipt_payment_request_id != access.payment_request_id {
        return Err(PaykitSdkError::Protocol {
            context: "decrypted Receipt Payment Request ID does not match Receipt Access".into(),
            source: None,
        });
    }
    let receipt_billing_period = receipt
        .billing_period
        .as_ref()
        .map(BillingPeriodRecord::from);
    if receipt_billing_period != access.billing_period {
        return Err(PaykitSdkError::Protocol {
            context: "decrypted Receipt Billing Period does not match Receipt Access".into(),
            source: None,
        });
    }
    Ok(())
}

pub(super) fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}
