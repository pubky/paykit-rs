//! `encoding.json`: decimal amounts (Paykit wire 1.2).

use paykit_lib::amount::Decimal;
use serde_json::{json, Value};

use crate::common::{arr, generated, paykit, st, writing, Keys, Place};

#[test]
fn test_vectors_encoding() {
    if !writing() {
        check();
    }
}

/// `encoding.json` against paykit-lib: its generated normalized spellings, then every amount
/// read again. With [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration
/// driver runs it once it has written the set.
pub(crate) fn check() {
    generate(&Keys::from_seeds());
    let e = paykit("encoding.json");
    let valid = arr(&e["amounts"], "valid");
    assert!(!valid.is_empty());
    for case in valid {
        let amount = Decimal::parse(st(case, "text")).unwrap();
        assert_eq!(amount.as_str(), st(case, "text"), "the spelling is kept");
        // The normalized spelling reads back as the same value and spells itself.
        let again = Decimal::parse(st(case, "normalized")).unwrap();
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

/// The cases paykit-lib generates in `encoding.json` (README "How the files were
/// generated"), compared with the file, and written into the set by the regeneration driver
/// (`regenerate.rs`): the normalized spelling of each valid amount, from its hand-written
/// `text`. The amounts and the invalid cases are written by hand.
pub(crate) fn generate(_keys: &Keys) {
    let e = paykit("encoding.json");
    let cases: Vec<(Place<'static>, Value)> = arr(&e["amounts"], "valid")
        .iter()
        .enumerate()
        .map(|(i, case)| {
            let amount = Decimal::parse(st(case, "text")).unwrap();
            (
                Place::At(format!("/amounts/valid/{i}/normalized")),
                json!(amount.normalized()),
            )
        })
        .collect();
    generated("encoding.json", &cases);
}
