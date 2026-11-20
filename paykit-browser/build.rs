//! Gives this crate's wasm32 test binaries a 2 MiB stack instead of the linker's 1 MiB default.
//!
//! The pay flow in `tests/pay_flow/` and the network flow in `tests/switch_flow/` overflow
//! 1 MiB in the debug test build. Measured with `-zstack-size`, in Chromium, Firefox and
//! WebKit, in a page and in a dedicated worker, in steps of 64 KiB: 1216 KiB passes and
//! 1152 KiB fails for each (`--release`, in steps of 32 KiB: 192 KiB passes, 160 KiB fails).
//! 2 MiB leaves about 68 percent above the largest need measured. The setting is for the test
//! binaries only: a web app that links Paykit into its own WebAssembly module sets that
//! module's stack size itself.
//!
//! This linker argument comes after those of `RUSTFLAGS` and `cargo rustc -- -C link-arg=...`,
//! so it wins over them, as the initial value of `__stack_pointer` in each measurement binary
//! showed. To measure again, edit the `TEST_STACK_BYTES` const below to each size and build.
//!
//! It also lists Paykit's vector folder, `specs/v0.2/vectors/`, for the tests in
//! `tests/vectors/`: a browser has no file system to read the files from, and a file added to
//! the folder later has to reach those tests without anyone editing them, so that they can
//! fail until the file is run or named as not run. For the same reason it lists the sources of
//! paykit-lib's vector tests, `paykit-lib/tests/vectors/`, which those tests include, so that
//! a source added there fails them until it is included or named as not included. The build
//! script runs on the host for every target, so it can list both folders for the wasm32 build
//! too. When a folder cannot be read, it warns and writes an empty listing, which those tests
//! refuse.
//!
//! A build script cannot tell a test build from any other, so the listings are written, and
//! watched, for every build of this crate: a change to a vector file or to a source of
//! paykit-lib's vector tests also rebuilds this library and what depends on it, such as the
//! harness bundle in `rig/harness/`. The library itself does not use the listings.

use std::fmt::{Display, Write as _};
use std::path::Path;

/// The stack size of the wasm32 test binaries, in bytes.
const TEST_STACK_BYTES: u32 = 2 * 1024 * 1024;

/// Paykit's vector folder, relative to this crate.
const VECTORS_DIR: &str = "../specs/v0.2/vectors";

/// The folder of paykit-lib's vector test sources, relative to this crate.
const LIB_VECTOR_TESTS_DIR: &str = "../paykit-lib/tests/vectors";

/// The file in `OUT_DIR` that holds the listings, which `tests/vectors/` includes.
const VECTOR_FILES_RS: &str = "vector_files.rs";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-tests=-zstack-size={TEST_STACK_BYTES}");
    }
    write_listings();
}

/// Write `VECTOR_FILES`, every entry of the vector folder by name, sorted, with the text of
/// each `.json` file built in through `include_str!` and none for any other entry, and
/// `VECTOR_TEST_SOURCES`, the name of each top-level source of paykit-lib's vector test folder,
/// sorted: each `.rs` file, and `<folder>/mod.rs` for each folder that holds a `mod.rs`.
fn write_listings() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let vectors = Path::new(&manifest_dir).join(VECTORS_DIR);
    let sources = Path::new(&manifest_dir).join(LIB_VECTOR_TESTS_DIR);

    let mut out = String::new();
    out.push_str(
        "/// Every file of Paykit's vector folder, `specs/v0.2/vectors/`, as the build script\n\
         /// listed it: each file's name, sorted, with the text of each `.json` file, and `None`\n\
         /// for any other entry, such as a folder.\n\
         pub const VECTOR_FILES: &[(&str, Option<&str>)] = &[\n",
    );
    // `include_str!` takes the path as a string, so a folder whose path is not UTF-8 is not
    // listed. Its entries' names are UTF-8 ([`list_folder`]), so each file's path is too.
    let vector_entries = match vectors.to_str() {
        Some(_) => list_folder(&vectors),
        None => unlisted(&vectors, "not a UTF-8 path"),
    };
    for (name, is_file) in vector_entries {
        let path = vectors.join(&name);
        match path.to_str() {
            Some(path) if is_file && name.ends_with(".json") => {
                writeln!(out, "    ({name:?}, Some(include_str!({path:?}))),").unwrap();
            }
            _ => writeln!(out, "    ({name:?}, None),").unwrap(),
        }
    }
    out.push_str(
        "];\n\n\
         /// The top-level sources of paykit-lib's vector test folder,\n\
         /// `paykit-lib/tests/vectors/`, as the build script listed them, sorted: each `.rs`\n\
         /// file, and the `mod.rs` of each folder that holds one, as `<folder>/mod.rs`.\n\
         pub const VECTOR_TEST_SOURCES: &[&str] = &[\n",
    );
    for (name, is_file) in list_folder(&sources) {
        if is_file && name.ends_with(".rs") {
            writeln!(out, "    {name:?},").unwrap();
        } else if !is_file && sources.join(&name).join("mod.rs").is_file() {
            writeln!(out, "    {:?},", format!("{name}/mod.rs")).unwrap();
        }
    }
    out.push_str("];\n");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let path = Path::new(&out_dir).join(VECTOR_FILES_RS);
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

/// The entries of the folder `dir`, but stray ones ([`is_stray`]), sorted by name, each with
/// whether it is a file, following links. Cargo runs this script again when the folder, or an
/// entry of it, is added, removed or changed; while the folder is missing, it runs it again on
/// every build. Without the folder, as in a copy of this crate outside the repository, or with
/// an entry that cannot be read or whose name is not UTF-8, it warns and lists nothing, so the
/// library still builds and the vector tests fail.
fn list_folder(dir: &Path) -> Vec<(String, bool)> {
    // Before the folder is read, so that a folder that cannot be read yet is read again once
    // it changes, rather than when this script does.
    println!("cargo:rerun-if-changed={}", dir.display());
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return unlisted(dir, e),
    };
    let mut listed = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return unlisted(dir, e),
        };
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(name) => return unlisted(dir, format!("{name:?}: not a UTF-8 file name")),
        };
        if !is_stray(&name) {
            listed.push((name, entry.path().is_file()));
        }
    }
    listed.sort();
    listed
}

/// Warn that the folder `dir` cannot be listed, for `problem`, and list nothing.
fn unlisted(dir: &Path, problem: impl Display) -> Vec<(String, bool)> {
    println!(
        "cargo:warning={}: {problem}; the vector tests will fail",
        dir.display()
    );
    Vec::new()
}

/// Whether `name` is a file no one puts in the set on purpose, which the repository ignores or
/// a tool leaves behind: a hidden file such as `.DS_Store`, an editor's swap or backup file, or
/// the `.json.partial` file paykit-lib's regeneration writes before renaming it into place.
fn is_stray(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with(".swp")
        || name.ends_with('~')
        || name.ends_with(".partial")
}
