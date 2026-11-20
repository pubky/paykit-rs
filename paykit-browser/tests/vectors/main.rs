//! Paykit's vector set, `specs/v0.2/vectors/`, checked by paykit-lib's own vector tests,
//! natively and in a browser.
//!
//! The checks are paykit-lib's, unchanged: [`lib_vectors`] compiles the sources of
//! `paykit-lib/tests/vectors/` into this test target, and each test here runs one file's
//! `check()`, or the writers' check, with the same test body on both targets. Each test builds
//! for the host, under `cargo test -p paykit-browser`, and for `wasm32-unknown-unknown`, where
//! wasm-bindgen-test runs it in a browser page or in a dedicated worker.
//!
//! # Why the verdict is the native one
//!
//! A browser has no file system, so the files come from the build script, which lists the
//! folder and builds in the text of each `.json` file ([`VECTOR_FILES`]). Each test here runs
//! its check inside paykit-lib's `common::in_corpus`, with the built-in files, each parsed as a
//! [`serde_json::Value`], as the corpus:
//!
//! - paykit-lib's `paykit(name)`, through which its helpers read every file, reads the built-in
//!   file from the corpus, never the disk;
//! - the included files import `generated(name, cases)` from `crate::common`, which is this
//!   target's [`common`]: paykit-lib's `common.rs`, loaded as `lib_vectors::lib_common`, with
//!   this target's own `generated` in place of paykit-lib's. It compares every place with the
//!   built-in file exactly as paykit-lib's `generated` does outside a corpus: the value at a
//!   `Member` or `At` pointer, or the element of the `Case` list with the generated case's
//!   `name`, must equal the generated value. paykit-lib's `generated`, which writes into the
//!   corpus inside one instead of comparing, is never called;
//! - the corpus the check leaves must still equal the files it started with, so nothing wrote
//!   into it.
//!
//! The check's other assertions read the same values as natively, from the corpus here and
//! from disk there, and the tests natively read the same built-in files, so both targets run
//! the same bodies. A generated value that differs is reported once the check has returned,
//! with its file and the first place where it differs. Each file's test also requires its
//! check to have compared at least one place of that file, which shows that the check is the
//! file's own and that this target's `generated` ran. `test_vector_set_changed_case_fails`
//! changes one generated value of `keys.json` and shows that it is reported, and
//! `test_vector_set_corpus_write_fails` shows that a write into the corpus fails the run.
//!
//! A change in paykit-lib's test helpers fails loudly, never silently: a changed name or
//! signature fails to compile here, a helper that reads the disk despite the corpus panics on
//! wasm32, where `std::fs` returns an error, and a helper that wrote through paykit-lib's own
//! `generated` would change the corpus.
//!
//! # No silent gaps
//!
//! Every file of the folder is either checked by a test here ([`CHECKED`]) or named as not
//! run with a reason ([`NOT_RUN`]). Each checked file names its case lists and the members that
//! are data of a case, as JSON pointers. Going down from the top through the file's objects,
//! every array met, wherever it lies, must be named as one or the other; every other member met
//! there (every `description` left out) must be named, lie below a name, or hold one. So a file
//! added to the set later, an array added anywhere below the file's objects, or a member added
//! outside every named object fails `test_vector_set_every_file_run_or_named` or
//! `test_vector_set_every_case_list_named` until it is run or named. An array added inside a
//! named object fails too, as the walk goes down through every object. A member other than an
//! array added inside a named object, such as a case written as one object, or anything added
//! inside an element of an array, is not caught.
//!
//! Likewise every top-level source of `paykit-lib/tests/vectors/`, a `.rs` file or a folder's
//! `mod.rs`, is either included here ([`INCLUDED`]) or named as not included with a reason
//! ([`NOT_INCLUDED`]), so a source added there later fails
//! `test_vector_set_every_lib_source_included_or_named` until it is one or the other.
//!
//! A case is one element of a case list, or one case written as an object. Each checked file
//! must hold at least one case ([`file_cases`]), so a file whose case lists are all empty
//! cannot pass. The suite counts the cases each file holds and the places each check compares,
//! but it cannot count the cases a paykit-lib check's loops visit without changing paykit-lib,
//! and a case changed where a check's own assertions read it makes the check panic, which on
//! wasm32 cannot be caught.
//!
//! The checks of [`sensitivity`] add a few assertions paykit-lib's checks do not make: values
//! of the set that change when one of their inputs changes.
//!
//! # What runs where
//!
//! Natively, libtest also runs the `#[test]` functions of the included files: each file's own
//! test, which runs its check from disk, and the other tests of `sealed.rs` and `writers.rs`.
//! There no corpus is set, so this target's `generated` compares with the file on disk and
//! panics at the first difference, as paykit-lib's does.
//! wasm-bindgen-test runs only the tests of this target's own modules. The writers' tests are
//! the parts of `writers::check()`, so they run in a browser too; the other tests of
//! `sealed.rs` check rules on records built in code, are not reachable from a file's check,
//! and run natively only.
//!
//! On wasm32 a panic does not unwind, so when a check panics inside `in_corpus` the corpus
//! stays set, and may stay borrowed, and the tests that run after it in the same run fail with
//! a corpus or borrow error. The first failure is the one to read. A generated value that
//! differs fails after `in_corpus` has returned, and leaves the other tests unaffected.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::Value;

