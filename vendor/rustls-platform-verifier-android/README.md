# Android rustls platform verifier support

This directory vendors the Android support artifact from
`rustls-platform-verifier` 0.6.2 (`rustls-platform-verifier-android` 0.1.1).
It contains one Paykit-specific correction for
[rustls-platform-verifier#221](https://github.com/rustls/rustls-platform-verifier/issues/221):
only `CertPathValidatorException.BasicReason.REVOKED` is reported as a revoked
certificate. Android's undetermined-status and known missing-CRL errors remain
soft failures, as intended by the upstream verifier's
`PKIXRevocationChecker.Option.SOFT_FAIL` policy. Other unexpected validator
errors still fail closed.

The missing-CRL check matches the Bouncy Castle message observed on current
Android releases: `No CRLs found for issuer `. This wording is not a stable
API. If Android changes it, the exception is returned as `UnknownCert` with
the original validator message instead of being accepted. The resulting
fail-closed error should make a future wording change visible without
weakening certificate validation.

The exact source and test changes are recorded in
`android-revocation-soft-fail.patch`. Apply that zero-context patch to the
upstream `v/0.6.2` tag, then run the following from its `android` directory to
reproduce the AAR:

```sh
git apply --unidiff-zero android-revocation-soft-fail.patch
./gradlew testReleaseUnitTest assembleRelease
```

The vendored AAR SHA-256 is:

```text
50330ab5c487fcd3960cf150d4b0caffea39330fc1c0e7143918aee600681eec
```
