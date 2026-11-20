#!/bin/sh
set -eu
if [ "$#" -ne 3 ]; then
    echo "Usage: $0 PAYKIT_BINDGEN NATIVE_LIBRARY OUTPUT_DIRECTORY" >&2
    exit 2
fi
bindgen=$1
library=$2
out=$3
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mkdir -p "$out/kotlin" "$out/swift"
"$bindgen" generate --no-format --metadata-no-deps --library "$library" --config "$dir/uniffi.toml" --language kotlin --out-dir "$out/kotlin"
"$bindgen" generate --no-format --metadata-no-deps --library "$library" --config "$dir/uniffi.toml" --language swift --out-dir "$out/swift"

python3 "$dir/prepare-bindings.py" "$out/kotlin/com/synonym/paykit/v2/paykit_v2.kt" "$out/swift/WalletRedaction.swift" "$out/swift/PaykitV2.swift"
cp "$dir/swift/WalletFuture.swift" "$out/swift/WalletFuture.swift"
