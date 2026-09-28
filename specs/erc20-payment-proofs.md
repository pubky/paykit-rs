# ERC-20 Payment Proof profile

This document specifies the `erc20-transfer-eip712` method-specific proof profile
for direct ERC-20 transfers on EVM chains. It composes ERC-20 transfer receipts
with EIP-712 account signatures. It is a Paykit profile using Ethereum standards,
not a claim that existing wallets already implement this message format.

Paykit's generic proof message carries the profile. The wallet integration
constructs signatures, reads the chain and verifies settlement. Paykit Library
and SDK do not implement an EVM client or a token/signature verifier.

## Payload

```json
{
  "type": "erc20-transfer-eip712",
  "chain_id": "42161",
  "transaction_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
  "receipt_log_index": "0",
  "signature": "0x<65-byte recoverable signature>"
}
```

`chain_id` is a positive base-10 uint256 string with no leading zeroes. It MUST
equal the selected endpoint's chain ID, and the EIP-712 domain `chainId` MUST
equal that same numeric value. Other EVM chains use their own chain ID.
`transaction_hash` is a lowercase 0x-prefixed 32-byte hash.
`receipt_log_index` is a canonical base-10 uint256 string selecting the zero-based
position within that transaction receipt's **entire logs array**, not the
block-global RPC `logIndex` and not an index among Transfer events only. The value
MUST be `0` or start with a digit from `1` through `9`, MUST contain only ASCII
digits, and MUST be no greater than
`115792089237316195423570985008687907853269984665640564039457584007913129639935`
(`2^256 - 1`). JSON numbers, signs, leading zeroes, whitespace and exponent notation
MUST be rejected. Verifiers parse this string into the EIP-712 `uint256` value only
after these checks. This distinguishes batched payments and avoids tying payment
identity to a block-global position that can change after a reorganization. The
unique payment identity is the independently verified endpoint chain ID,
transaction hash and receipt-relative log index. Signatures and request IDs are
not part of the deduplication key.

The payload intentionally does not repeat the token, recipient or amount. They
are independently read from the selected event and compared with the accepted
endpoint and request. Token symbols never establish token identity.

## EIP-712 signature

The domain name and version are fixed; `chainId` is the verified endpoint chain.
For example, on Arbitrum One:

```json
{"name":"Paykit ERC20 Payment","version":"1","chainId":42161}
```

The domain has no `verifyingContract`: verification is off-chain and the token
contract is not a Paykit proof verifier. The type declarations, including field
order and spelling, are:

```text
EIP712Domain(string name,string version,uint256 chainId)
RequestBinding(string payer,string payee,string payerReceiverPath,string payeeReceiverPath,string paymentRequestId,string paymentReference,string paymentEndpointIdentifier,string periodStartsAt,string periodEndsAt,string conversionQuoteId)
Erc20Payment(bytes32 transactionHash,uint256 receiptLogIndex,RequestBinding request)
```

The EIP-712 `encodeType` for `Erc20Payment` appends the `RequestBinding` declaration
as required for referenced types. The primary type is `Erc20Payment`.

Populate the binding from the authenticated Encrypted Link and immutable request,
never from unauthenticated claims in the proof:

- `payer` and `payee`: canonical bare Pubky public key strings, without `pubky://`.
- `payerReceiverPath` and `payeeReceiverPath`: the exact Paykit Receiver Paths.
- `paymentRequestId`: the canonical lowercase UUID-v4 Payment Request ID.
- `paymentReference`: the exact accepted Payment Reference text.
- `paymentEndpointIdentifier`: the selected accepted endpoint identifier.
- `periodStartsAt` and `periodEndsAt`: the exact Billing Period timestamp strings
  carried by a recurring proof, or both empty strings for a one-time proof.
- `conversionQuoteId`: the canonical lowercase quote Event ID selected in the
  outer Payment Proof, or the empty string when no quote is selected.

The transaction hash and receipt-relative index match the payload. The request
ID plus parties/paths scopes immutable terms; a separate terms hash is not used.
This is a statement associating an executed transfer with a request, not an
allowance, token approval or permission to execute another payment.

