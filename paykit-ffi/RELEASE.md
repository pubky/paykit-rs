# Release the wallet bindings

The Swift package and Android AAR use the version in the root Cargo.toml.
Build both from a clean checkout on macOS. Fetch the tags and check GitHub
Releases and GitHub Packages for the next unused version.

A release, its tag and a package publication are the maintainers' decision.
Nothing here runs by itself: the build scripts change only your checkout, and the
publish workflow runs only for a release a maintainer publishes, or by hand.

## Before the first release

- Decide who may publish releases, and protect `v*` tags so only they can push them.
- Enable GitHub Packages for the repository, and let its Actions workflows write
  packages. The publish workflow then uses the automatic `GITHUB_TOKEN`; no other
  secret is needed.
- Confirm the package name `com.synonym:paykit-android` and the Swift package's
  release URL in `paykit-ffi/swift/Package.release.swift`.
- There is no `Package.swift` at the repository root until the first release: a
  release build adds it, with that release's tag and archive checksum, and the
  release commit carries it with the generated Swift sources.

## Prerequisites

- Install Rust 1.91.1 or newer, Python 3.11 or newer, Git, zip and unzip.
- Select Xcode with the iOS device and simulator SDKs and Swift 5.9 or newer.
- Install JDK 17, Gradle 9.9.0, the Android SDK for API 36 and the NDK (r28c works
  with API 28). Set `JAVA_HOME`, `ANDROID_HOME` and `ANDROID_NDK_HOME` to those
  installations.
- Configure Git commit and tag signing with your registered signing key.
- Allow the repository's Actions workflow to publish packages at
  `https://maven.pkg.github.com/pubky/paykit-rs`. The publish job uses the
  automatic `GITHUB_TOKEN` with `packages: write` and the workflow actor.
  GitHub Packages accepts the AAR without a Maven signing key. Building the
  static XCFramework requires no Apple signing certificate.

Install the Rust targets and the Android build tool before building.

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk --version 4.1.2 --locked
```

## Build both packages

From the repository root, run `paykit-ffi/build.sh all` to check the current
version. For a release, choose one command from the following table.

| Command | New workspace version |
| --- | --- |
| `paykit-ffi/build.sh -r --rc all` | First candidate for the current base version, or the next candidate |
| `paykit-ffi/build.sh -r all` | Final version of an alpha or candidate, or the next patch of a final version |
| `paykit-ffi/build.sh -r --patch all` | Next patch |
| `paykit-ffi/build.sh -r --minor all` | Next minor with patch zero |

Add `--rc` to a patch or minor bump to start that version at `-rc1`.
The three crates inherit the workspace version. The build updates the exact
workspace dependency, lockfile and Android version together. After both builds
succeed, a release build writes `Package.swift` at the repository root from
`paykit-ffi/swift/Package.release.swift`, with the release tag and the archive's
checksum. A build without `-r` changes no version and writes no `Package.swift`.

The iOS build writes the prepared Paykit.swift and WalletRedaction.swift under
`paykit-ffi/bindings/ios/` and the release archive at
`paykit-ffi/dist/ios/Paykit.xcframework.zip`. Keep this exact archive for upload.
The AAR is `paykit-ffi/bindings/android/lib/build/outputs/aar/lib-release.aar`.
For local debugging, run `paykit-ffi/build_ios.sh` or `paykit-ffi/build_android.sh`.

## Commit and tag

1. Inspect the version changes and both generated Swift files. Run the workspace
   format, Clippy, test and documentation checks.
2. Stage Cargo.toml, Cargo.lock, Package.swift, the Android gradle.properties,
   and both generated Swift files. Create a signed commit with `git commit -S`.
3. Verify the signatures of all commits in the PR range before pushing the
   release branch. Check GitHub verifies every commit and wait for package CI.
4. On the approved release commit, create a fresh signed tag using
   `git tag -s "v$VERSION" -m "Release v$VERSION"`, with `VERSION` set to the
   workspace version. Verify and push that tag.

## Publish

1. Run `swift package compute-checksum paykit-ffi/dist/ios/Paykit.xcframework.zip`
   and compare it with the checksum in the tagged Package.swift.
2. Create a GitHub release for that tag. Attach the exact verified
   `Paykit.xcframework.zip` and publish the release.
3. The **Gradle Package** workflow checks out the tag, builds the AAR and
   publishes `com.synonym:paykit-android:$VERSION` to GitHub Packages.
   If publication has not occurred, the workflow also accepts the version
   or tag as a manual input.

Never reuse a published tag or replace its artifacts. A correction needs
a new version, a new signed tag and the matching archive and checksum.
