# Paykit Library

Stateless Rust crate that implements the Paykit transport layer. It orchestrates reads from Paykit public storage and writes to private/public paths while delegating authentication to callers.

## Auth & Dependency Injection

- Writes require an authenticated client. Instead of hard-coding `PubkySession`, public APIs accept an argument that implements a thin Paykit-defined trait (e.g., `AuthenticatedTransport`).  
- The crate provides adapters so callers can wrap [`pubky::PubkySession`](https://docs.rs/pubky/0.6.0-rc.6/pubky/struct.PubkySession.html) or provide mocks for tests.  
- Public reads only require the `UnauthenticatedTransportRead` trait, keeping unauthenticated flows lightweight. Session lifecycle, capability scoping, and key rotation stay outside this crate.
- The `pubky` feature flag (enabled by default) wires in Pubky adapters under `transport::pubky`. Disable it if you want to use custom transports only.

## Proposed Surface

- `set_payment_endpoint(client: impl AuthenticatedTransport, method: MethodId, data: EndpointData) -> Result<()>`  
  Store or update a payee-owned endpoint using the caller’s authenticated client.
- `remove_payment_endpoint(client: impl AuthenticatedTransport, method: MethodId) -> Result<()>`  
  Remove previously published endpoint data for a given method.
- `get_payment_list(reader: impl UnauthenticatedTransportRead, payee: PublicKey) -> Result<SupportedPayments>`  
  Resolve the supported methods document for a public key. The result is empty when no endpoints are published.
- `get_payment_endpoint(reader: impl UnauthenticatedTransportRead, payee: PublicKey, method: MethodId) -> Result<Option<EndpointData>>`  
  Convenience resolver for a single method. Returns `Ok(None)` when the endpoint is missing or empty.
- `get_known_contacts(reader: impl UnauthenticatedTransportRead) -> Result<Vec<PublicKey>>`  
  Retrieve all known contacts by listing `/pub/pubky.app/follows/`. Returns an empty vector when none are stored.

Method/endpoint naming follows the PMIP consensus described in the repository root `README.md`. Each API returns well-typed structures (enums/structs) that mirror the protocol specification so downstream clients can share the same serialization layer.  
When the `pubky` feature is enabled the crate exports:

- `transport::pubky::PAYKIT_PATH_PREFIX` (`/pub/paykit.app/v0/`) and `PUBKY_FOLLOWS_PATH` (`/pub/pubky.app/follows/`) to standardize path construction.  
- `PubkyAuthenticatedTransport` (wraps `PubkySession`) and `PubkyUnauthenticatedTransport` (wraps `pubky::PublicStorage`) as ready-to-use adapters that satisfy the traits above.
