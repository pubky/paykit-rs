use super::*;
use paykit_lib::{EventId, PaymentAmount, PaymentRequestId};

pub(super) fn utc_time(value: &str) -> Result<DateTime<Utc>> {
    if !value.ends_with('Z') {
        return Err(protocol("Accounting time must use UTC Z"));
    }
    let time = DateTime::parse_from_rfc3339(value)
        .map_err(|_| protocol("Invalid accounting time"))?
        .with_timezone(&Utc);
    valid_time(time)?;
    Ok(time)
}

pub(super) fn valid_time(time: DateTime<Utc>) -> Result<()> {
    if time.timestamp_subsec_nanos() >= 1_000_000_000 {
        return Err(protocol("Leap seconds cannot authorize accounting"));
    }
    Ok(())
}

pub(super) fn uuid(value: &str) -> Result<()> {
    EventId::new(value)
        .and_then(|id| {
            if id.as_str() == value {
                Ok(())
            } else {
                Err(paykit_lib::PaykitError::Validation(
                    "Noncanonical accounting ID".into(),
                ))
            }
        })
        .map_err(|_| protocol("Invalid accounting identifier"))
}

pub(super) fn amount(value: &crate::AmountRecord) -> Result<PaymentAmount> {
    PaymentAmount::new(value.value.clone(), value.asset.clone())
        .map_err(|_| protocol("Invalid accounting amount"))
}

fn valid_scope(value: &PaymentAccountingScope) -> Result<()> {
    crate::PubkyPublicKey::new(value.local_public_key.as_str())?;
    crate::PubkyPublicKey::new(value.counterparty.as_str())?;
    uuid(&value.payment_request_id)?;
    PaymentRequestId::new(value.payment_request_id.clone())
        .map_err(|_| protocol("Invalid accounting request ID"))?;
    // Receiver paths and public keys are validated by their serde implementations.
    Ok(())
}

