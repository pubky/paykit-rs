//! `encoding.json`: decimal amounts (Paykit wire 1.2).

use paykit_lib::amount::Decimal;

use crate::common::{arr, paykit, st};

#[test]
fn test_vectors_encoding() {
    let e = paykit("encoding.json");
    let valid = arr(&e["amounts"], "valid");
    assert!(!valid.is_empty());
    for case in valid {
        let amount = Decimal::parse(st(case, "text")).unwrap();
        assert_eq!(amount.as_str(), st(case, "text"), "the spelling is kept");
        assert_eq!(amount.normalized(), st(case, "normalized"), "{case}");
        // The normalized spelling reads back as the same value and spells itself.
        let again = Decimal::parse(&amount.normalized()).unwrap();
        assert_eq!(again, amount);
        assert_eq!(again.normalized(), again.as_str());
    }
    let invalid = arr(&e["amounts"], "invalid");
    assert!(!invalid.is_empty());
    for case in invalid {
        let error = Decimal::parse(st(case, "text")).unwrap_err().to_string();
        assert!(error.contains("Paykit wire 1.2"), "{case}: {error}");
    }
}
