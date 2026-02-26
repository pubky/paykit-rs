# Paykit Library

Stateless Rust crate that implements the Paykit transport layer. It orchestrates reads from Paykit public storage and writes to private/public paths while delegating authentication to callers.

Paykit replies on **transport** protocol for network *communication* between peers and on **social media** protocol for bootstrap of *profile* and *social graph*. 

The default transport protocol in this implementation is [pubky](https://pubky.org/) and default social media protocol is [pubky.app](https://pubky.app/).
Both of them are enabled via default feature flag `pubky`.

## Auth & Dependency Injection

- Writes require an authenticated client. Instead of hard-coding `PubkySession`, public APIs accept an argument that implements a thin Paykit-defined trait (e.g., `AuthenticatedTransport`).  
- The crate provides adapters so callers can wrap [`pubky::PubkySession`](https://docs.rs/pubky/0.6.0/pubky/struct.PubkySession.html) or provide mocks for tests.  
- Public reads only require the `UnauthenticatedTransportRead` trait, keeping unauthenticated flows lightweight. Session lifecycle, capability scoping, and key rotation stay outside this crate.
- The `pubky` feature flag (enabled by default) wires in Pubky adapters under `transport::pubky`. Disable it if you want to use custom transports only.

## Timeout Handling

The transport traits intentionally do **not** enforce timeouts. Each transport implementation is responsible for configuring appropriate timeout behaviour at its own layer. A slow or unresponsive backend will block the caller indefinitely unless the underlying transport applies a timeout.

For the Pubky adapter the underlying SDK handles this via [`PubkyHttpClientBuilder::request_timeout`](https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout):

```rust
use std::time::Duration;
let client = PubkyHttpClient::builder()
    .request_timeout(Duration::from_secs(10))
    .build()?;
```

Custom transport implementations should apply equivalent safeguards (e.g. per-request deadlines, connect timeouts) before passing the transport to Paykit APIs.

## Core Types

### `MethodId`

Identifier for a payment method specification (e.g. `"lightning"`, `"onchain"`, `"bolt11"`).
`MethodId` is validated at construction time to prevent path injection attacks.

```rust
// Construction is fallible:
let method = MethodId::new("lightning")?;

// Read access via as_str(), Display, or AsRef<str>:
println!("{}", method.as_str());
```

**Allowed values:** 1-64 ASCII characters from the set `[a-zA-Z0-9_-.]`. The value must not consist solely of dots (`.` and `..` are rejected as path traversal components). Slashes, null bytes, spaces, and other special characters are rejected.

`MethodId::new()` returns `Err(PaykitError::Validation(...))` for invalid input.

### `EndpointData`

Serialized payload served by a payment endpoint (UTF-8 text such as JSON, lnurl, etc.).

```rust
let data = EndpointData::new("{\"bolt11\":\"ln...\"}");
let payload: &str = data.as_str();
let owned: String = data.into_inner();
```

### `PaykitError`

Domain error enum with the following variants:

| Variant | Meaning |
|---------|---------|
| `Transport { context, source }` | Network or SDK failure |
| `NotFound(String)` | Requested resource does not exist (404/GONE) |
| `InvalidData { context, source }` | Fetched data is corrupt or structurally invalid |
| `Profile(String)` | Profile data is malformed |
| `Validation(String)` | Caller-supplied input failed validation (e.g. invalid `MethodId`) |

## Proposed Surface

- `set_payment_endpoint(client: impl AuthenticatedTransport, method: MethodId, data: EndpointData) -> Result<()>`  
  Store or update a payee-owned endpoint using the caller's authenticated client.
- `remove_payment_endpoint(client: impl AuthenticatedTransport, method: MethodId) -> Result<()>`  
  Remove previously published endpoint data for a given method.
- `get_payment_list(reader: impl UnauthenticatedTransportRead, payee: PublicKey) -> Result<SupportedPayments>`  
  Resolve the supported methods document for a public key. The result is empty when no endpoints are published.
- `get_payment_endpoint(reader: impl UnauthenticatedTransportRead, payee: PublicKey, method: &MethodId) -> Result<Option<EndpointData>>`  
  Convenience resolver for a single method. Returns `Ok(None)` when the endpoint is missing or empty.
- `get_known_contacts(reader: impl UnauthenticatedTransportRead, owner: &PublicKey) -> Result<Vec<PublicKey>>`  
  Retrieve all known contacts by listing `/pub/pubky.app/follows/`. Returns an empty vector when none are stored.

Method/endpoint naming follows the PMIP consensus described in the repository root `README.md`. Each API returns well-typed structures (enums/structs) that mirror the protocol specification so downstream clients can share the same serialization layer.  
When the `pubky` feature is enabled the crate exports:

- `transport::pubky::PAYKIT_PATH_PREFIX` (`/pub/paykit.app/v0/`) and `PUBKY_FOLLOWS_PATH` (`/pub/pubky.app/follows/`) to standardize path construction.  
- `PubkyAuthenticatedTransport` (wraps `PubkySession`) and `PubkyUnauthenticatedTransport` (wraps `pubky::PublicStorage`) as ready-to-use adapters that satisfy the traits above.
