//! Caller-created conversion rates and public endpoint pricing (Paykit wire 4.4).

use paykit_lib::{
    amount_for_endpoint, Asset, Decimal, Error, PaymentAmount, PaymentEndpointIdentifier, Rate,
};

#[test]
fn test_amount_rate_constructor_refuses_malformed_selectors() {
    for selector in [
        "usdt-Polygon",
        "USDT",
        "usdt_polygon",
        "usdt.polygon",
        "usdt-",
        "-polygon",
        "usdt--polygon",
        "usdt-polygon-address",
        "usdt polygon",
        "usdt-p\u{f3}lygon",
    ] {
        assert!(
            matches!(
                Rate::new(
                    Asset::new(selector).unwrap(),
                    Decimal::new("1.008").unwrap()
                ),
                Err(Error::Validation(_))
            ),
            "{selector}"
        );
    }
    for selector in ["usdt", "usdt-polygon", "usdt2-rail9"] {
        assert!(Rate::new(
            Asset::new(selector).unwrap(),
            Decimal::new("1.008").unwrap()
        )
        .is_ok());
    }
}

#[test]
fn test_amount_explicit_empty_rates_never_fall_back_to_parity() {
    let requested = PaymentAmount {
        value: Decimal::new("1").unwrap(),
        asset: Asset::new("usdt").unwrap(),
    };
    let endpoint = PaymentEndpointIdentifier::new("usdt-polygon-address").unwrap();
    assert!(matches!(
        amount_for_endpoint(&requested, Some(&[]), &endpoint, 6),
        Err(Error::Validation(_))
    ));
    assert_eq!(
        amount_for_endpoint(&requested, None, &endpoint, 6).unwrap(),
        Some(requested.clone())
    );
    let rates = [Rate::new(
        Asset::new("usdt-polygon").unwrap(),
        Decimal::new("1.008").unwrap(),
    )
    .unwrap()];
    assert_eq!(
        amount_for_endpoint(&requested, Some(&rates), &endpoint, 6)
            .unwrap()
            .unwrap()
            .value
            .as_str(),
        "1.008"
    );
}
