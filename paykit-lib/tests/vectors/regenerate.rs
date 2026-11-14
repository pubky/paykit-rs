//! The regeneration driver: every file of the set built as one, in dependency order, from one
//! in-memory corpus (vectors README "How the files were generated").
//!
//! It starts from the files on disk, for what they author, and runs each file's generator in
//! [`generators`] order. A generator reads the files it builds on from the corpus, as their
//! own generators have just made them, and writes its cases into the corpus, never to disk.
//! Without [`WRITE_VECTORS`] the built set must be the set on disk byte for byte. With it, the
//! driver writes every file that differs, then runs every check of the set against the files
//! now on disk, and fails if any of them differs from what it built.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::common::{file_bytes, file_text, in_corpus, vectors_dir, writing, Keys, WRITE_VECTORS};

/// Each generated file with its generator, in the order they build on one another: the
/// certificates and keys first; then the messages' JSON; the identifiers, whose request and
/// terms hashes are of those messages; the documents, whose grant carries the terms hash;
/// the signatures, of that grant; then the files that build only on their own authored
/// members; and last the sealed records, which carry the messages, identifiers and grant.
fn generators() -> [(&'static str, fn(&Keys)); 9] {
    [
        ("keys.json", crate::keys::generate),
        ("messages.json", crate::messages::generate),
        ("identifiers.json", crate::identifiers::generate),
        ("documents.json", crate::documents::generate),
        ("signatures.json", crate::signatures::generate),
        ("allowance_usage.json", crate::allowance_usage::generate),
        ("periods.json", crate::periods::generate),
        ("encoding.json", crate::encoding::generate),
        ("sealed.json", crate::sealed::generate),
    ]
}

/// Every check of the set, file by file, then the writers'.
fn check_all() {
    crate::keys::check();
    crate::messages::check();
    crate::identifiers::check();
    crate::documents::check();
    crate::signatures::check();
    crate::allowance_usage::check();
    crate::periods::check();
    crate::encoding::check();
    crate::sealed::check();
    crate::writers::check();
}

/// Every file of the set on disk, by name.
fn on_disk() -> BTreeMap<String, Value> {
    let dir = vectors_dir();
    let mut set = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".json") {
            let file = serde_json::from_slice(&file_bytes(&name)).unwrap();
            set.insert(name, file);
        }
    }
    set
}

/// The set built from `from` by every generator in [`generators`] order, as file texts.
fn build(from: BTreeMap<String, Value>) -> BTreeMap<String, String> {
    let keys = Keys::from_seeds();
    let built = in_corpus(from, || {
        for (_, generate) in generators() {
            generate(&keys);
        }
    });
    built
        .iter()
        .map(|(name, file)| (name.clone(), file_text(file)))
        .collect()
}

/// The texts read as a corpus again.
fn parsed(texts: &BTreeMap<String, String>) -> BTreeMap<String, Value> {
    texts
        .iter()
        .map(|(name, text)| (name.clone(), serde_json::from_str(text).unwrap()))
        .collect()
}

/// Write `text` as the file `name`, beside it first and renamed into place, so no reader
/// takes it half written.
fn write(name: &str, text: &str) {
    let path = vectors_dir().join(name);
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text).unwrap_or_else(|e| panic!("{}: {e}", partial.display()));
    std::fs::rename(&partial, &path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

#[test]
fn test_vectors_set_regenerates_in_dependency_order() {
    let names: Vec<&str> = generators().iter().map(|(name, _)| *name).collect();
    let disk = on_disk();
    for name in disk.keys() {
        assert!(names.contains(&name.as_str()), "{name} has no generator");
    }
    let built = build(disk);
    // Built again from itself, the set changes no byte: every generator took the files it
    // builds on as already generated, so the order is a dependency order and a second run
    // writes the same bytes.
    let again = build(parsed(&built));
    for (name, text) in &built {
        assert!(
            again.get(name) == Some(text),
            "{name}: a second build differs, so a generator ran before a file it builds on"
        );
    }
    if writing() {
        for (name, text) in &built {
            if file_bytes(name) != text.as_bytes() {
                write(name, text);
            }
        }
        check_all();
    }
    for (name, text) in &built {
        assert!(
            file_bytes(name) == text.as_bytes(),
            "{name} isn't what paykit-lib generates; regenerate the set with {WRITE_VECTORS} after an intended change"
        );
    }
}
