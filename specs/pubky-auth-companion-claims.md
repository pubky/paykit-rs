# Pubky Auth Companion Claims

A companion claim is an application-defined, signed, encrypted payload sent
alongside one Pubky Auth approval. In simple terms, it lets an app say “approve
this identity and also give the server this extra account data” as one
fail-closed operation.

Paykit implements the common security and transport protocol. The integrating
application owns the claim name, capability scope, and unsigned payload schema.
This keeps application-specific formats out of Paykit without requiring Swift
or Kotlin code to implement channel derivation, signatures, nonces, encryption,
or relay delivery.

## Inputs

`approve_auth_with_companion_claim` accepts:

- a sign-in-grant or signup-grant `pubkyauth://` URL
- the exact expected Pubky capability text
- the approving Pubky local identity key
- a `PubkyAuthCompanionClaim` containing:
  - `query_parameter`: the URL parameter announcing the claim
  - `claim_type`: the value required for that parameter
  - `unsigned_payload`: application-serialized bytes

The two identifiers must be non-empty ASCII protocol tokens containing only
letters, digits, hyphens, underscores, and dots. The SDK does not interpret or
validate the application's payload schema.

The URL must contain exactly one query pair matching the supplied identifiers,
for example:

```text
x-example-claim=account-export-v1
```

The request capabilities must exactly match `expected_capabilities`, and its
client ID must match the stable app-owned client ID of the session bootstrap.
The URL must also contain one valid 32-byte base64url-no-pad `secret` and one
absolute HTTP(S) `relay` URL. Duplicate, missing, empty, or mismatched values
are invalid.

## Request-Bound Signature

Paykit appends a 64-byte Ed25519 signature from the approving Pubky identity to
the application's unsigned payload. The signature input is:

```text
UTF8(query_parameter || "|" || claim_type || "|")
|| SHA256(decoded_auth_secret)
|| unsigned_payload
```

The signed plaintext is therefore:

```text
unsigned_payload || ed25519_signature[64]
```

The recipient verifies the signature with the issuer Pubky public key from the
grant. This binds the payload to the approving identity, the claim
protocol, and the individual auth request. Learning only the auth/relay secret
does not allow a third party to substitute a different signed payload.

## Relay Channel And Encryption

The companion relay channel identifier is:

```text
base64url_no_pad(
    BLAKE3(UTF8(claim_type) || UTF8("|") || decoded_auth_secret)
)
```

The signed plaintext is encrypted with XSalsa20-Poly1305 secretbox using the
decoded auth secret as its 32-byte key and a fresh random 24-byte nonce. The
relay body is:

```text
nonce[24] || secretbox_ciphertext_and_tag
```

Paykit posts that body through Pubky's HTTP relay inbox implementation using
the claim-specific channel identifier.

## Approval Ordering And Errors

The SDK performs these steps in order:

1. Validate the claim identifiers, auth URL, claim type, capability, secret,
   and relay.
2. Sign the application-provided unsigned payload for this auth request.
3. Encrypt and deliver the signed claim to its derived relay channel.
4. Only after successful claim delivery, approve the Pubky grant.

The grant is never delivered when claim validation, encryption, or relay
delivery fails. If grant authorization fails, the companion claim may
already be present on the relay, but it is not authorization by itself.

Callers receive distinct invalid-auth-URL, invalid-claim, encryption,
relay-delivery, and grant-authorization errors. Platform adapters may also
report an invalid local identity key before entering the protocol operation.

Auth URLs, decoded secrets, local Pubky secret keys, signed claims, and
plaintext payloads are sensitive and must not appear in normal logs or error
messages. Network deadlines remain the caller's Pubky client configuration
responsibility.

## Bitkit Watch-Only Account Example

Bitkit supplies these application-owned values:

```text
query_parameter = "x-bitkit-claim"
claim_type = "watch-only-account-v1"
expected_capabilities = "/pub/paykit/v0/bitkit/server/:rw"
```

For version 1, Bitkit serializes `unsigned_payload` as:

| Offset | Length | Value |
| ---: | ---: | --- |
| 0 | 1 | Protocol version (`1`) |
| 1 | 4 | BIP account index, unsigned big-endian |
| 5 | 1 | Address type (`0` = BIP84 native SegWit/P2WPKH) |
| 6 | 78 | Serialized account extended public key |

Paykit treats those 84 bytes as opaque application data, appends the
request-bound signature, and performs the shared transport protocol above.
Bitkit remains responsible for its version, BIP index, address type, and xpub
validation rules.