Sign the EIP-712 digest with the token-sending account. The signature is 65 bytes
`r || s || v`, lowercase 0x-prefixed hexadecimal, with `v` equal to 27 or 28 and
low-s normalization. This profile specifies recoverable secp256k1 account
signatures, including an EIP-7702 account's owner address. Other contract-account
signature schemes need an explicitly agreed profile; do not interpret an arbitrary
contract owner's recovered address as the token-sending contract itself.

The transfer reference is known after execution, so integrations must preserve
payment identity through restart and separate proof-signing/delivery retries from
payment execution. They must never send again merely to obtain a proof.

## Verification

A receiver must:

1. Validate the authenticated payer, known request, accepted endpoint, Billing
   Period and selected quote under the ordinary Paykit rules. Resolve the expected
   chain, token contract and receiving address from that endpoint. Require the
   payload chain ID and EIP-712 domain chain ID to equal that verified chain.
2. Read the referenced transaction receipt from the expected chain. Verify its
   transaction hash, successful execution status, canonical block association and
   the application's confirmation policy. A missing or temporarily unavailable
   receipt is pending verification, not proof of failure.
3. Select the specified log. Require the expected token contract and an ERC-20
   `Transfer` event with the expected recipient and a positive amount. Read the
   sender and amount from the event. For supported tokens, use their verified
   transfer semantics; this profile does not make arbitrary token contracts honest.
4. Rebuild the EIP-712 message from authenticated request context and recover the
   signer. It must equal the Transfer event's sender, not the transaction envelope's
   sender: a bundler or relayer can be the latter.
5. Verify actual amount using the agreed rate and rounding, and payment time using
   the canonical block timestamp and the applicable request deadline and quote
   validity interval, including its start. A quote issued after payment cannot
   retroactively establish sufficient payment. A
   valid signature alone proves neither settlement nor sufficient payment.
6. Prevent the transfer identity from satisfying another request or Billing Period.
   Repeated proof delivery for the same association is idempotent. Handle chain
   reorganizations consistently with the wallet's existing history policy.

This is a receipt lookup plus account attestation, not a self-contained Merkle
inclusion proof. It inherits the wallet's chain/RPC trust model. No UserOperation
hash, bundler, paymaster or wallet implementation is prescribed. No extra on-chain
transaction or user gas fee is needed for the proof.

A custodial withdrawal or unsupported wallet can still transfer funds. Without
suitable account evidence, the wallet must not claim verified Paykit contact or
request attribution solely from a publicly visible transaction hash.

Underpaid and late transfers remain real received funds. Their request status is
an application concern; this profile does not prescribe dispute resolution.

## Message size

The complete outer Payment Proof must fit `PUBKY_NOISE_MSG_LEN` (currently 1000
UTF-8 bytes), including JSON escaping. A maximum-length Payment Reference can
make an otherwise valid proof too large. Requesters must budget for the chosen
profile before offering it; payers must verify the exact serialized size before
payment approval. Do not truncate or change an agreed reference after payment.

The ERC-20 transaction hash (66 ASCII bytes) and signature (132 ASCII bytes)
have fixed textual lengths. Preflight can use placeholders of those lengths,
the selected chain ID, the actual request/period/optional IDs, and 78 decimal digits for
the maximum uint256 receipt-log index (or a smaller bound guaranteed by the
selected chain). Do not use a short
example signature or omit optional IDs when measuring. No additional payment
should be sent to recover from proof-delivery failure.

## References and vectors

- [ERC-20 Transfer event](https://eips.ethereum.org/EIPS/eip-20)
- [EIP-712 typed signatures](https://eips.ethereum.org/EIPS/eip-712)
- [ERC-1271 contract signatures](https://eips.ethereum.org/EIPS/eip-1271), for a
  separately supported contract-account profile
- [Payment conversion and deadlines](payment-conversion.md)
- [ERC-20 proof vectors](fixtures/erc20-payment-proofs.json)

Vectors contain public test data only. They cover one-time and quoted recurring
bindings, expected digests and signatures, and changed-context rejection. They
are encoding/signature vectors, not evidence of live on-chain transfers.