pub(super) fn disposition(value: &PaymentDisposition) -> Result<()> {
    if let PaymentDisposition::Deferred { reason } = value {
        if reason.is_empty() || reason.len() > 256 || reason.chars().any(char::is_control) {
            return Err(policy(
                "Deferred reason must contain 1 to 256 bytes without controls",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_accounting(state: &AllowanceAccountingState) -> Result<()> {
    uuid(&state.epoch)?;
    let mut authorizations = std::collections::HashSet::new();
    for (index, record) in state.history.associations.iter().enumerate() {
        valid_scope(&record.request)?;
        if record.revisions.is_empty()
            || state.history.associations[..index]
                .iter()
                .any(|other| other.request == record.request)
        {
            return Err(protocol(
                "Accounting association history is incomplete or duplicated",
            ));
        }
        let mut boundary = None;
        let mut authorized_at = None;
        for (i, revision) in record.revisions.iter().enumerate() {
            if revision.revision
                != u64::try_from(i)
                    .ok()
                    .and_then(|v| v.checked_add(1))
                    .ok_or_else(|| protocol("Accounting revision overflow"))?
            {
                return Err(protocol(
                    "Accounting association revisions must be contiguous",
                ));
            }
            uuid(&revision.allowance_id)?;
            AllowanceId::new(revision.allowance_id.clone())
                .map_err(|_| protocol("Invalid accounting Allowance ID"))?;
            valid_time(revision.authorized_at)?;
            if !state.history.watermarks.iter().any(|watermark| {
                watermark_scope(watermark, &record.request, &revision.allowance_id)
                    && watermark.evaluated_at >= revision.authorized_at
            }) {
                return Err(protocol("Accounting watermark precedes selection history"));
            }
            if authorized_at.is_some_and(|previous| revision.authorized_at < previous) {
                return Err(protocol("Association time moved backward"));
            }
            authorized_at = Some(revision.authorized_at);
            if i == 0 {
                if revision.effective_from.is_some() || revision.authorization_id.is_some() {
                    return Err(protocol("Initial association cannot be a replacement"));
                }
            } else {
                let from = revision
                    .effective_from
                    .ok_or_else(|| protocol("Replacement boundary is missing"))?;
                valid_time(from)?;
                uuid(
                    revision
                        .authorization_id
                        .as_deref()
                        .ok_or_else(|| protocol("Replacement authorization is missing"))?,
                )?;
                if !authorizations.insert(revision.authorization_id.as_deref()) {
                    return Err(protocol("Duplicate reassociation authorization ID"));
                }
                if from < revision.authorized_at
                    || boundary.is_some_and(|previous| from <= previous)
                {
                    return Err(protocol("Replacement must advance to a future boundary"));
                }
                boundary = Some(from);
            }
        }
    }
    let mut attempt_ids = std::collections::HashSet::new();
    for (index, record) in state.history.occurrences.iter().enumerate() {
        valid_scope(&record.key.request)?;
        disposition(&record.disposition)?;
        if occupied(record) && matches!(record.disposition, PaymentDisposition::Deferred { .. }) {
            return Err(protocol("Execution evidence cannot be labeled deferred"));
        }
        if state.history.occurrences[..index]
            .iter()
            .any(|other| other.key == record.key)
        {
            return Err(protocol("Duplicate payment occurrence"));
        }
        if let Some(period) = &record.key.billing_period {
            valid_time(period.starts_at)?;
            valid_time(period.ends_at)?;
            if period.ends_at <= period.starts_at {
                return Err(protocol("Invalid accounting Billing Period"));
            }
        }
        validate_attribution(
            state,
            &record.key,
            record.allowance_id.as_deref(),
            record.association_revision,
        )?;
        if record
            .attempts
            .iter()
            .filter(|a| a.status != PaymentExecutionStatus::Failed)
            .count()
            > 1
        {
            return Err(protocol(
                "Multiple live attempts for one payment occurrence",
            ));
        }
        for attempt in &record.attempts {
            uuid(&attempt.attempt_id)?;
            uuid(&attempt.epoch)?;
            valid_time(attempt.admitted_at)?;
            amount(&attempt.amount)?;
            if !attempt_ids.insert(&attempt.attempt_id) {
                return Err(protocol("Duplicate payment attempt ID"));
            }
            match attempt.mode {
                PaymentExecutionMode::Automatic => {
                    if attempt.allowance_id.is_none() {
                        return Err(protocol("Automatic attempt has no Allowance"));
                    }
                    validate_attribution(
                        state,
                        &record.key,
                        attempt.allowance_id.as_deref(),
                        attempt.association_revision,
                    )?;
                    let id = attempt
                        .allowance_id
                        .as_deref()
                        .ok_or_else(|| protocol("Automatic attempt lacks Allowance attribution"))?;
                    if !state.history.watermarks.iter().any(|w| {
                        watermark_scope(w, &record.key.request, id)
                            && w.evaluated_at >= attempt.admitted_at
                    }) {
                        return Err(protocol("Accounting watermark precedes retained admission"));
                    }
                }
                PaymentExecutionMode::Manual
                    if attempt.allowance_id.is_some() || attempt.association_revision.is_some() =>
                {
                    return Err(protocol("Manual attempt cannot consume an Allowance"))
                }
                PaymentExecutionMode::Manual => {}
            }
        }
    }
    for (index, watermark) in state.history.watermarks.iter().enumerate() {
        valid_time(watermark.evaluated_at)?;
        uuid(&watermark.allowance_id)?;
        AllowanceId::new(watermark.allowance_id.clone())
            .map_err(|_| protocol("Invalid watermark Allowance ID"))?;
        if state.history.watermarks[..index].iter().any(|other| {
            other.local_public_key == watermark.local_public_key
                && other.local_receiver_path == watermark.local_receiver_path
                && other.counterparty == watermark.counterparty
                && other.counterparty_receiver_path == watermark.counterparty_receiver_path
                && other.allowance_id == watermark.allowance_id
        }) {
            return Err(protocol("Duplicate accounting watermark"));
        }
    }
    Ok(())
}

fn validate_attribution(
    state: &AllowanceAccountingState,
    key: &PaymentOccurrenceKey,
    id: Option<&str>,
    revision: Option<u64>,
) -> Result<()> {
    match (id, revision) {
        (None, None) => Ok(()),
        (Some(id), Some(revision))
            if state.history.associations.iter().any(|a| {
                a.request == key.request
                    && a.revisions.iter().any(|r| {
                        r.revision == revision
                            && r.allowance_id == id
                            && r.effective_from.is_none_or(|from| {
                                key.billing_period
                                    .as_ref()
                                    .is_some_and(|p| p.starts_at >= from)
                            })
                    })
            }) =>
        {
            Ok(())
        }
        _ => Err(protocol(
            "Accounting attribution has no matching association revision",
        )),
    }
}

pub(super) fn payer_scope(
    state: &AllowanceAccountingState,
    identity: &crate::PubkyPublicKey,
    local: &PaykitReceiverPath,
) -> Result<()> {
    if state
        .history
        .associations
        .iter()
        .any(|a| &a.request.local_public_key != identity || &a.request.local_receiver_path != local)
        || state.history.occurrences.iter().any(|o| {
            &o.key.request.local_public_key != identity
                || &o.key.request.local_receiver_path != local
        })
        || state
            .history
            .watermarks
            .iter()
            .any(|w| &w.local_public_key != identity || &w.local_receiver_path != local)
    {
        return Err(protocol("Accounting belongs to another payer scope"));
    }
    Ok(())
}