/// A test that runs under libtest natively and under wasm-bindgen-test on wasm32.
macro_rules! conformance_test {
    ($(#[$meta:meta])* fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
        #[cfg_attr(not(target_arch = "wasm32"), test)]
        fn $name() $body
    };
}

mod sensitivity;

/// Declare paykit-lib's vector test sources `$module` as modules of [`lib_vectors`], and list
/// their file names, with `common.rs`, in [`INCLUDED`], so the modules and the list cannot
/// differ.
macro_rules! lib_vector_modules {
    ($($module:ident),* $(,)?) => {
        /// paykit-lib's vector tests, `paykit-lib/tests/vectors/`, compiled here unchanged. The
        /// directory path on this inline module keeps each file's own module rules, so
        /// `sealed.rs` finds `sealed/publications.rs`. `common.rs` is loaded as `lib_common`,
        /// and the included files reach it through this target's [`common`]. No lint allow is
        /// needed here: `common.rs` allows its own unused items, and each file's own `#[test]`
        /// function uses the rest.
        #[path = "../../../paykit-lib/tests/vectors"]
        mod lib_vectors {
            $(pub mod $module;)*
            #[path = "common.rs"]
            pub mod lib_common;
        }

        // The included files name one another from the crate root (`crate::keys`).
        use lib_vectors::{$($module),*};

        /// The file name of each source of paykit-lib's vector tests [`lib_vectors`] includes.
        const INCLUDED: &[&str] = &["common.rs", $(concat!(stringify!($module), ".rs")),*];
    };
}

lib_vector_modules!(
    allowance_usage,
    documents,
    encoding,
    identifiers,
    keys,
    messages,
    periods,
    sealed,
    signatures,
    writers,
);

/// The top-level sources of paykit-lib's vector tests [`lib_vectors`] does not include, each
/// with the reason.
const NOT_INCLUDED: &[(&str, &str)] = &[
    // It declares paykit-lib's modules; this file declares them here instead.
    (
        "main.rs",
        "paykit-lib's own crate root, replaced by this file",
    ),
    // paykit-lib's regeneration driver; each check it runs once it has written runs here.
    (
        "regenerate.rs",
        "lists, reads and writes the folder on disk",
    ),
];

/// paykit-lib's vector helpers as the included files import them (`crate::common`): every item
/// of paykit-lib's `common.rs`, but `generated`, which is this target's own. An item defined
/// here shadows the one of the same name the glob brings in, so no included file can reach
/// paykit-lib's `generated`, which writes into the corpus instead of comparing.
mod common {
    pub use crate::lib_vectors::lib_common::*;

    /// The cases paykit-lib generates for the file `name`, compared with the file as
    /// paykit-lib's `generated` compares them outside a corpus ([`crate::compare_generated`]).
    pub fn generated(name: &str, cases: &[(Place<'_>, serde_json::Value)]) {
        crate::compare_generated(name, cases);
    }
}

// `VECTOR_FILES` and `VECTOR_TEST_SOURCES`, written by the build script.
include!(concat!(env!("OUT_DIR"), "/vector_files.rs"));

/// A file of the set that a test here checks with paykit-lib's check of it.
struct CheckedFile {
    /// The file's name in the folder.
    name: &'static str,
    /// The file's case lists, and its cases written as one object, as JSON pointers.
    cases: &'static [&'static str],
    /// The members of the file that are data of a case, not cases, as JSON pointers.
    inputs: &'static [&'static str],
}

/// Each file of the set a test here checks: its test, the file, paykit-lib's check of it, its
/// case lists (`cases`) and the members that are data of a case (`inputs`), as JSON pointers. A
/// case pointer to an array names a list of cases; one to an object names one case. Name a new
/// list only after reading that paykit-lib's check of the file runs it, or uses it as data.
macro_rules! checked_files {
    ($(
        $(#[$meta:meta])*
        $test:ident: $file:literal => $check:path,
            cases [$($case:literal),* $(,)?],
            inputs [$($input:literal),* $(,)?];
    )*) => {
        /// Each file of the set a test here checks, with its case lists and the arrays that
        /// are data of a case.
        const CHECKED: &[CheckedFile] = &[$(CheckedFile {
            name: $file,
            cases: &[$($case),*],
            inputs: &[$($input),*],
        }),*];

        $(conformance_test! {
            $(#[$meta])*
            fn $test() {
                refuse_while_writing();
                check_file($file, $check);
            }
        })*
    };
}

checked_files! {
    /// `allowance_usage.json`: Allowance usage, failure evidence and compaction.
    test_vector_set_allowance_usage: "allowance_usage.json" => allowance_usage::check,
        cases [
            "/cases",
            "/compaction",
            "/compaction/successors/cases",
            "/failure_evidence",
            "/period_limit_members/refused",
            "/period_limit_spellings/accepted",
            "/period_limit_spellings/refused",
        ],
        inputs [
            "/compaction/after/entries",
            "/compaction/after/totals/failed",
            "/compaction/before/entries",
            "/compaction/claims",
            "/compaction/terms/period_limits",
        ];
    /// `documents.json`: certificates, app records and the executor grant.
    test_vector_set_documents: "documents.json" => documents::check,
        cases [
            "/app_records/accepted",
            "/app_records/refused",
            "/certificate_rejections/cases",
            "/certificates/cases",
            "/executor_certificates/accepted",
            "/executor_certificates/refused",
            "/executor_grant",
            "/executor_grant_history_entry",
            "/extension_app",
            "/labelled_scope_rejections/cases",
            "/multi_protocol_certificates/accepted",
            "/multi_protocol_certificates/refused",
            "/payee_certificates/accepted",
            "/payee_certificates/refused",
            "/payer_certificates/accepted",
            "/payer_certificates/refused",
        ],
        inputs [];
    /// `encoding.json`: decimal amounts.
    test_vector_set_encoding: "encoding.json" => encoding::check,
        cases ["/amounts/invalid", "/amounts/valid"],
        inputs [];
    /// `identifiers.json`: request and terms hashes, obligations, policies and record keys.
    test_vector_set_identifiers: "identifiers.json" => identifiers::check,
        cases [
            "/obligations",
            "/policies/cases",
            "/record_keys/cases",
            "/request_hash",
            "/terms_hash",
        ],
        inputs [];
    /// `keys.json`: the fixed keys of the set.
    test_vector_set_keys: "keys.json" => keys::check,
        cases ["/apps", "/identities", "/scope_keys"],
        inputs [];
    /// `messages.json`: the messages and their rejections.
    test_vector_set_messages: "messages.json" => messages::check,
        cases [
            "/cancellation_roles/accepted",
            "/cancellation_roles/refused",
            "/conversion_amounts/cases",
            "/conversion_terms/accepted",
            "/conversion_terms/refused",
            "/label_rejections/cases",
            "/list_versions/refused",
            "/messages",
            "/multi_protocol_senders/accepted",
            "/multi_protocol_senders/refused",
            "/payee_label_senders/accepted",
            "/payee_label_senders/refused",
            "/proof_members/refused",
            "/receipt_members/accepted",
            "/receipt_members/refused",
        ],
        inputs ["/payee_label_senders/proof", "/payee_label_senders/request_recurring"];
    /// `periods.json`: recurrences and period details rates.
    test_vector_set_periods: "periods.json" => periods::check,
        cases ["/cases", "/details_rates/cases"],
        inputs ["/details_rates/created_at", "/details_rates/expires_at"];
    /// `sealed.json`: sealed records, their bindings and their rules.
    test_vector_set_sealed: "sealed.json" => sealed::check,
        cases [
            "/activity_records/accepted",
            "/activity_records/later_writes/accepted",
            "/activity_records/later_writes/refused",
            "/activity_records/refused",
            "/allowance_initial_ledger",
            "/allowance_initial_ledger/allower_proposal",
            "/allowance_initial_ledger/refused",
            "/allowance_proposals/accepted",
            "/allowance_proposals/refused",
            "/allowance_records/accepted",
            "/allowance_records/refused",
            "/binding_accepted",
            "/binding_rejections",
            "/labelled",
            "/labelled/rejections",
            "/msg_ids_bounds/cases",
            "/private_list_publications/later_writes",
            "/private_list_publications/writes",
            "/settlement_marks/accepted",
            "/settlement_marks/example",
            "/settlement_marks/merges",
            "/settlement_marks/refused",
        ],
        inputs [
            "/labelled/write_scopes",
            "/private_list_publications/message",
            "/private_list_publications/plaintext",
            "/settlement_marks/data_epoch",
            "/settlement_marks/example/marks",
            "/settlement_marks/identity_hex",
            "/settlement_marks/policy_hash",
            "/settlement_marks/read_scope",
            "/settlement_marks/scope_key_hex",
            "/settlement_marks/write_scopes",
        ];
    /// `signatures.json`: the executor grant, the ledger handoff and the recovery report.
    test_vector_set_signatures: "signatures.json" => signatures::check,
        cases ["/recovery_report", "/recovery_report/rejections", "/signatures"],
        inputs [
            "/recovery_report/disclosure/anchors",
            "/recovery_report/disclosure/decisions",
            "/recovery_report/disclosure/items",
            "/recovery_report/preimages_hex",
        ];
}

conformance_test! {
    /// paykit-lib's writers against its readers over the whole set.
    fn test_vector_set_writers() {
        refuse_while_writing();
        run_in_set(writers::check);
    }
}

/// The files of the folder no test here runs, each with the reason.
const NOT_RUN: &[(&str, &str)] = &[
    // The set's own documentation.
    ("README.md", "prose describing the set, not vectors"),
];

/// Fail the test calling this while paykit-lib's set is being written again
/// ([`common::writing`]): the built-in files predate the rewrite, and this target has no
/// regeneration driver, so its checks would prove nothing. A failure, not a skip, as libtest
/// shows nothing of a test that passes. On wasm32 the variable is never set.
fn refuse_while_writing() {
    assert!(
        !common::writing(),
        "{} is set: write the set with paykit-lib's own tests \
         (`cargo test -p paykit-lib --test vectors`), then run these tests without it",
        common::WRITE_VECTORS
    );
}

/// The set as built into the test binary: every `.json` file of [`VECTOR_FILES`], parsed
/// once per test binary.
fn built_in_set() -> &'static BTreeMap<String, Value> {
    static SET: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    SET.get_or_init(|| {
        VECTOR_FILES
            .iter()
            .filter_map(|(name, text)| {
                let file = serde_json::from_str((*text)?).unwrap_or_else(|e| panic!("{name}: {e}"));
                Some((name.to_string(), file))
            })
            .collect()
    })
}

/// The built-in `.json` file `name`, parsed.
fn built_in_file(name: &str) -> &'static Value {
    built_in_set()
        .get(name)
        .unwrap_or_else(|| panic!("{name}: not a .json file of the folder"))
}

/// What the check of one test compared, on the thread that runs it.
#[derive(Default)]
struct Compared {
    /// The number of places compared, by file name.
    places: BTreeMap<String, usize>,
    /// The first generated value that differs from its file, as a message naming the file and
    /// the place.
    mismatch: Option<String>,
}

thread_local! {
    /// What the check [`run_check`] runs on this thread has compared so far. Natively the tests
    /// run on parallel threads, so each test reads only its own check's comparisons.
    static COMPARED: RefCell<Option<Compared>> = const { RefCell::new(None) };
}

/// This target's `generated` ([`common::generated`]): compare each of `cases` with the file
/// `name` as paykit-lib's `paykit` reads it, as paykit-lib's `generated` does outside a
/// corpus. Inside [`run_check`], `paykit` reads the corpus, the places are counted, and the
/// first difference is held for `run_check` to report once the check has returned. Anywhere
/// else, as when libtest runs an included file's own test from disk natively, a difference
/// panics here, as paykit-lib's `generated` does.
fn compare_generated(name: &str, cases: &[(common::Place<'_>, Value)]) {
    let mismatch = first_mismatch(name, &common::paykit(name), cases);
    let unheld = COMPARED.with(|compared| match compared.borrow_mut().as_mut() {
        Some(compared) => {
            *compared.places.entry(name.to_owned()).or_default() += cases.len();
            compared.mismatch = compared.mismatch.take().or(mismatch);
            None
        }
        None => mismatch,
    });
    if let Some(mismatch) = unheld {
        panic!("{mismatch}");
    }
}

/// The first of `cases` whose value differs from the one `file`, the vector file `name`,
/// holds at its place, named by the file and the JSON pointer of the first difference, or
/// `None` when every value equals the file's. A `Member` or `At` place is the value at its
/// pointer; a `Case` place is the element of the array at its pointer whose `name` equals the
/// generated case's, as paykit-lib's `generated` finds them.
fn first_mismatch(
    name: &str,
    file: &Value,
    cases: &[(common::Place<'_>, Value)],
) -> Option<String> {
    use common::Place;
    cases.iter().find_map(|(place, value)| {
        let (at, held) = match place {
            Place::Member(pointer) => ((*pointer).to_owned(), file.pointer(pointer)),
            Place::At(pointer) => (pointer.clone(), file.pointer(pointer)),
            Place::Case(pointer) => {
                let list = file.pointer(pointer).and_then(Value::as_array);
                let Some(i) =
                    list.and_then(|list| list.iter().position(|c| c["name"] == value["name"]))
                else {
                    return Some(format!(
                        "{name}: {pointer} holds no case named {}",
                        value["name"]
                    ));
                };
                (format!("{pointer}/{i}"), list.and_then(|list| list.get(i)))
            }
        };
        let Some(held) = held else {
            return Some(format!(
                "{name}: nothing at {}, where paykit-lib generates a value",
                place_name(at)
            ));
        };
        first_difference(held, value, at)
            .map(|at| format!("{name}: {} isn't what paykit-lib generates", place_name(at)))
    })
}

/// A JSON pointer in a message: the empty pointer, the top of a file, as "the file".
fn place_name(at: String) -> String {
    if at.is_empty() {
        "the file".to_owned()
    } else {
        at
    }
}

/// Run `check` with `set` as paykit-lib's corpus, and return the number of places its
/// generated values were compared at, by file. It fails with the first generated value that
/// differs from its file, or, when nothing differs, with the first place where the corpus the
/// check leaves differs from `set`: this target's `generated` writes nothing, so a change there
/// means something wrote into the corpus.
fn run_check(
    set: &BTreeMap<String, Value>,
    check: fn(),
) -> Result<BTreeMap<String, usize>, String> {
    COMPARED.with(|compared| compared.replace(Some(Compared::default())));
    let held = ComparedGuard;
    let left = common::in_corpus(set.clone(), check);
    let compared = COMPARED
        .with(|compared| compared.take())
        .unwrap_or_default();
    drop(held);
    if let Some(mismatch) = compared.mismatch {
        return Err(mismatch);
    }
    if let Some(name) = left.keys().find(|name| !set.contains_key(*name)) {
        return Err(format!(
            "{name}: written into the corpus, which held no such file"
        ));
    }
    let changed = set.iter().find_map(|(name, file)| match left.get(name) {
        None => Some(format!("{name}: dropped from the corpus")),
        Some(left) => first_difference(file, left, String::new())
            .map(|at| format!("{name}: {} changed in the corpus", place_name(at))),
    });
    match changed {
        Some(changed) => Err(changed),
        None => Ok(compared.places),
    }
}

/// Clears [`COMPARED`] when dropped, also when a check panics and unwinds natively, so that a
/// later test on the same thread, such as an included file's own test, panics on a difference
/// instead of holding it for a check that has ended. paykit-lib's corpus has no such reset
/// without changing paykit-lib: after such a panic it still holds the built-in files, which
/// equal the files on disk, so reads give the same values, and the next [`run_check`] on that
/// thread fails with "one corpus at a time". On wasm32 a panic does not unwind.
struct ComparedGuard;

impl Drop for ComparedGuard {
    fn drop(&mut self) {
        COMPARED.with(|compared| compared.replace(None));
    }
}

/// Run `check` with the built-in set as paykit-lib's corpus ([`run_check`]), and return the
/// number of places compared, by file.
fn run_in_set(check: fn()) -> BTreeMap<String, usize> {
    run_check(built_in_set(), check).unwrap_or_else(|failure| panic!("{failure}"))
}

/// Run paykit-lib's check of the file `name` against the built-in set. The file must hold at
/// least one case, so a file whose case lists are all empty cannot pass, and the check must
/// compare at least one generated value of the file `name`, which shows that the check is that
/// file's and that this target's `generated` compared it. Natively, `--nocapture` prints the
/// number of places compared in each file.
fn check_file(name: &str, check: fn()) {
    assert!(file_cases(name) > 0, "{name}: holds no case");
    let compared = run_in_set(check);
    for (file, places) in &compared {
        println!("{name}: its check compared {places} places of {file}");
    }
    assert!(
        compared.get(name).is_some_and(|places| *places > 0),
        "{name}: the check compared no generated value of the file; pair the file with \
         paykit-lib's check of it"
    );
}

/// The JSON pointer of the first place where `actual` differs from `expected`, below `at`.
fn first_difference(expected: &Value, actual: &Value, at: String) -> Option<String> {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            let mut keys: Vec<&String> = e.keys().chain(a.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|key| {
                let at = format!("{at}/{}", pointer_token(key));
                match (e.get(key), a.get(key)) {
                    (Some(e), Some(a)) => first_difference(e, a, at),
                    _ => Some(at),
                }
            })
        }
        (Value::Array(e), Value::Array(a)) => (0..e.len().max(a.len())).find_map(|i| {
            let at = format!("{at}/{i}");
            match (e.get(i), a.get(i)) {
                (Some(e), Some(a)) => first_difference(e, a, at),
                _ => Some(at),
            }
        }),
        _ => (expected != actual).then_some(at),
    }
}

/// A member name as a JSON pointer writes it (RFC 6901).
fn pointer_token(member: &str) -> String {
    member.replace('~', "~0").replace('/', "~1")
}

/// What a file holds going down through its objects from the top, every `description` left
/// out: the JSON pointer of each object, of each array, below which nothing is visited, and of
/// each other value.
#[derive(Default)]
struct Shape {
    /// The objects below the top, reached through objects only.
    objects: Vec<String>,
    /// The arrays met first, reached through objects only.
    arrays: Vec<String>,
    /// The other values, reached through objects only.
    values: Vec<String>,
}

impl Shape {
    /// The shape of `file`.
    fn of(file: &Value) -> Self {
        let mut shape = Self::default();
        shape.visit_members(file, "");
        shape
    }

    /// Visit the members of `value`, at `at`, but `description`.
    fn visit_members(&mut self, value: &Value, at: &str) {
        let members = value.as_object().into_iter().flatten();
        for (member, value) in members.filter(|(member, _)| member.as_str() != "description") {
            self.visit(value, format!("{at}/{}", pointer_token(member)));
        }
    }

    fn visit(&mut self, value: &Value, at: String) {
        match value {
            Value::Array(_) => self.arrays.push(at),
            Value::Object(_) => {
                self.visit_members(value, &at);
                self.objects.push(at);
            }
            _ => self.values.push(at),
        }
    }

    /// Whether `at` is a member reached through objects only.
    fn holds(&self, at: &str) -> bool {
        [&self.objects, &self.arrays, &self.values]
            .into_iter()
            .flatten()
            .any(|held| held == at)
    }
}

/// The cases at the case pointer `at` of the file `name`: the elements of an array, or one
/// case written as an object.
fn cases_at(name: &str, at: &str) -> usize {
    match built_in_file(name).pointer(at) {
        Some(Value::Array(cases)) => cases.len(),
        Some(Value::Object(_)) => 1,
        _ => panic!("{name}: {at} is neither an array nor an object"),
    }
}

/// The cases the checked file `name` holds, over all its case pointers.
fn file_cases(name: &str) -> usize {
    let file = CHECKED
        .iter()
        .find(|file| file.name == name)
        .unwrap_or_else(|| panic!("{name}: not a checked file"));
    file.cases.iter().map(|at| cases_at(name, at)).sum()
}

conformance_test! {
    /// Every file of the vector folder is checked by a test here or named as not run, never
    /// both, and every file either list names is in the folder.
    fn test_vector_set_every_file_run_or_named() {
        let checked = |name: &str| CHECKED.iter().any(|file| file.name == name);
        let not_run = |name: &str| NOT_RUN.iter().any(|(file, _)| *file == name);
        assert!(
            !VECTOR_FILES.is_empty(),
            "the build script found no vector folder, ../specs/v0.2/vectors from this crate"
        );
        for (name, _) in VECTOR_FILES {
            assert!(
                checked(name) || not_run(name),
                "{name}: a file of the vector folder that no test here runs; add it to the \
                 checked files with paykit-lib's check of it, or to NOT_RUN with the reason"
            );
            assert!(
                !(checked(name) && not_run(name)),
                "{name}: both checked and in NOT_RUN"
            );
        }
        for file in CHECKED {
            built_in_file(file.name);
        }
        for (name, _) in NOT_RUN {
            assert!(
                VECTOR_FILES.iter().any(|(file, _)| file == name),
                "{name}: in NOT_RUN, but not a file of the folder"
            );
        }
    }
}

conformance_test! {
    /// Every top-level source of paykit-lib's vector tests is included here or named as not
    /// included, never both, and every source either list names is in paykit-lib's folder.
    fn test_vector_set_every_lib_source_included_or_named() {
        let included = |name: &str| INCLUDED.iter().any(|source| is_source(name, source));
        let not_included = |name: &str| {
            NOT_INCLUDED
                .iter()
                .any(|(source, _)| is_source(name, source))
        };
        assert!(
            !VECTOR_TEST_SOURCES.is_empty(),
            "the build script found no vector test sources, ../paykit-lib/tests/vectors from \
             this crate"
        );
        for name in VECTOR_TEST_SOURCES {
            assert!(
                included(name) || not_included(name),
                "{name}: a source of paykit-lib's vector tests that is not included here; add \
                 its module to lib_vector_modules!, or add it to NOT_INCLUDED with the reason"
            );
            assert!(
                !(included(name) && not_included(name)),
                "{name}: both included and in NOT_INCLUDED"
            );
        }
        for name in INCLUDED.iter().chain(NOT_INCLUDED.iter().map(|(source, _)| source)) {
            assert!(
                VECTOR_TEST_SOURCES
                    .iter()
                    .any(|listed| is_source(listed, name)),
                "{name}: named here, but not a top-level source of paykit-lib's vector tests"
            );
        }
    }
}

conformance_test! {
    /// A module of paykit-lib's vector tests is matched by its file, `<module>.rs`, or by its
    /// folder's `<module>/mod.rs`, and by nothing else.
    fn test_vector_set_source_names_match_module_files() {
        assert!(is_source("keys.rs", "keys.rs"));
        assert!(is_source("cases/mod.rs", "cases.rs"));
        assert!(!is_source("cases/mod.rs", "mod.rs"));
        assert!(!is_source("cases/mod.rs", "other.rs"));
        assert!(!is_source("keys.rs", "keys"));
    }
}

/// Whether `listed`, a source of paykit-lib's vector tests as the build script lists it, is the
/// module file `named`: the same name, or `<module>/mod.rs` for `<module>.rs`, the two files a
/// module declared in [`lib_vectors`] may be read from.
fn is_source(listed: &str, named: &str) -> bool {
    listed == named
        || listed
            .strip_suffix("/mod.rs")
            .is_some_and(|module| named.strip_suffix(".rs") == Some(module))
}

conformance_test! {
    /// In each checked file, every array met going down through its objects is named once,
    /// as a case list or as data of a case; every case pointer names such an array or an
    /// object, and every input pointer a member reached through objects; and every member
    /// reached through objects, every `description` left out, is named, lies below a name, or
    /// holds one.
    fn test_vector_set_every_case_list_named() {
        for file in CHECKED {
            let name = file.name;
            let shape = Shape::of(built_in_file(name));
            let named: Vec<&str> = file.cases.iter().chain(file.inputs).copied().collect();
            for at in &shape.arrays {
                assert!(
                    named.contains(&at.as_str()),
                    "{name}: {at} is an array the checked files do not name; add it to the \
                     file's cases if it is a case list paykit-lib's check runs, or to its \
                     inputs if the check uses it as data of a case"
                );
            }
            for (i, at) in named.iter().enumerate() {
                assert!(!named[..i].contains(at), "{name}: {at} is named twice");
            }
            for at in file.cases {
                assert!(
                    shape.arrays.iter().chain(&shape.objects).any(|held| held == at),
                    "{name}: the case pointer {at} names no array or object of the file \
                     reached through objects; correct it or remove it"
                );
            }
            for at in file.inputs {
                assert!(
                    shape.holds(at),
                    "{name}: the input pointer {at} names no member of the file reached \
                     through objects; correct it or remove it"
                );
            }
            let below = |at: &str, parent: &str| at.starts_with(&format!("{parent}/"));
            for at in shape.objects.iter().chain(&shape.arrays).chain(&shape.values) {
                assert!(
                    named
                        .iter()
                        .any(|held| held == at || below(at, held) || below(held, at)),
                    "{name}: {at} holds no case pointer and lies below none; name the case it \
                     holds, or its case lists, after confirming paykit-lib's check of the file \
                     covers them, add it to the file's inputs if the check uses it as data of \
                     a case, or list the file as not run with the reason"
                );
            }
        }
    }
}

conformance_test! {
    /// Every checked file holds at least one case. Natively, `--nocapture` prints the cases
    /// at each case pointer and in each file, which the README lists.
    fn test_vector_set_every_checked_file_holds_cases() {
        let mut total = 0;
        for file in CHECKED {
            let name = file.name;
            for at in file.cases {
                println!("{name} {at}: {}", cases_at(name, at));
            }
            let cases = file_cases(name);
            assert!(cases > 0, "{name}: holds no case");
            println!("{name}: {cases}");
            total += cases;
        }
        println!("all files: {total}");
    }
}

conformance_test! {
    /// A generated value that differs from its file is reported, with the file and the place:
    /// with one value of `keys.json` changed in the corpus, the comparison of `keys::check`
    /// names that place, after the check has returned and without a panic.
    fn test_vector_set_changed_case_fails() {
        refuse_while_writing();
        let at = "/identities/0/public_hex";
        let mut set = built_in_set().clone();
        let held = set
            .get_mut("keys.json")
            .and_then(|file| file.pointer_mut(at))
            .unwrap_or_else(|| panic!("keys.json: nothing at {at}"));
        let changed = format!("{}0", held.as_str().unwrap_or_else(|| panic!("{at}")));
        *held = Value::String(changed);
        assert_eq!(
            run_check(&set, keys::check),
            Err(format!("keys.json: {at} isn't what paykit-lib generates"))
        );
    }
}

/// A check that writes one value into the corpus with paykit-lib's own `generated`, which no
/// included file can reach.
fn write_into_corpus() {
    lib_vectors::lib_common::generated(
        "keys.json",
        &[(
            common::Place::At("/identities/0/public_hex".to_owned()),
            Value::String("00".to_owned()),
        )],
    );
}

conformance_test! {
    /// A value written into the corpus fails the run, so a check that wrote its generated
    /// values instead of comparing them could not pass.
    fn test_vector_set_corpus_write_fails() {
        refuse_while_writing();
        assert_eq!(
            run_check(built_in_set(), write_into_corpus),
            Err("keys.json: /identities/0/public_hex changed in the corpus".to_owned())
        );
    }
}
