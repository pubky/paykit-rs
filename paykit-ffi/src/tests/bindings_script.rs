//! The bindings preparation step makes every generated callback wrapper answer any Throwable,
//! and fails when the generated text it expects isn't there.

use std::process::Command;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/prepare-bindings.py");
const RETHROW: &str = "if (e !is kotlin.Exception || \
    e is kotlin.coroutines.cancellation.CancellationException) throw e";

/// A sync wrapper as UniFFI writes it: the catch writes the unexpected-failure status.
fn sync_wrapper(head: &str) -> String {
    format!(
        "{head}\n) {{\n    try {{\n        writeReturn(makeCall())\n    }} catch(e: kotlin.Exception) {{\n        \
         callStatus.code = UNIFFI_CALL_UNEXPECTED_ERROR\n        \
         callStatus.error_buf = FfiConverterString.lower(e.toString())\n    }}\n}}\n"
    )
}

/// An async wrapper as UniFFI writes it: the catch completes the future, then returns.
fn async_wrapper(head: &str) -> String {
    format!(
        "{head}\n) {{\n    val job = GlobalScope.launch coroutineBlock@ {{\n        val callResult = try {{\n            \
         makeCall()\n        }} catch(e: kotlin.Exception) {{\n            \
         handleError(UniffiRustCallStatus.create(UNIFFI_CALL_UNEXPECTED_ERROR, \
         FfiConverterString.lower(e.toString())))\n            return@coroutineBlock\n        }}\n        \
         handleSuccess(callResult)\n    }}\n}}\n"
    )
}

fn wrappers() -> String {
    [
        sync_wrapper("internal inline fun<T> uniffiTraitInterfaceCall("),
        sync_wrapper(
            "internal inline fun<T, reified E: Throwable> uniffiTraitInterfaceCallWithError(",
        ),
        async_wrapper("internal inline fun<T> uniffiTraitInterfaceCallAsync("),
        async_wrapper(
            "internal inline fun<T, reified E: Throwable> uniffiTraitInterfaceCallAsyncWithError(",
        ),
    ]
    .concat()
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
fn test_bindings_callbacks_answer_every_throwable_then_rethrow() {
    let (ok, patched) = patch(&wrappers());
    assert!(ok);
    assert_eq!(patched.matches("catch(e: kotlin.Throwable)").count(), 4);
    assert!(!patched.contains("catch(e: kotlin.Exception)"));
    // Only fixed text crosses to Rust; the host's own text never does.
    assert!(!patched.contains("e.toString()"));
    assert_eq!(patched.matches("\"host callback failed\"").count(), 4);
    // Each wrapper answers first, then an Error or a cancellation goes on.
    assert_eq!(patched.matches(RETHROW).count(), 4);
    let fixed = "callStatus.error_buf = FfiConverterString.lower(\"host callback failed\")";
    assert_eq!(
        patched
            .matches(&format!("{fixed}\n        {RETHROW}"))
            .count(),
        2
    );
    assert_eq!(
        patched
            .matches(&format!("{RETHROW}\n            return@coroutineBlock"))
            .count(),
        2
    );
}

#[test]
fn test_bindings_preparation_fails_without_the_generated_catch() {
    // A generator upgrade that renames a wrapper, changes its catch, or stops writing the
    // unexpected-failure status stops the step.
    let renamed = wrappers().replace(
        "uniffiTraitInterfaceCallAsync(",
        "uniffiTraitInterfaceCallAsyncRenamed(",
    );
    let changed = wrappers().replacen("catch(e: kotlin.Exception)", "catch(e: Exception)", 1);
    let no_status = wrappers().replacen(
        "        callStatus.error_buf = FfiConverterString.lower(e.toString())\n",
        "",
        1,
    );
    let no_return = wrappers().replacen("            return@coroutineBlock\n", "", 1);
    for kotlin in [renamed, changed, no_status, no_return, String::new()] {
        assert!(!patch(&kotlin).0);
    }
}
