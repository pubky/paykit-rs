//! `encoding.json`: decimal amounts (Paykit wire 1.2).

use paykit_lib::amount::Decimal;

use crate::common::{arr, generated, paykit, st, writing, Keys};

#[test]
fn test_vectors_encoding() {
    if !writing() {
        check();
    }
}

/// `encoding.json` against paykit-lib's decimal parser. With
/// [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration driver runs it once it
/// has written the set.
pub(crate) fn check() {
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

/// `encoding.json` is written by hand (README "How the files were generated"): paykit-lib
/// generates none of its cases, and the driver writes it again unchanged.
pub(crate) fn generate(_keys: &Keys) {
    generated("encoding.json", &[]);
}
