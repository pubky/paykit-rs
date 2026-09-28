use crate::*;
use serde_json::{json, Value};

fn rate(asset: &str, value: &str) -> ConversionRate {
    ConversionRate {
        asset: asset.into(),
        value: value.into(),
    }
}
fn period() -> BillingPeriod {
    BillingPeriod::new("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z").unwrap()
}
fn recurrence() -> Recurrence {
    Recurrence::try_from(RecurrenceConfig {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-06-01T00:00:00Z".into(),
        anchor: "2026-06-01T00:00:00Z".into(),
        ends_at: None,
    })
    .unwrap()
}
fn request_with(
    conversion: Option<PaymentConversion>,
    recurrence: Option<Recurrence>,
    payment_deadline: Option<PaymentDeadline>,
    asset: &str,
    endpoints: &[&str],
) -> Result<PaymentRequest> {
    let terms = PaymentRequestTerms::builder(
        PaymentAmount::new("10", asset)?,
        PaymentReference::new("monthly-membership")?,
        endpoints
            .iter()
            .map(|id| PaymentEndpointIdentifier::new(*id))
            .collect::<Result<Vec<_>>>()?,
    )
    .recurrence(recurrence)
    .conversion(conversion)
    .payment_deadline(payment_deadline)
    .build()?;
    Ok(PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        terms,
    ))
}
fn request(conversion: Option<PaymentConversion>) -> PaymentRequest {
    request_with(
        conversion,
        Some(recurrence()),
        Some(PaymentDeadline::PeriodStart { seconds: 86400 }),
        "usd",
        &[
            "usd-bank-account",
            "usdt-arbitrum-address",
            "btc-lightning-bolt11",
        ],
    )
    .unwrap()
}
fn parse(value: Value) -> PaymentRequestEventMessage {
    parse_payment_request_event_message(&PrivateApplicationMessage {
        version: Some(1),
        kind: value["kind"].as_str().map(str::to_owned),
        raw_json: value.to_string(),
    })
    .unwrap()
}
fn round_trip(event: PaymentRequestEvent) {
    let raw = serialize_payment_request_event(&event).unwrap();
    assert!(
        raw.len() <= pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        "representative event must fit one encrypted message"
    );
    assert_eq!(
        parse(serde_json::from_str(&raw).unwrap()).event.unwrap(),
        event
    );
}
fn proof(request: &PaymentRequest, asset: &str) -> PaymentProof {
    PaymentProof::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        request.request().payment_reference().clone(),
        Some(period()),
        PaymentEndpointIdentifier::new(asset).unwrap(),
        Default::default(),
    )
}

