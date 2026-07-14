# Bitkit Watch-Only Account Companion Claim

This document defines the `watch-only-account-v1` companion claim carried by a
Bitkit Pubky Auth approval. Paykit SDK implements this protocol as one
high-level operation so applications do not construct cryptographic messages or
relay channels themselves.

## Auth Request

The request is a normal sign-in or signup `pubkyauth://` URL with exactly one
additional query pair:

```text
x-bitkit-claim=watch-only-account-v1
```

The request and the caller's expected capability text must both name exactly:

```text
/pub/paykit/v0/bitkit/server/:rw
```

The URL must contain one valid 32-byte base64url-no-pad `secret` and one
absolute HTTP(S) `relay` URL. Duplicate, missing, empty, or unsupported values
are invalid.

## Claim Encoding

The SDK accepts a Base58Check account xpub and decodes it to its 78-byte binary
serialization. The signed plaintext has this fixed version-1 layout:

| Offset | Length | Value |
| ---: | ---: | --- |
| 0 | 1 | Protocol version (`1`) |
| 1 | 4 | BIP account index, unsigned big-endian |
| 5 | 1 | Address type (`0` = BIP84 native SegWit/P2WPKH) |
| 6 | 78 | Serialized account extended public key |
| 84 | 64 | Ed25519 signature by the approving Pubky identity |

Account indexes with the hardened bit already set are rejected. Version 1
supports only native SegWit accounts.

The first 84 bytes are the `unsigned_claim`. The signature input is:

```text
UTF8("x-bitkit-claim|watch-only-account-v1|")
|| SHA256(UTF8(percent_decoded_secret_query_value))
|| unsigned_claim
```

The secret query value is hashed as its percent-decoded base64url text for the
signature binding. Percent-decoding does not apply HTML-form semantics: a
literal `+` remains `+`. Channel derivation and encryption use the 32 bytes
obtained by base64url-no-pad decoding that value. The server verifies the
signature with the creator Pubky public key in the normal AuthToken. This binds
the xpub to both the approving identity and this individual auth request.

## Relay Channel And Encryption

The companion relay channel identifier is:

```text
base64url_no_pad(
    BLAKE3(UTF8("watch-only-account-v1") || UTF8("|") || decoded_auth_secret)
)
```

The 148-byte signed plaintext is encrypted with XSalsa20-Poly1305 secretbox
using the decoded auth secret as its 32-byte key and a fresh random 24-byte
nonce. The relay body is:

```text
nonce[24] || secretbox_ciphertext_and_tag[164]
```

The resulting body is 188 bytes. It is posted through Pubky's HTTP relay inbox
channel implementation using the claim-specific channel identifier.

## Approval Ordering And Errors

The SDK performs these steps in order:

1. Validate the auth URL, claim type, capability, and structured claim.
2. Encode and sign the request-bound claim.
3. Encrypt and deliver the companion claim to its relay channel.
4. After successful relay delivery, approve the normal Pubky AuthToken.

The normal AuthToken must never be delivered when claim validation, encryption,
or relay delivery fails. If normal authorization fails, the companion claim may
already be present on the relay and remains unusable as authorization by
itself.

Callers receive distinct invalid-auth-URL, invalid-claim, encryption,
relay-delivery, and normal-authorization errors. Platform adapters may also
report an invalid local identity key before entering the protocol operation.

Auth URLs, decoded auth secrets, local Pubky secret keys, signed claims, and
plaintext xpub payloads are sensitive. Implementations must not include these
values in normal logs or error messages. Network deadlines remain the caller's
Pubky client configuration responsibility.
