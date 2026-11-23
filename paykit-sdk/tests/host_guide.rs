//! Check the host guide against Paykit's public error and outcome declarations.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use syn::{Item, Visibility};

const GUIDE: &str = include_str!("../../docs/paykit-host-guide.md");

const OUTCOMES: &[&str] = &[
    "AllowanceStatus",
    "MessageDelivery",
    "PayOutcome",
    "PayeeStatus",
    "PublicAppFailureReason",
    "RailOutcome",
    "RequestStatus",
    "Resolution",
    "Stage",
];

#[test]
fn test_host_guide_covers_errors_notices_and_outcomes() {
    let mut declared = BTreeSet::new();
    read_variants(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut declared,
    );
    for name in ["PaykitError", "Refusal", "NoticeKind"]
        .into_iter()
        .chain(OUTCOMES.iter().copied())
    {
        assert!(
            declared.iter().any(|(kind, _)| kind == name),
            "no public variants found for {name}"
        );
    }
    let declared: BTreeSet<_> = declared
        .into_iter()
        .map(|(name, variant)| format!("{name}::{variant}"))
        .collect();
    let documented = documented_variants();
    let missing: Vec<_> = declared.difference(&documented).collect();
    let unknown: Vec<_> = documented.difference(&declared).collect();
    assert!(
        missing.is_empty() && unknown.is_empty(),
        "missing {missing:?}, unknown {unknown:?}"
    );
}

fn read_variants(dir: &Path, declared: &mut BTreeSet<(String, String)>) {
    for entry in fs::read_dir(dir).expect("read SDK source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            read_variants(&path, declared);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).expect("read SDK source");
            let parsed = syn::parse_file(&source).unwrap_or_else(|err| {
                panic!("cannot parse {}: {err}", path.display());
            });
            for item in parsed.items {
                let Item::Enum(item) = item else { continue };
                let name = item.ident.to_string();
                if matches!(item.vis, Visibility::Public(_)) && covers(&name) {
                    for variant in item.variants {
                        let variant = variant.ident.to_string();
                        assert!(
                            declared.insert((name.clone(), variant.clone())),
                            "ambiguous {name}::{variant}"
                        );
                    }
                }
            }
        }
    }
}

fn covers(name: &str) -> bool {
    OUTCOMES.contains(&name)
        || ["Error", "Refusal", "Notice", "NoticeKind"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn documented_variants() -> BTreeSet<String> {
    let mut documented = BTreeSet::new();
    // The wallet bindings' section lists their own codes, checked by the bindings' tests.
    let mut bindings = false;
    let sdk_lines = GUIDE.lines().filter(|line| {
        if let Some(heading) = line.strip_prefix("## ") {
            bindings = heading == "Wallet errors";
        }
        !bindings
    });
    for row in sdk_lines.filter_map(|line| line.strip_prefix("| `")) {
        let (name, explanation) = row.split_once("` | ").expect("case and explanation");
        assert!(
            !explanation.trim_end_matches('|').trim().is_empty(),
            "empty explanation for {name}"
        );
        assert!(documented.insert(name.to_owned()), "duplicate {name}");
    }
    documented
}
