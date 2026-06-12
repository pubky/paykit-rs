# Releasing paykit-ffi

## 1. Build

From `paykit-ffi/`:

```bash
./build.sh -r --rc all           # RC: 0.1.0 → 0.1.0-rc1 → 0.1.0-rc2 → ...
./build.sh -r --minor --rc all   # Minor RC: 0.1.0 → 0.2.0-rc1
./build.sh -r all                # Finalize RC: 0.1.0-rc2 → 0.1.0
./build.sh -r --patch all        # Patch: 0.1.0 → 0.1.1
```

This bumps the version consistently in `paykit-ffi/Cargo.toml`,
`paykit-lib/Cargo.toml`, `paykit-sdk/Cargo.toml`, the SDK's `paykit-lib`
dependency, the root `Package.swift`, and `gradle.properties`, then builds both
platforms and updates the `Package.swift` checksum.

Always build all platform bindings together with `all` so Swift and Kotlin stay
in sync. The internal `build_ios.sh` and `build_android.sh` scripts are only for
local sub-build debugging.

## 2. Commit

```bash
git add -A
git commit -m "Release v<VERSION>"
git push origin HEAD
```

## 3. Create GitHub release

1. Go to https://github.com/pubky/paykit-rs/releases/new
2. Select the tag, attach `paykit-ffi/bindings/ios/Paykit.xcframework.zip`.
3. Publish.

iOS consumers resolve the SPM package from this release automatically.

## 4. Publish Android

The `Gradle Package` workflow triggers automatically on release publish. If it didn't, run it manually from Actions → "Gradle Package" → Run workflow with the version.

Requires the `ORG_PACKAGES_TOKEN` secret with `write:packages` scope.