#[test]
fn test_payment_conversion_terms_and_quotes_round_trip() {
    for conversion in [
        None,
        Some(PaymentConversion::Fixed {
            rates: vec![rate("usdt", "1"), rate("btc", "0.00001234")],
        }),
        Some(PaymentConversion::PerPeriod {}),
    ] {
        round_trip(PaymentRequestEvent::Request(request(conversion)));
    }
    let request = request(Some(PaymentConversion::PerPeriod {}));
    let quote = PaymentConversionQuote::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        period(),
        vec![rate("usdt", "1"), rate("btc", "0.00001234")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap();
    quote.validate_for_request(&request).unwrap();
    let proof =
        proof(&request, "usdt-arbitrum-address").with_conversion_quote_id(quote.event_id().clone());
    proof
        .validate_conversion_quote(&request, Some(&quote))
        .unwrap();
    round_trip(PaymentRequestEvent::ConversionQuote(quote));
    round_trip(PaymentRequestEvent::Proof(proof));
}

#[test]
fn test_payment_conversion_rates_are_positive_unique_and_exact() {
    for value in ["0", "0.000", "-1", "1e-6", "NaN", "1,000", ""] {
        assert!(request_with(
            Some(PaymentConversion::Fixed {
                rates: vec![rate("usdt", value)],
            }),
            Some(recurrence()),
            Some(PaymentDeadline::PeriodStart { seconds: 86400 }),
            "usd",
            &["usd-bank-account", "usdt-arbitrum-address"],
        )
        .is_err());
    }
    for rates in [
        vec![],
        vec![rate("usdt", "1"), rate("usdt", "2")],
        vec![rate("usd", "1")],
        vec![rate("eth", "1")],
    ] {
        assert!(request_with(
            Some(PaymentConversion::Fixed { rates }),
            Some(recurrence()),
            Some(PaymentDeadline::PeriodStart { seconds: 86400 }),
            "usd",
            &[
                "usd-bank-account",
                "usdt-arbitrum-address",
                "btc-lightning-bolt11"
            ],
        )
        .is_err());
    }
    round_trip(PaymentRequestEvent::Request(request(Some(
        PaymentConversion::Fixed {
            rates: vec![rate("btc", "0.000000000000000000000000000001")],
        },
    ))));
}

#[test]
fn test_payment_conversion_absence_and_missing_rate_have_distinct_meanings() {
    let without_conversion = request(None);
    proof(&without_conversion, "btc-lightning-bolt11")
        .validate_for_request(&without_conversion)
        .unwrap();
    let fixed = request(Some(PaymentConversion::Fixed {
        rates: vec![rate("usdt", "1")],
    }));
    assert!(proof(&fixed, "btc-lightning-bolt11")
        .validate_for_request(&fixed)
        .is_err());
    proof(&fixed, "usdt-arbitrum-address")
        .validate_for_request(&fixed)
        .unwrap();
    proof(&fixed, "usd-bank-account")
        .validate_for_request(&fixed)
        .unwrap();
    let per_period = request(Some(PaymentConversion::PerPeriod {}));
    assert!(proof(&per_period, "usdt-arbitrum-address")
        .validate_for_request(&per_period)
        .is_err());
    proof(&per_period, "usd-bank-account")
        .validate_for_request(&per_period)
        .unwrap();
}

#[test]
fn test_payment_conversion_quote_binds_request_period_and_selected_asset() {
    let request = request(Some(PaymentConversion::PerPeriod {}));
    let quote = PaymentConversionQuote::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        period(),
        vec![rate("usdt", "1")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap();
    let proof =
        proof(&request, "usdt-arbitrum-address").with_conversion_quote_id(quote.event_id().clone());
    assert!(proof.validate_conversion_quote(&request, None).is_err());
    // Expiry is evaluated against verified payment time, never the time this proof is parsed.
    proof
        .validate_conversion_quote(&request, Some(&quote))
        .unwrap();
    let quote = PaymentConversionQuote::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        period(),
        vec![rate("usdt", "1")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap();
    assert!(proof
        .validate_conversion_quote(&request, Some(&quote))
        .is_err());
    let quote = PaymentConversionQuote::new(
        proof.conversion_quote_id().unwrap().clone(),
        PaymentRequestId::new_v4(),
        period(),
        vec![rate("usdt", "1")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap();
    assert!(proof
        .validate_conversion_quote(&request, Some(&quote))
        .is_err());
    let quote = PaymentConversionQuote::new(
        proof.conversion_quote_id().unwrap().clone(),
        request.payment_request_id().clone(),
        BillingPeriod::new("2026-06-02T00:00:00Z", "2026-07-01T00:00:00Z").unwrap(),
        vec![rate("usdt", "1")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap();
    assert!(proof
        .validate_conversion_quote(&request, Some(&quote))
        .is_err());
    let quote = PaymentConversionQuote::new(
        proof.conversion_quote_id().unwrap().clone(),
        request.payment_request_id().clone(),
        BillingPeriod::new("2026-06-01T00:00:00.000Z", "2026-07-01T00:00:00.000000000Z").unwrap(),
        vec![rate("usdt", "1")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap();
    proof
        .validate_conversion_quote(&request, Some(&quote))
        .unwrap();
    let quote = PaymentConversionQuote::new(
        proof.conversion_quote_id().unwrap().clone(),
        request.payment_request_id().clone(),
        period(),
        vec![rate("btc", "0.00001")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap();
    assert!(proof
        .validate_conversion_quote(&request, Some(&quote))
        .is_err());
}

#[test]
fn test_payment_deadlines_require_the_correct_request_shape() {
    let deadline = PaymentDeadline::PeriodStart { seconds: 86400 };
    assert_eq!(
        deadline.at(Some(&period())).unwrap(),
        "2026-06-02T00:00:00Z"
    );
    assert!(deadline.at(None).is_err());
    assert!(PaymentDeadline::PeriodStart { seconds: u64::MAX }
        .at(Some(&period()))
        .is_err());
    let absolute = PaymentDeadline::At {
        timestamp: "2026-06-02T00:00:00Z".into(),
    };
    assert_eq!(absolute.at(None).unwrap(), "2026-06-02T00:00:00Z");
    assert!(absolute.at(Some(&period())).is_err());
    assert!(request_with(
        None,
        Some(recurrence()),
        Some(absolute.clone()),
        "usd",
        &["usd-bank-account"],
    )
    .is_err());
    assert!(request_with(None, None, Some(deadline), "usd", &["usd-bank-account"],).is_err());
    assert!(request_with(
        Some(PaymentConversion::PerPeriod {}),
        None,
        Some(absolute.clone()),
        "usd",
        &["usd-bank-account"],
    )
    .is_err());
    round_trip(PaymentRequestEvent::Request(
        request_with(None, None, Some(absolute), "usd", &["usd-bank-account"]).unwrap(),
    ));
}

#[test]
fn test_payment_conversion_wire_rejects_ambiguous_and_unknown_fields() {
    let raw =
        serialize_payment_request_event(&PaymentRequestEvent::Request(request(None))).unwrap();
    for (field, value) in [
        ("conversion", Value::Null),
        ("conversion", json!({"type":"floating"})),
        ("conversion", json!({"type":"per_period", "rates":[]})),
        ("payment_deadline", Value::Null),
        (
            "payment_deadline",
            json!({"type":"period_start","seconds":-1}),
        ),
    ] {
        let mut value_json: Value = serde_json::from_str(&raw).unwrap();
        value_json["request"][field] = value;
        assert!(!parse(value_json).is_valid());
    }
}

#[test]
fn test_conversion_quotes_require_a_portable_ordered_time_interval() {
    let request = request(Some(PaymentConversion::PerPeriod {}));
    PaymentConversionQuote::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        period(),
        vec![rate("usdt", "1")],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-01T00:00:00Z".into(),
    )
    .unwrap()
    .validate_for_request(&request)
    .unwrap();
    for invalid in [
        "2026-06-02T00:00:00Z",
        "2026-06-01 00:00:00Z",
        "2026-06-01t00:00:00Z",
        "2026-05-31T23:59:60Z",
        "+11533-01-14T05:20:00Z",
    ] {
        assert!(
            PaymentConversionQuote::new(
                EventId::new_v4(),
                request.payment_request_id().clone(),
                period(),
                vec![rate("usdt", "1")],
                invalid.into(),
                "2026-06-01T00:00:00Z".into(),
            )
            .is_err(),
            "{invalid}"
        );
    }
    assert!(PaymentDeadline::PeriodStart {
        seconds: 300_000_000_000
    }
    .at(Some(&period()))
    .is_err());
}

#[test]
fn test_conversion_requires_unambiguous_asset_and_endpoint_segments() {
    for endpoint in [
        "btc-",
        "Btc.v2-x-y",
        "usdc-e-arbitrum-address",
        "btc--bolt11",
    ] {
        assert!(
            request_with(
                Some(PaymentConversion::PerPeriod {}),
                Some(recurrence()),
                Some(PaymentDeadline::PeriodStart { seconds: 86400 }),
                "usd",
                &[endpoint],
            )
            .is_err(),
            "{endpoint}"
        );
    }
    assert!(request_with(
        Some(PaymentConversion::PerPeriod {}),
        Some(recurrence()),
        Some(PaymentDeadline::PeriodStart { seconds: 86400 }),
        "usdc-e",
        &["usdt-arbitrum-address"],
    )
    .is_err());
}
