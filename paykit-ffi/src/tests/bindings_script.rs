//! The bindings preparation step guards every generated host callback wrapper, and refuses
//! generated text it doesn't recognise (`tests/prepare_bindings_test.py`).

#[test]
fn test_bindings_preparation_guards_every_callback_wrapper() {
    let suite = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/prepare_bindings_test.py"
    );
    let out = std::process::Command::new("python3")
        .args(["-I", "-B", suite])
        .output()
        .expect("python3 runs the preparation tests");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
