"""THROWAWAY (#411): work around a ubrn 0.29.3-1 + wasm-bindgen 0.2.115 bug.

ubrn's generated wasm shim declares one exported struct per async-callback
completion type, all with the Rust name `JsCallbackFn` in different modules
and told apart only by `#[wasm_bindgen(js_name = ForeignFutureCompleteXxx)]`.
wasm-bindgen 0.2.115 merges them into ONE JS class (ForeignFutureCompleteU64)
holding 13 `call` methods, so JavaScript keeps only the last one
(ForeignFutureCompleteVoid). Every async foreign-trait method that returns a
value then completes through the void shim, and Rust reads a garbage
ForeignFutureResult: "null RustBuffer had non-zero capacity" or "buffer
capacity negative or overflowed", followed by "cannot recursively acquire
mutex" in RustFuture::free.

This renames each struct to its js_name so every class is distinct. It is
idempotent. A production fix belongs upstream in ubrn's wasm shim template
(unique Rust idents), or in pinning a wasm-bindgen that keys exported classes
by js_name. Usage: python3 -I patch_wasm_shim.py <path/to/paykit_module.rs>
"""

import re
import sys


def block_end(src: str, open_brace: int) -> int:
    """Index just past the brace that closes the one at `open_brace`."""
    depth = 0
    for i in range(open_brace, len(src)):
        if src[i] == "{":
            depth += 1
        elif src[i] == "}":
            depth -= 1
            if depth == 0:
                return i + 1
    raise ValueError("unbalanced braces")


def main(path: str) -> int:
    with open(path, encoding="utf-8") as f:
        src = f.read()
    pieces = []
    renames = []
    pos = 0
    for m in re.finditer(r"^mod (foreign_future_complete_\w+) \{\n", src, re.M):
        end = block_end(src, m.end() - 2)
        block = src[m.start():end]
        js_name = re.search(r"js_name = (ForeignFutureComplete\w+)\)", block)
        if js_name is None:
            continue
        ident = js_name.group(1)
        pieces.append(src[pos:m.start()])
        pieces.append(re.sub(r"\bJsCallbackFn\b", ident, block))
        pos = end
        renames.append((m.group(1), ident))
    pieces.append(src[pos:])
    patched = "".join(pieces)
    # Qualified uses from the vtable modules, e.g.
    # `foreign_future_complete_rust_buffer::JsCallbackFn`.
    for module, ident in renames:
        patched = patched.replace(f"{module}::JsCallbackFn", f"{module}::{ident}")
    with open(path, "w", encoding="utf-8") as f:
        f.write(patched)
    print(f"patch_wasm_shim: {len(renames)} completion structs have unique names")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
