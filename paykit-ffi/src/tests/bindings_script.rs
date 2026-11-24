//! The bindings preparation step makes every generated callback wrapper catch any Throwable,
//! and fails when the generated text it expects isn't there.

use std::process::Command;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/prepare-bindings.py");

/// The four generated wrappers, as UniFFI writes them, each with its one catch.
fn wrappers() -> String {
    [
        "internal inline fun<T> uniffiTraitInterfaceCall(",
        "internal inline fun<T, reified E: Throwable> uniffiTraitInterfaceCallWithError(",
        "internal inline fun<T> uniffiTraitInterfaceCallAsync(",
        "internal inline fun<T, reified E: Throwable> uniffiTraitInterfaceCallAsyncWithError(",
    ]
    .iter()
    .map(|head| format!("{head}\n) {{\n    try {{\n    }} catch(e: kotlin.Exception) {{\n    }}\n}}\n"))
    .collect()
}

/// Run the script's callback patch on `kotlin` and return its exit status and output.
fn patch(kotlin: &str) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paykit.kt");
    std::fs::write(&path, kotlin).unwrap();
    let load = "import importlib.util, sys\n\
        spec = importlib.util.spec_from_file_location('prepare', sys.argv[1])\n\
        module = importlib.util.module_from_spec(spec)\n\
        spec.loader.exec_module(module)\n\
        text = open(sys.argv[2]).read()\n\
        sys.stdout.write(module.catch_throwable(text))\n";
    let out = Command::new("python3")
        .args(["-I", "-c", load, SCRIPT])
        .arg(&path)
        .output()
        .expect("python3 runs the preparation step");
    (out.status.success(), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn test_bindings_callbacks_catch_every_throwable() {
    let (ok, patched) = patch(&wrappers());
    assert!(ok);
    assert_eq!(patched.matches("catch(e: kotlin.Throwable)").count(), 4);
    assert!(!patched.contains("catch(e: kotlin.Exception)"));
}

#[test]
fn test_bindings_preparation_fails_without_the_generated_catch() {
    // A generator upgrade that renames a wrapper, or changes its catch, stops the step.
    let renamed = wrappers().replace(
        "uniffiTraitInterfaceCallAsync(",
        "uniffiTraitInterfaceCallAsyncRenamed(",
    );
    let changed = wrappers().replacen("catch(e: kotlin.Exception)", "catch(e: Exception)", 1);
    for kotlin in [renamed, changed, String::new()] {
        assert!(!patch(&kotlin).0);
    }
}
