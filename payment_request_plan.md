# New session - 2026-04-08T10:56:59.941Z

**Session ID:** ses_29342fbdaffedtCQO24FCuSBRn
**Created:** 4/8/2026, 1:56:59 PM
**Updated:** 4/8/2026, 2:50:13 PM

---

## User

I want to implement payment-request feature. Basically it should be similar to private payments but instead of exchanging information about PaymentEndpoints it should write messages with PaymentRequests. Write a plan on what is needed in order to implement this feature

---

## Assistant (Plan · Claude Opus 4.6 · 103.8s)

_Thinking:_

The user wants me to plan a payment-request feature that's similar to private payments but for PaymentRequests instead of PaymentEndpoints. Let me first explore the codebase to understand the current structure, especially the private payments implementation, transport traits, and related types.

Let me start by exploring the codebase structure and understanding the existing private payments implementation.

I'll start by exploring the codebase to understand the existing private payments implementation and related types, so I can build a well-informed plan.

**Tool: task**

**Input:**
```json
{
  "description": "Explore paykit-rs codebase",
  "prompt": "Thoroughly explore the paykit-rs codebase structure. I need to understand:\n\n1. The full directory/file structure (especially paykit-lib/src/)\n2. The transport trait definitions in paykit-lib/src/transport/traits.rs\n3. The private payments implementation - look for `set_private_payments`, `get_private_payments`, and how they work with pubky-noise\n4. The `PaymentEndpoint` type and how it's structured\n5. The `MethodId` and `EndpointData` types\n6. The `lib.rs` file to see all public API functions\n7. Any existing types related to payment requests\n8. The Cargo.toml files to understand dependencies\n\nProvide the full content of key files: transport/traits.rs, lib.rs, any types/models files, and the private payments related code.",
  "subagent_type": "explore"
}
```

**Output:**
```
task_id: ses_29342d428ffe5FNfvTcRuaWNhX (for resuming to continue this task if needed)

<task_result>
I now have a complete picture of the codebase. Here is a comprehensive summary of everything found:

---

## Complete Codebase Structure

```
paykit-rs/
├── Cargo.toml                 # Workspace: members = ["paykit-lib", "paykit-ffi"], resolver = "2"
├── Cargo.lock
├── README.md                  # Protocol spec + library design doc
├── CHANGELOG.md               # Release history (0.1.0 -> 0.1.0-rc1 -> 0.1.0-rc2)
├── AGENTS.md                  # Repository guidelines for AI agents
├── LICENSE
├── Package.swift              # Swift package manifest (for iOS bindings)
├── .cargo/
├── .github/
├── paykit-lib/
│   ├── Cargo.toml
│   ├── README.md
│   └── src/
│       ├── lib.rs                              # Core library (2726 lines) — ALL types + public API
│       └── transport/
│           ├── mod.rs                           # Re-exports traits + pubky adapters
│           ├── traits.rs                        # Transport trait definitions (111 lines)
│           └── pubky/
│               ├── mod.rs                       # Path constants, re-exports (20 lines)
│               ├── authenticated_transport.rs   # PubkyAuthenticatedTransport (70 lines)
│               └── unauthenticated_transport.rs # PubkyUnauthenticatedTransport (218 lines)
├── paykit-ffi/
│   ├── Cargo.toml
│   ├── README.md
│   ├── RELEASE.md
│   ├── build.sh / build_ios.sh / build_android.sh
│   ├── uniffi.toml / uniffi-android.toml
│   ├── bindings/
│   ├── src/
│   │   ├── lib.rs                              # FFI bindings via UniFFI (446 lines)
│   │   └── bin/uniffi-bindgen.rs
│   └── update_package.py
└── paykit-react-native/
```

---

## Key Files: Full Content Summary

### 1. `paykit-lib/Cargo.toml` — Dependencies

```toml
[features]
default = ["pubky"]
pubky = ["dep:pubky", "dep:pubky-noise", "dep:serde_json"]

[dependencies]
anyhow = "1.0.102"
async-trait = "0.1.89"
pubky = { version = "0.7.0", optional = true }
pubky-noise = { optional = true, version = "0.1.0-rc2" }
serde_json = { version = "1", optional = true }
thiserror = "2.0.18"
tracing = "0.1.44"

[dev-dependencies]
pubky-testnet = { version = "0.7.0", features = ["embedded-postgres"] }
tokio = { version = "1.48.0", features = ["macros", "rt-multi-thread"] }
```

The `pubky` feature (enabled by default) gates all encrypted/private payment functionality, the Pubky transport adapters, and serde_json.

---

### 2. Transport Traits (`paykit-lib/src/transport/traits.rs`) — Full File (111 lines)

This file defines two traits that decouple Paykit from any specific SDK:

- **`UnauthenticatedTransportRead`** (read-only, public data):
  - `fetch_supported_payments(&self, payee: &PublicKey) -> Result<SupportedPayments>` — Lists all payment methods for a payee. Best-effort snapshot (not atomic).
  - `fetch_payment_endpoint(&self, payee: &PublicKey, method: &MethodId) -> Result<Option<EndpointData>>` — Fetches a single endpoint.

- **`AuthenticatedTransport`** (authenticated write access):
  - `upsert_payment_endpoint(&self, method: &MethodId, data: &EndpointData) -> Result<()>` — Creates/updates an endpoint.
  - `remove_payment_endpoint(&self, method: &MethodId) -> Result<()>` — Deletes an endpoint.

Key design note: These traits handle **only public payment endpoints**. Private payments bypass the transport traits entirely and use `pubky-noise` directly.

---

### 3. Core Types (all in `paykit-lib/src/lib.rs`)

There are **no separate model/type files**. All types live in `lib.rs`:

#### `MethodId` (line 128)
- Validated path-safe identifier, wraps `String` (private inner field).
- Constructor: `MethodId::new(id: impl Into<String>) -> Result<Self>`
- Validation: ASCII alphanumeric + hyphens + underscores + dots only; max 64 chars; rejects `.`, `..`, `"private"` (reserved), empty, path traversal.
- Accessors: `.as_str()`, `Display`, `AsRef<str>`
- Derives: `Clone, Debug, PartialEq, Eq, Hash`

#### `EndpointData` (line 206)
- UTF-8 string wrapper for payment endpoint payloads (private inner field).
- Constructor: `EndpointData::new(data: impl Into<String>)`
- Accessors: `.as_str()`, `.into_inner()`, `Display`, `AsRef<str>`
- Derives: `Clone, Debug, PartialEq, Eq`

#### `SupportedPayments` (line 238)
- `pub entries: HashMap<MethodId, EndpointData>` — collection of payment entries.
- Derives: `Clone, Debug, Default, PartialEq, Eq`

#### `PaykitError` (line 58)
Four variants:
- `Transport { context: String, source: anyhow::Error }` — network/SDK failures
- `NotFound(String)` — resource does not exist (404/GONE)
- `InvalidData { context: String, source: Option<anyhow::Error> }` — corrupt network data
- `Validation(String)` — caller input fails structural checks

#### `PublicKey` (line 11/17)
- With `pubky` feature: re-exports `pubky::PublicKey`
- Without: placeholder `pub struct PublicKey(pub String)` with `Display`, `FromStr`

---

### 4. `EncryptedLink` and Handshake Types (`#[cfg(feature = "pubky")]`)

#### `EncryptedLink` (line 267)
- Handle to an established encrypted Noise link. Contains:
  - `encryptor: PubkyNoiseEncryptor` — the Noise session in transport mode
  - `recipient: PublicKey` — counterparty
  - `config: Arc<PubkyNoiseConfig>` — for snapshot/restore
  - `max_send_retries: u32` — retry limit for `set_private_payments` (default: 3)
- Methods: `set_max_send_retries()`, `snapshot()`, `serialize()`, `config()`

#### `EncryptedLinkSnapshot` (line 350)
- Serializable 189-byte snapshot of an established link. For session resumption.
- Methods: `serialize()`, `deserialize(bytes)`, `recipient()`

#### `EncryptedLinkHandshake` (line 537)
- In-progress Noise XX handshake handle. Contains:
  - `encryptor`, `remote_pubkey`, `config`
  - `recovery_attempts: u32`, `max_recovery_attempts: u32` (default: 3)
- Methods: `set_max_recovery_attempts()`, `snapshot()`, `serialize()`, `config()`

#### `EncryptedLinkHandshakeSnapshot` (line 439)
- Serializable 189-byte snapshot of an in-progress handshake.

#### `HandshakeProgress` (line 593)
- Enum: `Pending(EncryptedLinkHandshake)` | `Complete(EncryptedLink)`

---

### 5. Public API Functions (all in `lib.rs`)

#### Public payment endpoints (generic over transport traits):
| Function | Signature | Purpose |
|---|---|---|
| `set_payment_endpoint` | `async fn<S: AuthenticatedTransport>(client, MethodId, EndpointData) -> Result<()>` | Store/update a public endpoint |
| `remove_payment_endpoint` | `async fn<S: AuthenticatedTransport>(client, MethodId) -> Result<()>` | Delete a public endpoint |
| `get_payment_list` | `async fn<R: UnauthenticatedTransportRead>(reader, &PublicKey) -> Result<SupportedPayments>` | Fetch all public endpoints for a payee |
| `get_payment_endpoint` | `async fn<R: UnauthenticatedTransportRead>(reader, &PublicKey, &MethodId) -> Result<Option<EndpointData>>` | Fetch a single public endpoint |

#### Private payments (`#[cfg(feature = "pubky")]`, concrete Pubky types):
| Function | Signature | Purpose |
|---|---|---|
| `set_private_payments` | `async fn(link: &mut EncryptedLink, entries: &HashMap<MethodId, EndpointData>) -> Result<()>` | Encrypt+send entire private payments map |
| `get_private_payments` | `async fn(link: &mut EncryptedLink) -> Result<SupportedPayments>` | Receive+decrypt the latest private payments map |

#### Encrypted link lifecycle (`#[cfg(feature = "pubky")]`):
| Function | Signature | Purpose |
|---|---|---|
| `initiate_encrypted_link` | `fn(session, [u8;32], &PublicKey, Pubky) -> Result<EncryptedLinkHandshake>` | Start Noise XX handshake (initiator) |
| `accept_encrypted_link` | `fn(session, [u8;32], &PublicKey, Pubky) -> Result<EncryptedLinkHandshake>` | Start Noise XX handshake (responder) |
| `advance_handshake` | `async fn(EncryptedLinkHandshake) -> Result<HandshakeProgress>` | Polling-safe handshake step; auto-recovery on write failure |
| `close_encrypted_link` | `async fn(EncryptedLink) -> Result<()>` | Clean up the Noise session |
| `restore_encrypted_link` | `async fn(session, [u8;32], &PublicKey, Pubky, EncryptedLinkSnapshot) -> Result<EncryptedLink>` | Resume link from snapshot (cross-restart) |
| `restore_encrypted_link_from_config` | `async fn(Arc<PubkyNoiseConfig>, &PublicKey, EncryptedLinkSnapshot) -> Result<EncryptedLink>` | Resume link from snapshot (in-process) |
| `restore_encrypted_link_handshake` | `async fn(..., EncryptedLinkHandshakeSnapshot) -> Result<EncryptedLinkHandshake>` | Resume handshake from snapshot (cross-restart) |
| `restore_encrypted_link_handshake_from_config` | `async fn(Arc<PubkyNoiseConfig>, &PublicKey, EncryptedLinkHandshakeSnapshot) -> Result<EncryptedLinkHandshake>` | Resume handshake from snapshot (in-process) |

---

### 6. Private Payments Implementation Details

**How `set_private_payments` works** (line 746):
1. Serializes `HashMap<MethodId, EndpointData>` to JSON via `serialize_private_payments_json()` producing `{ "method_id": "endpoint_value", ... }`
2. Validates payload size is within `PUBKY_NOISE_MSG_LEN` (1000 bytes); returns `PaykitError::Validation` if exceeded
3. Calls `link.encryptor.send_message(&plaintext)` with automatic retry up to `max_send_retries` (default 3)
4. pubky-noise handles encryption, file naming, and storage on the homeserver

**How `get_private_payments` works** (line 871):
1. Loops calling `link.encryptor.receive_message()` until the queue is empty
2. Keeps only the **latest** message (drains intermediates)
3. Trims trailing zero-padding from fixed-size buffers
4. Parses the UTF-8 plaintext as JSON via `parse_private_payments_json()`
5. Returns `SupportedPayments` (empty if no messages were available)

**Path derivation** (line 634):
- Uses `pubky_noise::path_derivation::derive_asymmetric_paths` with domain `b"paykit-path-v0"` and base path `"/pub/paykit/v0/private"`.
- Produces asymmetric `(write_path, read_path)` per peer-pair from a DH shared secret.
- Alice's `write_path == Bob's read_path` and vice versa.

---

### 7. Path Constants (`transport/pubky/mod.rs`)

```rust
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0/";
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";
```

Public endpoints are stored at: `{pubkey}/pub/paykit/v0/{method_id}`
Private encrypted paths are derived under: `/pub/paykit/v0/private/...`

---

### 8. Payment Request Types

**There are no existing payment request types in the codebase.** A grep for `payment.?request`, `PaymentRequest`, `pay_request`, and `PayRequest` across all `.rs` files returned zero results. The README describes payment requests conceptually (with a JSON example at line 374), but no Rust types or functions implement them yet. The README's `Appendix` shows an example payment request JSON structure:
```json
{
  "supported payment list": "...",
  "frequency": "1d",
  "startsAt": "1736415571",
  "endsAt": "1736445571",
  "amount": 0.001,
  "currency": "BTC"
}
```
But this is listed as a future "Paykit Daemon" feature, not part of the current `paykit-lib` implementation.

---

### 9. Pubky Transport Adapters

**`PubkyAuthenticatedTransport`** (wraps `PubkySession`):
- `upsert_payment_endpoint`: PUTs to `{PAYKIT_PATH_PREFIX}{method_id}`
- `remove_payment_endpoint`: DELETEs from `{PAYKIT_PATH_PREFIX}{method_id}`

**`PubkyUnauthenticatedTransport`** (wraps `pubky::PublicStorage`):
- `fetch_supported_payments`: Lists `{payee}{PAYKIT_PATH_PREFIX}` directory, fetches each non-directory entry, validates MethodId from path, returns `SupportedPayments`
- `fetch_payment_endpoint`: GETs `{payee}{PAYKIT_PATH_PREFIX}{method_id}`
- Treats 404/GONE as `None`/empty (not errors)
- Handles UTF-8 validation (returns `InvalidData` on bad encoding)
</task_result>
```

---

## Assistant (Plan · Claude Opus 4.6 · 5.9s)

_Thinking:_

Excellent! Now I have a comprehensive understanding of the codebase. Let me also check the README for more details about the payment request concept, and look at the pubky-noise API to understand what capabilities are available for message exchange.

Great, I have a thorough understanding of the codebase. Let me also check the README for the payment request concept and understand the pubky-noise message API better.

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/u/Projects/Synonym/Paykit/paykit-rs/README.md"
}
```

**Output:**
```
<path>/home/u/Projects/Synonym/Paykit/paykit-rs/README.md</path>
<type>file</type>
<content>
1: ⚠️ WIP - NOT FOR PRODUCTION ⚠️
2: 
3: # Paykit
4: 
5: # Description
6: 
7: Paykit is a method for abstracting and automating any payment settlement process behind a single static public key which refers to a location of a file containing all supported payment methods and related data and endpoints.
8: 
9: As a meta payment protocol, Paykit also serves as an ideal layer for handling metadata related to payments, proofs of payment, and related features like automated subscriptions.
10: 
11: # Paykit protocol
12: 
13: Peers and applications that support Paykit may share or retrieve necessary payment information by accessing a compatible **routing network**. This network facilitates the storage and retrieval of data associated with public keys. The intended solution is to utilize Pubky’s PKARR method with Mainline DHT for discovery and routing, and data storage in Pubky homeservers.
14: 
15: ## Routing network
16: 
17: Paykit requires a network to lookup **Supported Payments List** in order to retrieve and share data. Therefore the minimum requirements to the network are:
18: 
19: * Ability to look up a node on a network based on its public key  
20: * Data stored on the node under certain path is guaranteed to be available by URL  
21: * Authenticity of location where data is stored is verifiable with owner’s public key  
22: * Read access to locations can be public or restricted and granted with URL  
23: * Write access to locations can be granted to non-owners and granted with URL  
24: * Access levels can be changed without changing the path component of the URL.   
25: * Optionally support sending private direct messages to users of the network?
26: 
27: ### Pubky Core protocol
28: 
29: DHT’s are optimal routing mechanisms for key based methods like Paykit. Paykit currently utilizes the Mainline DHT via use of [**Pubky Core**](https://github.com/pubky/pubky-core) protocol but theoretically any network could be used that satisfies the requirements above.
30: 
31: ## Supported Payments List, Payment Method and Payment Endpoint
32: 
33: The examples of following concepts are provided in the Appendix at the end of this document. For the reasons explained below they are for illustrative purposes only.
34: 
35: ### Supported Payments List
36: 
37: Read request to **Paykit Routing Network** with public key returns a **Supported Payments List** stored at **default public path**. This is an array of objects with one key being “**method**” whose value is URL to **Payment Method** and another key is **“endpoint”** with value being **Payment Endpoint** URL.
38: 
39: ### Payment Method
40: 
41: The term "**Payment Method**" refers to the general concept of the medium through which a payment can be executed. 
42: 
43: ### Payment Endpoint
44: 
45: **Payment Endpoint** corresponds to the specific payee owned credentials/reference on which they can receive corresponding payments. 
46: 
47: ### Paykit Method Implementation Proposals
48: 
49: To prevent miscommunication between payer and payee which will result in inability to execute payments. The terms used on both ends of the payment are to be decided by the developers community.   
50: Given that both the payer and payee may have preferences regarding payment media based on a virtually infinite number of factors—such as market conditions, address types, and interbank settlement times—it is up to social consensus to determine the naming conventions for payment methods and the structure of payment endpoints. Therefore, the examples provided in this document are for illustrative purposes only.
51: 
52: ## Payment Method Lists
53: 
54: Paykit can support virtually any payment method as long as payer and payee can mutually describe and identify it. Paykit users create the **Supported Payments Lists** \- minimum necessary data related to their supported payment methods and publish them as records on **Paykit routing** network.
55: 
56: ### Public Payment Method Lists
57: 
58: Paykit allows you to receive payments from anyone who is aware of the payee's public key.
59: 
60: #### Flow
61: 
62: 1. The payee creates **Supported Payments List, Methods and Endpoints**  
63: 2. The payee stores created data under public location on Paykit Routing Network associated with their key pair   
64: 3. The payee publicly shares public key
65: 
66: #### NOTE:
67: 
68: It is important to understand that this data could be logged and monitored by all peers that know of this pubkey, and thus some methods, like bitcoin addresses, could expose correlations and payment information of peers in a suboptimal or undesirable way. 
69: 
70: ### Private Payment Method Lists
71: 
72: Paykit can create personalized, dedicated Payment Method Lists for known public keys. These lists are:
73: 
74: * Optionally encrypted  
75: * Only readable via the corresponding URL, and contain personalized data  
76: * Designed to maximize privacy of both payers and payees by preventing public access to payment data
77: 
78: #### Flow
79: 
80: 1. The payee creates **Supported Payments List, Methods and Endpoints**  
81: 2. The payee stores all created data under private locations on Paykit Network associated with their key pair  
82: 3. The payee grants access to created data to whoever possesses access URL  
83: 4. The payee shares access URL to **Supported Payments List** with the payer in a secure manner
84: 
85: ## Payment Method Selection
86: 
87: Paykit attempts to match the supported payment methods of two peers by comparing payers supported payments against payees **Payment Method Lists** to find a match. If multiple matches are detected, paykit uses the payer’s **Payment Selection Logic** settings to prioritize the order of execution. If a user has not customized their **Payment Selection Logic**, paykit will use the **Default Payment Selection Logic**
88: 
89: ### Default Payment Selection Logic
90: 
91: The “known peer” relationship means that there was previous out of band communication between payer and payee, during which public keys were exchanged. Consequently payee created a Private Payment Method List  for payer and payee has a URL which grants access to it. Access URL can be shared via private message implemented by the underlying routing network. It is a not known peer otherwise and payment should be assumed as a public list with payees threat model adjusted accordingly.
92: 
93: #### Payee is a known peer
94: 
95: 1. The payer retrieves payee’s **Private Payment Method List** using the access url  
96: 2. The payer filters out supported payment methods  
97: 3. The payer selects the first payment method according to payers own personal preferences  
98: 4. The payer retrieves data from the corresponding payment endpoint and decrypts it  
99: 5. The payer attempts to execute a payment  
100: 6. In case of failure \- repeats from step 3 until the list from step 2 is empty.  
101: 7. In case if all payments failed send a direct message to payee using routing network in order to inform about the problem
102: 
103: #### Payee is not a known peer
104: 
105: 1. The payer resolves payee’s **Public Payment Method List** using their public key  
106: 2. The filters out supported payment methods  
107: 3. The payer selects the first payment method according to payers own personal preference  
108: 4. The payer retrieves data from the corresponding payment endpoint   
109: 5. The payer attempts to execute a payment  
110: 6. In case of failure \- repeats from step 3 until the list from step 2 is empty.
111: 
112: ## Payment Method Interactivity
113: 
114: Both Private & Public **Payment Method Lists** can contain virtually any payment data, regardless of the interactivity requirements to either payer or payee on any level. In other words, paykit peers implement hooks for uni- and bi- directional communication.
115: 
116: ### Interactive Payments
117: 
118: For example, peers may include URLs that direct the payer to an appropriate server or API in order to interact using other specific payment protocols that are mutually supported. 
119: 
120: ### Non-interactive Payments
121: 
122: For example, any static blockchain address, lightning network invoice, address or offer, email address, cashtag etc.
123: 
124: # Paykit library
125: 
126: A stateless toolkit featuring developer-friendly APIs and language bindings to engage with Paykit’s Payment Method Lists. This kit is intended to serve as a new dependency in the existing logic of applications and services responsible for processing of payments.
127: 
128: For release history and upgrade notes see [CHANGELOG.md](CHANGELOG.md).
129: 
130: ## Usecases
131: 
132: The Paykit Library is intended for users who have already implemented payment receiving and execution functions including both push and pull subscription functionality implemented with these methods.
133: 
134: ### Intended users
135: 
136: #### Light user wallets
137: 
138: These wallets already incorporate specific payment functionalities and aim to integrate Paykit features, such as enabling payment to contacts based on their public key.
139: 
140: #### Payment processors
141: 
142: These entities have already implemented various payment methods and seek to enhance the Paykit user experience by offering a single payment endpoint. This endpoint allows Paykit payees to execute payments without the need for manual selection of payment methods.
143: 
144: ## Technical Requirements
145: 
146: The Paykit Library is an abstraction for payment specific CRUD methods of Paykit Protocol. It is to be used as a transport layer dependency for payment processing business logic. This will allow it to be used in implementations of payment logic of any complexity \- from single to recurring payments in implementations of any architecture from micro services to monolith.
147: 
148: ### Implementation language
149: 
150: * **Rust**: Edition 2021, Version \+1.91.1
151: 
152: ### Dependencies
153: 
154: * **The Pubky SDK** [https://github.com/pubky/pubky-core/tree/main/pubky](https://github.com/pubky/pubky-core/tree/main/pubky)
155: * **async-trait** — async fn in trait definitions
156: * **tracing** — structured, context-aware logging
157: 
158: ### Language bindings
159: 
160: Mobile bindings are generated via [UniFFI](https://mozilla.github.io/uniffi-rs/) in the [`paykit-ffi/`](paykit-ffi/) crate. See its [README](paykit-ffi/README.md) for build and integration instructions.
161: 
162: * **Swift:** Version \+5  
163: * **Kotlin:** Version \+2.0
164: 
165: ### Test coverage
166: 
167: * Documentation tests for all public methods
168: 
169: ### Documentation
170: 
171: * Rust doc documentation for all public methods
172: 
173: #### Examples
174: 
175: * Send and receive test payment with mocked payment logic for two payment methods available for the receiving key.
176: 
177: ## Functional Requirements
178: 
179: **Notes**: *The implementation of ID and its usage is subject to your specific application design and requirements*
180: 
181: ### Public Payment Data
182: 
183: The APIs facilitate seamless interaction with public payment data using Paykit’s Routing Network which ensures efficient communication between payees and payers. 
184: 
185: #### Retrieve public Supported Payments List for a given payee's public key
186: 
187: Allow users to fetch the list of payment methods that are publicly available for a specific payee, identified by their public key.
188: 
189: #### Retrieve Payment Endpoint for a payee's public key and payment method
190: 
191: Enable users to access detailed payment information associated with a particular payment method for a given payee's public key.
192: 
193: #### Store Payment Endpoint for a specific Payment Method and make it publicly accessible 
194: 
195: Allow users to store payment data for a specific payment method, making it publicly accessible.
196: 
197: ### Private Payment Data
198: 
199: These APIs facilitate secure interaction with private payment data, ensuring that sensitive information is encrypted and accessible only to authorized parties.
200: 
201: #### Retrieve all privately Supported Payment List for a given URL
202: 
203: Enables users to fetch the optionally encrypted list of payment methods that are privately supported for a specific payer, identified by their public key.
204: 
205: #### Retrieve Payment Endpoint for a given URL and payment method
206: 
207: Allows users to access and decrypt detailed payment information associated with a particular payment method for a given URL
208: 
209: #### Store encrypted payment data at the private location
210: 
211: Allows users to securely store encrypted payment data for a specific payee. Payee can optionally provide a shared ID for easier data management and reference.
212: 
213: #### Send arbitrary data to specific URL
214: 
215: This API allows users:
216: 
217: - Send a link with the private payment lists as a payment request  
218: - To send a text with meta data corresponding it to payment with shared ID as a memo
219: 
220: ##### Note: 
221: 
222: It is up to the owner of the URL to guarantee that only intended users can write to this url. This can be done using mechanisms of the Paykit Routing Network.
223: 
224: ## Deliverable Example
225: 
226: The first integration will be into bitkit. Thus [https://github.com/synonymdev/bitkit-core/](https://github.com/synonymdev/bitkit-core/) will be a wrapper around deliverables while also provided as a deliverables example.
227: 
228: # Paykit Daemon
229: 
230: The **Paykit Daemon** is a stateful component that keeps track of sent and received payments, provides a unified API for various payment operations, and includes advanced logic for payment prioritization and subscription management.
231: 
232: ## Functional Requirements
233: 
234: The **Paykit Daemon** offers the following features:
235: 
236: ### Payments
237: 
238: #### To public key
239: 
240: Allows sending payments to a public key with automatic fallback to alternative payment methods based on the default payment selection logic assuming the location of Supported Payment List under conventional path.
241: 
242: #### To URL
243: 
244: Allows sending payments to a URL with automatic fallback to alternative payment methods based on the default payment selection logic. If write permission are granted for a location associated with the provided URL then it should be possible to add optional memo to the executed payment.
245: 
246: ### Request
247: 
248: For a known peer who has provided write access to the secure location under their URL, it should be possible to send payment requests. See Appendix for payment request example.
249: 
250: ### Receive
251: 
252: Both public and private receiving have an option of automatically recycling payment receiving data upon use, expiration or change in conditions.
253: 
254: #### Receive on public key
255: 
256: Enables publicly receiving payments using multiple selected payment endpoints via one public key. From anyone, with or without specifying amount and / or ID shared with a payer. Using conventional path for Supported Payments List. 
257: 
258: #### Receive on URL
259: 
260: Enables receiving payments for optionally specified amount using given multiple selected payment endpoints via one URL using private path and encryption key for the stored content returned as a part of the URL pointing to the Supported Payment List.
261: 
262: ### Accounting
263: 
264: Provides an API to retrieve payments based on various filters, such as date range, payment status, payment method, ID, receiver etc
265: 
266: ### Events
267: 
268: Provides an API to receive notifications about change of both incoming and outgoing payments as well as new write events in owned locations shared with other network participants.   
269: This feature allows for additional custom data to be provided for transitions after intermediate steps, such as handling payments from multisig accounts or providing second-factor authentication with OTPs for payments.
270: 
271: ### Subscriptions
272: 
273: Subscription management in **Paykit Daemon** is designed to be flexible and efficient. Ideally, subscription-related logic should be resolved at the payment protocol level (SEPA standing order / direct debit, BOLT12 subscriptions, etc).  
274: However, if not possible, the Paykit Daemon can handle subscription management by implementing the following subscription functionality while relying on the **Paykit Library** for individual payments in the subscription process.
275: 
276: #### Push subscriptions (Payments)
277: 
278: Allows the payer to create a push subscription to a payee's public key with custom subscription parameters which will satisfy for execution and termination conditions allowing the daemon periodically executes payments based on these conditions.
279: 
280: #### Pull subscriptions (Payment Requests)
281: 
282: Allows the payee to give the payer a subscription URL with secret component and subscription parameters which satisfy for execution and termination conditions. So that the daemon based periodically sends a unique secret derived from the shared key/secret to trigger a payment on the client side to daemon’s public key upon successful validation for the received secret. 
283: 
284: ## Additional functionality for customization
285: 
286: The Paykit Daemon allows for the following customizable logic for advanced payment management.
287: 
288: ### Customization of payment prioritization logic for payee
289: 
290: Allows configuring payment prioritization based on various factors, such as spending fiat currency when the price of Bitcoin is low.
291: 
292: ### Customization of payment receiving prioritization
293: 
294: Allows configuring payment receiving prioritization, such as prioritizing payment channels based on available funds or reliability.
295: 
296: ## Future development directions
297: 
298: * In the future, Paykit will be able to specify all of the payment types within the Bitcoin world, including all of the competing methods like Offers & LNURL, etc.  
299: * This payment negotiation process is so abstracted that it could allow for new ways to coordinate Bitcoin transactions, including multisigs, DLCs, and mixes.  
300: * It could even support non-Bitcoin payments like credit cards and other payment processors. You only need to locally and mutually support the method across payer and payee.
301: 
302: ## Technical Requirements
303: 
304: The daemon is expected to be run as a standalone background process with CLI for administration or to be added as a dependency into a web server. The design and implementation should account for extensibility of adding new payment methods and infrastructure, thus a plugin system is suggested with the instance of the Paykit Daemon being passed as parameter using dependency injection pattern.
305: 
306: ### Implementation language
307: 
308: * **Rust:** Edition 2021\. Version \+1.82.0
309: 
310: ### Dependencies
311: 
312: * **The Paykit library**
313: 
314: ### Database connectivity
315: 
316: * **SQLite:** Version3 (using pluggable connector)
317: 
318: ### Payment Infrastructure Connectivity
319: 
320: * **Lightning Network Daemon** \+v0.18.3  
321: * **LNDK** v0.2.0 
322: 
323: ### Payment Methods Requirements
324: 
325: * Onchain payments for all supported address types  
326: * BOLT11 Invoices. 0 amount BOLT11 Invoices  
327: * BOLT12 Offers
328: 
329: # Resources:
330: 
331: * First draft implementation of paykit library [https://github.com/pubky/paykit-pdk](https://github.com/pubky/paykit-pdk)  
332: * First draft implementation of paykit daemon [https://github.com/pubky/paykit](https://github.com/pubky/paykit)  
333: * Original Slashpay POC Presentation: [https://docs.google.com/presentation/d/1TqbQUbWANzMdze5\_OSdqy7RjajOwHUXRCEC73LQsjyY/edit\#slide=id.g100ef2f468b\_0\_139](https://docs.google.com/presentation/d/1TqbQUbWANzMdze5_OSdqy7RjajOwHUXRCEC73LQsjyY/edit#slide=id.g100ef2f468b_0_139)  
334: * Initial roadmap [https://docs.google.com/document/d/16mpEuyX3yRYLsQRD92T6J1VdUdp2nPhW7Jm6fHgYpHY/edit?pli=1\&tab=t.0](https://docs.google.com/document/d/16mpEuyX3yRYLsQRD92T6J1VdUdp2nPhW7Jm6fHgYpHY/edit?pli=1&tab=t.0)  
335: * Product overview [https://docs.google.com/document/d/1Z1HHdxpkOtelOXJRgPldso4\_-lchzs3NL\_JqDxCdiu8/edit?pli=1\&tab=t.0](https://docs.google.com/document/d/1Z1HHdxpkOtelOXJRgPldso4_-lchzs3NL_JqDxCdiu8/edit?pli=1&tab=t.0)
336: 
337: # Appendix
338: 
339: ## Examples
340: 
341: ### Possible Examples (Require PMIP)
342: 
343: Examples of **Payment Methods** can be “bitcoin” \- referring to bitcoin onchain payments, “lighting” \- referring to bitcoin lightning network payment, “SEPA” referring to SEPA network bank transfer. Correspondingly **Payment Endpoints** will be bitcoin onchain address, bolt11 invoice or bolt12 offer, IBAN with optional BIC code. 
344: 
345: #### Supported Payments List
346: 
347: ```
348: [  
349:   {  
350:     “method”: "paykit.standards.com/p2pkh"
351:     “endpoint": “payee-paykit-server.com/bitcoin/p2pkh"
352:   },
353:   {  
354:     “method”: "paykit.standards.com/lightning",
355:     “endpoint”: "payee-paykit-server.com/bitcoin/bolt11",
356:   },
357:   {
358:     “method”: "paykit.standards.com/sepa",
359:     “endpoint”: "payee-paykit-server.com/fiat/euro"
360:   }  
361: ]
362: ```
363: 
364: #### Payment Method Specification (p2pkh)
365: 
366: `Payment Endpoint should return UTF-8 encoded string containing p2pkh address`
367: 
368: #### Payment Endpoint (p2pkh)
369: 
370: `n2HyESbFJAz6PAFuRL5wEqv21yrKt9UTCP`
371: 
372: #### Payment Request
373: 
374: ```
375: {
376:   "supported payment list": "payee-paykit-server.com/private/random-id-path/payment.json",
377:   "freequency": "1d",
378:   "startsAt": "1736415571",
379:   "endsAt": "1736445571",
380:   "amount": 0.001,
381:   "currency": "BTC"
382: }
383: ```

(End of file - total 383 lines)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs",
  "offset": 740,
  "limit": 200
}
```

**Output:**
```
<path>/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs</path>
<type>file</type>
<content>
740: /// - Returns [`PaykitError::Validation`] if the serialized payload exceeds
741: ///   the maximum message size.
742: /// - Returns [`PaykitError::InvalidData`] if the map cannot be serialized.
743: /// - Returns [`PaykitError::Transport`] if `send_message` fails after all
744: ///   retry attempts are exhausted.
745: #[instrument(skip(link, entries), fields(count = entries.len()))]
746: pub async fn set_private_payments(
747:     link: &mut EncryptedLink,
748:     entries: &HashMap<MethodId, EndpointData>,
749: ) -> Result<()> {
750:     debug!("sending private payments map");
751: 
752:     let json = serialize_private_payments_json(entries)
753:         .map_err(|err| map_error("set_private_payments", err))?;
754: 
755:     let plaintext = json.into_bytes();
756: 
757:     if plaintext.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
758:         return Err(PaykitError::Validation(format!(
759:             "private payments payload ({} bytes) exceeds max message size ({} bytes)",
760:             plaintext.len(),
761:             pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
762:         )));
763:     }
764: 
765:     let max_attempts = send_attempts_from_retries(link.max_send_retries); // first try + retries
766:     let mut last_error: Option<String> = None;
767: 
768:     for attempt in 1..=max_attempts {
769:         match link.encryptor.send_message(&plaintext).await {
770:             Ok(()) => {
771:                 debug!("private payments map sent successfully");
772:                 return Ok(());
773:             }
774:             Err(err) => {
775:                 last_error = Some(format!("{err:?}"));
776:                 if attempt < max_attempts {
777:                     warn!(
778:                         attempt,
779:                         max_retries = link.max_send_retries,
780:                         error = ?err,
781:                         "send_message failed, retrying"
782:                     );
783:                 }
784:             }
785:         }
786:     }
787: 
788:     Err(PaykitError::Transport {
789:         context: format!(
790:             "failed to send private payments after {} attempts",
791:             max_attempts,
792:         ),
793:         source: anyhow::anyhow!(
794:             "pubky-noise send_message failed on all {} attempts; last error: {}",
795:             max_attempts,
796:             last_error.unwrap_or_else(|| "unknown error".to_string())
797:         ),
798:     })
799: }
800: 
801: /// Removes a payment endpoint via the injected authenticated client.
802: #[instrument(skip(client), fields(method = %method))]
803: pub async fn remove_payment_endpoint<S>(client: &S, method: MethodId) -> Result<()>
804: where
805:     S: AuthenticatedTransport,
806: {
807:     debug!("removing payment endpoint");
808:     client
809:         .remove_payment_endpoint(&method)
810:         .await
811:         .map_err(|err| map_error("remove_payment_endpoint", err))
812: }
813: 
814: /// Retrieves all supported payment methods for the given payee.
815: ///
816: /// # Semantics
817: /// - Returns an empty map when the payee has not published any endpoints or their
818: ///   storage directory is missing.
819: /// - Returns `Err(PaykitError::InvalidData)` when a resource path is unparseable or
820: ///   an endpoint payload contains invalid UTF-8.
821: /// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
822: ///
823: /// # Examples
824: /// ```
825: /// # use paykit_lib::{get_payment_list, MethodId, EndpointData, SupportedPayments};
826: /// # use paykit_lib::{AuthenticatedTransport, UnauthenticatedTransportRead};
827: /// # async fn demo(reader: &impl UnauthenticatedTransportRead, pk: &paykit_lib::PublicKey) -> paykit_lib::Result<()> {
828: /// let payments = get_payment_list(reader, pk).await?;
829: /// if payments.entries.is_empty() {
830: ///     println!("payee published no endpoints yet");
831: /// } else {
832: ///     for (method, data) in &payments.entries {
833: ///         println!("method={} payload={}", method.as_str(), data.as_str());
834: ///     }
835: /// }
836: /// # Ok(())
837: /// # }
838: /// ```
839: #[instrument(skip(reader))]
840: pub async fn get_payment_list<R>(reader: &R, payee: &PublicKey) -> Result<SupportedPayments>
841: where
842:     R: UnauthenticatedTransportRead,
843: {
844:     debug!("fetching payment list");
845:     let result = reader
846:         .fetch_supported_payments(payee)
847:         .await
848:         .map_err(|err| map_error("get_payment_list", err))?;
849:     debug!(count = result.entries.len(), "payment list retrieved");
850:     Ok(result)
851: }
852: 
853: #[cfg(feature = "pubky")]
854: /// Receives and decrypts the private payments map from the remote peer
855: /// via the established encrypted link.
856: ///
857: /// Returns the full map of payment methods. The caller can look up
858: /// individual methods from the returned [`SupportedPayments`].
859: ///
860: /// # Parameters
861: /// - `link` — an established [`EncryptedLink`] for decryption and I/O.
862: ///
863: /// # Semantics
864: /// - Returns an empty [`SupportedPayments`] when no messages are available.
865: /// - Drains all currently unread queued updates and returns the latest map.
866: ///   Intermediate queued updates are consumed.
867: /// - Returns `Err(PaykitError::InvalidData)` when the decrypted payload
868: ///   is not valid UTF-8 or cannot be parsed as a payments JSON map.
869: /// - Returns `Err(PaykitError::Transport)` for decryption or I/O failures.
870: #[instrument(skip(link))]
871: pub async fn get_private_payments(link: &mut EncryptedLink) -> Result<SupportedPayments> {
872:     debug!("receiving private payments map");
873: 
874:     let mut latest: Option<[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN]> = None;
875:     let mut drained = 0usize;
876: 
877:     loop {
878:         let messages =
879:             link.encryptor
880:                 .receive_message()
881:                 .await
882:                 .map_err(|err| PaykitError::Transport {
883:                     context: format!("failed to receive private payments: {err:?}"),
884:                     source: anyhow::anyhow!("pubky-noise receive_message failed: {err:?}"),
885:                 })?;
886:         if messages.is_empty() {
887:             break;
888:         }
889: 
890:         drained += messages.len();
891:         latest = messages.into_iter().last();
892:     }
893: 
894:     let Some(raw) = latest else {
895:         debug!("no private payments messages available, returning empty map");
896:         return Ok(SupportedPayments::default());
897:     };
898: 
899:     // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
900:     let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
901:     let plaintext = std::str::from_utf8(&raw[..end]).map_err(|err| PaykitError::InvalidData {
902:         context: format!("private payments plaintext is not valid UTF-8: {err}"),
903:         source: Some(err.into()),
904:     })?;
905: 
906:     let entries = parse_private_payments_json(plaintext)?;
907:     debug!(
908:         count = entries.len(),
909:         drained, "private payments map received"
910:     );
911:     Ok(SupportedPayments { entries })
912: }
913: 
914: /// Retrieves a specific payment endpoint for `payee` and `method`.
915: ///
916: /// # Semantics
917: /// - Returns `Ok(None)` when the endpoint file is missing or empty.
918: /// - Returns `Err(PaykitError::InvalidData)` when the endpoint payload contains invalid UTF-8.
919: /// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
920: ///
921: /// # Examples
922: /// ```
923: /// # use paykit_lib::{get_payment_endpoint, MethodId, PublicKey};
924: /// # use paykit_lib::UnauthenticatedTransportRead;
925: /// # async fn inspect(reader: &impl UnauthenticatedTransportRead, pk: &PublicKey) -> paykit_lib::Result<()> {
926: /// let lightning = MethodId::new("lightning")?;
927: /// if let Some(endpoint) = get_payment_endpoint(reader, pk, &lightning).await? {
928: ///     println!("lightning endpoint: {}", endpoint.as_str());
929: /// } else {
930: ///     println!("no lightning endpoint published");
931: /// }
932: /// # Ok(())
933: /// # }
934: /// ```
935: #[instrument(skip(reader), fields(method = %method))]
936: pub async fn get_payment_endpoint<R>(
937:     reader: &R,
938:     payee: &PublicKey,
939:     method: &MethodId,

(Showing lines 740-939 of 2726. Use offset=940 to continue.)
</content>
```

---

## Assistant (Plan · Claude Opus 4.6 · 5.1s)

_Thinking:_

Now let me also look at the serialization helpers and the FFI layer to understand what needs to be exposed.

Let me also look at the serialization helpers and the FFI bindings to understand the full picture.

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs",
  "offset": 940,
  "limit": 300
}
```

**Output:**
```
<path>/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs</path>
<type>file</type>
<content>
940: ) -> Result<Option<EndpointData>>
941: where
942:     R: UnauthenticatedTransportRead,
943: {
944:     debug!("fetching payment endpoint");
945:     let result = reader
946:         .fetch_payment_endpoint(payee, method)
947:         .await
948:         .map_err(|err| map_error("get_payment_endpoint", err))?;
949:     debug!(found = result.is_some(), "payment endpoint lookup complete");
950:     Ok(result)
951: }
952: 
953: #[cfg(feature = "pubky")]
954: /// Initiates a Noise XX handshake with a remote peer (initiator role).
955: ///
956: /// Initializes the encryption stack and creates a handshake context. The actual
957: /// handshake messages are exchanged by repeatedly calling [`advance_handshake`]
958: /// until it returns [`HandshakeProgress::Complete`].
959: ///
960: /// Ephemeral keys are managed internally by the Noise stack — callers only need
961: /// to provide their static identity key and the remote peer's public key.
962: ///
963: /// # Parameters
964: /// - `session` — authenticated Pubky session for writing handshake messages
965: ///   (consumed; caller should `.clone()` if needed elsewhere).
966: /// - `sender_secret_key` — 32-byte Ed25519 secret key of the local peer.
967: /// - `receiver_pubkey` — public key of the remote peer.
968: /// - `outbox_client` — HTTP client for reading from the remote homeserver
969: ///   (consumed; caller should `.clone()` if needed elsewhere).
970: ///
971: /// # Errors
972: /// Returns [`PaykitError::Transport`] if the encryption stack cannot be
973: /// initialized or if the context creation fails.
974: #[instrument(skip(session, sender_secret_key, outbox_client))]
975: pub fn initiate_encrypted_link(
976:     session: pubky::PubkySession,
977:     sender_secret_key: [u8; 32],
978:     receiver_pubkey: &PublicKey,
979:     outbox_client: pubky::Pubky,
980: ) -> Result<EncryptedLinkHandshake> {
981:     debug!("initializing encrypted link handshake (initiator)");
982: 
983:     let (write_path, read_path) =
984:         compute_private_payment_paths(&sender_secret_key, receiver_pubkey);
985: 
986:     let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
987:         sender_secret_key,
988:         0,
989:         "XX",
990:         session,
991:         write_path,
992:         read_path,
993:         outbox_client,
994:     )
995:     .map_err(|err| PaykitError::Transport {
996:         context: format!("failed to create encryptor config: {err:?}"),
997:         source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
998:     })?;
999: 
1000:     let encryptor = pubky_noise::PubkyNoiseEncryptor::new(
1001:         config.clone(),
1002:         sender_secret_key,
1003:         true,
1004:         receiver_pubkey.clone(),
1005:     )
1006:     .map_err(|err| PaykitError::Transport {
1007:         context: format!("failed to initialize encryptor: {err:?}"),
1008:         source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
1009:     })?;
1010: 
1011:     debug!("handshake context initialized (initiator)");
1012:     Ok(EncryptedLinkHandshake {
1013:         encryptor,
1014:         remote_pubkey: receiver_pubkey.clone(),
1015:         config,
1016:         recovery_attempts: 0,
1017:         max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
1018:     })
1019: }
1020: 
1021: #[cfg(feature = "pubky")]
1022: /// Accepts a Noise XX handshake from a remote peer (responder role).
1023: ///
1024: /// Initializes the encryption stack and creates a handshake context for the
1025: /// responder side. The actual handshake messages are exchanged by repeatedly
1026: /// calling [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
1027: ///
1028: /// # Parameters
1029: /// - `session` — authenticated Pubky session for writing handshake messages
1030: ///   (consumed; caller should `.clone()` if needed elsewhere).
1031: /// - `receiver_secret_key` — 32-byte Ed25519 secret key of the local peer.
1032: /// - `sender_pubkey` — public key of the remote peer (the initiator).
1033: /// - `outbox_client` — HTTP client for reading from the remote homeserver
1034: ///   (consumed; caller should `.clone()` if needed elsewhere).
1035: ///
1036: /// # Errors
1037: /// Returns [`PaykitError::Transport`] if the encryption stack cannot be
1038: /// initialized or if the context creation fails.
1039: #[instrument(skip(session, receiver_secret_key, outbox_client))]
1040: pub fn accept_encrypted_link(
1041:     session: pubky::PubkySession,
1042:     receiver_secret_key: [u8; 32],
1043:     sender_pubkey: &PublicKey,
1044:     outbox_client: pubky::Pubky,
1045: ) -> Result<EncryptedLinkHandshake> {
1046:     debug!("initializing encrypted link handshake (responder)");
1047: 
1048:     let (write_path, read_path) =
1049:         compute_private_payment_paths(&receiver_secret_key, sender_pubkey);
1050: 
1051:     let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
1052:         receiver_secret_key,
1053:         0,
1054:         "XX",
1055:         session,
1056:         write_path,
1057:         read_path,
1058:         outbox_client,
1059:     )
1060:     .map_err(|err| PaykitError::Transport {
1061:         context: format!("failed to create encryptor config: {err:?}"),
1062:         source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
1063:     })?;
1064: 
1065:     let encryptor = pubky_noise::PubkyNoiseEncryptor::new(
1066:         config.clone(),
1067:         receiver_secret_key,
1068:         false,
1069:         sender_pubkey.clone(),
1070:     )
1071:     .map_err(|err| PaykitError::Transport {
1072:         context: format!("failed to initialize encryptor: {err:?}"),
1073:         source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
1074:     })?;
1075: 
1076:     debug!("handshake context initialized (responder)");
1077:     Ok(EncryptedLinkHandshake {
1078:         encryptor,
1079:         remote_pubkey: sender_pubkey.clone(),
1080:         config,
1081:         recovery_attempts: 0,
1082:         max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
1083:     })
1084: }
1085: 
1086: #[cfg(feature = "pubky")]
1087: /// Advances the handshake by one step.
1088: ///
1089: /// This function is **polling-safe**: calling it when the remote peer has not
1090: /// written their next message yet returns [`HandshakeProgress::Pending`] without
1091: /// corrupting internal state. The caller can safely retry after a delay.
1092: ///
1093: /// # Automatic recovery
1094: ///
1095: /// If the homeserver write fails during a handshake step
1096: /// (`HomeserverWriteError`), the internal Noise state is irreversibly
1097: /// corrupted. This function automatically recovers by restoring from the
1098: /// pre-mutation snapshot captured at the start of the failed step and returns
1099: /// [`HandshakeProgress::Pending`] so the caller's polling loop retries
1100: /// transparently.
1101: ///
1102: /// The maximum number of **consecutive** recovery attempts is configurable via
1103: /// [`EncryptedLinkHandshake::set_max_recovery_attempts`] (default:
1104: /// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`]). The counter resets to zero after every
1105: /// successful step. If the limit is exceeded, the function returns
1106: /// [`PaykitError::Transport`].
1107: ///
1108: /// # Polling strategy
1109: ///
1110: /// The caller controls the polling strategy. Common patterns:
1111: ///
1112: /// **Fixed interval:**
1113: /// ```ignore
1114: /// loop {
1115: ///     match advance_handshake(handshake).await? {
1116: ///         HandshakeProgress::Pending(h) => {
1117: ///             handshake = h;
1118: ///             tokio::time::sleep(Duration::from_millis(100)).await;
1119: ///         }
1120: ///         HandshakeProgress::Complete(link) => break link,
1121: ///     }
1122: /// }
1123: /// ```
1124: ///
1125: /// **With timeout:**
1126: /// ```ignore
1127: /// let deadline = Instant::now() + Duration::from_secs(60);
1128: /// loop {
1129: ///     if Instant::now() > deadline {
1130: ///         return Err(/* timeout */);
1131: ///     }
1132: ///     match advance_handshake(handshake).await? {
1133: ///         HandshakeProgress::Pending(h) => {
1134: ///             handshake = h;
1135: ///             tokio::time::sleep(Duration::from_millis(100)).await;
1136: ///         }
1137: ///         HandshakeProgress::Complete(link) => break link,
1138: ///     }
1139: /// }
1140: /// ```
1141: ///
1142: /// # Parameters
1143: /// - `handshake` — the in-progress handshake handle (consumed; returned inside
1144: ///   [`HandshakeProgress::Pending`] if the handshake is not yet finished).
1145: ///
1146: /// # Errors
1147: /// - Returns [`PaykitError::Transport`] if the handshake processing fails, if
1148: ///   the context is in an invalid state, or if automatic recovery is exhausted.
1149: #[instrument(skip(handshake))]
1150: pub async fn advance_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
1151:     // Check whether the handshake has already finished.
1152:     if handshake.encryptor.is_handshake_complete() {
1153:         return finish_handshake(handshake);
1154:     }
1155: 
1156:     // Process the next handshake step.
1157:     match handshake.encryptor.handle_handshake().await {
1158:         Ok(pubky_noise::HandshakeResult::Pending) => {
1159:             debug!("handshake step pending (waiting for peer)");
1160:             handshake.recovery_attempts = 0;
1161:             Ok(HandshakeProgress::Pending(handshake))
1162:         }
1163:         Ok(pubky_noise::HandshakeResult::Terminal) => {
1164:             debug!("handshake terminal, transitioning to transport");
1165:             finish_handshake(handshake)
1166:         }
1167:         Err(pubky_noise::PubkyNoiseError::HomeserverWriteError) => {
1168:             handshake.recovery_attempts += 1;
1169: 
1170:             if handshake.recovery_attempts > handshake.max_recovery_attempts {
1171:                 return Err(PaykitError::Transport {
1172:                     context: format!(
1173:                         "handshake recovery exhausted after {} consecutive attempts",
1174:                         handshake.max_recovery_attempts,
1175:                     ),
1176:                     source: anyhow::anyhow!(
1177:                         "HomeserverWriteError persisted beyond recovery limit ({})",
1178:                         handshake.max_recovery_attempts,
1179:                     ),
1180:                 });
1181:             }
1182: 
1183:             warn!(
1184:                 attempts = handshake.recovery_attempts,
1185:                 max = handshake.max_recovery_attempts,
1186:                 "handshake write failed, attempting automatic recovery from snapshot"
1187:             );
1188: 
1189:             let snapshot = handshake
1190:                 .encryptor
1191:                 .last_good_snapshot()
1192:                 .cloned()
1193:                 .ok_or_else(|| PaykitError::Transport {
1194:                     context: "handshake recovery failed: missing last-good snapshot".into(),
1195:                     source: anyhow::anyhow!(
1196:                         "pubky-noise returned HomeserverWriteError but no recovery snapshot"
1197:                     ),
1198:                 })?;
1199: 
1200:             let restored = pubky_noise::PubkyNoiseEncryptor::restore(
1201:                 handshake.config.clone(),
1202:                 snapshot,
1203:                 handshake.remote_pubkey.clone(),
1204:             )
1205:             .await
1206:             .map_err(|err| PaykitError::Transport {
1207:                 context: format!("handshake recovery via restore() failed: {err:?}"),
1208:                 source: anyhow::anyhow!("restore after HomeserverWriteError failed: {err:?}"),
1209:             })?;
1210: 
1211:             debug!("handshake recovered successfully, returning Pending");
1212:             Ok(HandshakeProgress::Pending(EncryptedLinkHandshake {
1213:                 encryptor: restored,
1214:                 config: handshake.config,
1215:                 remote_pubkey: handshake.remote_pubkey,
1216:                 recovery_attempts: handshake.recovery_attempts,
1217:                 max_recovery_attempts: handshake.max_recovery_attempts,
1218:             }))
1219:         }
1220:         Err(err) => Err(PaykitError::Transport {
1221:             context: format!("handshake step failed: {err:?}"),
1222:             source: anyhow::anyhow!("pubky-noise handle_handshake failed: {err:?}"),
1223:         }),
1224:     }
1225: }
1226: 
1227: /// Transitions a completed handshake into an [`EncryptedLink`].
1228: #[cfg(feature = "pubky")]
1229: fn finish_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
1230:     let _link_id =
1231:         handshake
1232:             .encryptor
1233:             .transition_transport()
1234:             .map_err(|err| PaykitError::Transport {
1235:                 context: format!("failed to transition to transport mode: {err:?}"),
1236:                 source: anyhow::anyhow!("pubky-noise transition_transport failed: {err:?}"),
1237:             })?;
1238: 
1239:     debug!("encrypted link established");

(Showing lines 940-1239 of 2726. Use offset=1240 to continue.)
</content>
```

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-ffi/src/lib.rs"
}
```

**Output:**
```
<path>/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-ffi/src/lib.rs</path>
<type>file</type>
<content>
1: uniffi::setup_scaffolding!();
2: 
3: use once_cell::sync::OnceCell;
4: #[cfg(feature = "dev-auth")]
5: use pubky::Keypair;
6: use pubky::{Pubky, PubkySession, PublicKey};
7: use tokio::runtime::Runtime;
8: use tokio::sync::Mutex as TokioMutex;
9: 
10: use paykit_lib::{
11:     EndpointData, MethodId, PubkyAuthenticatedTransport, PubkyUnauthenticatedTransport,
12: };
13: 
14: // ---------------------------------------------------------------------------
15: // Android logger — routes tracing/log output to logcat
16: // ---------------------------------------------------------------------------
17: 
18: #[cfg(target_os = "android")]
19: fn init_android_logger() {
20:     use std::sync::Once;
21:     static INIT: Once = Once::new();
22:     INIT.call_once(|| {
23:         android_logger::init_once(
24:             android_logger::Config::default()
25:                 .with_max_level(log::LevelFilter::Debug)
26:                 .with_tag("PaykitRust"),
27:         );
28:     });
29: }
30: 
31: // ---------------------------------------------------------------------------
32: // FFI-safe types
33: // ---------------------------------------------------------------------------
34: 
35: #[derive(uniffi::Error, Debug, thiserror::Error)]
36: pub enum PaykitFfiError {
37:     #[error("Transport error: {reason}")]
38:     Transport { reason: String },
39:     #[error("Not found: {reason}")]
40:     NotFound { reason: String },
41:     #[error("Invalid data: {reason}")]
42:     InvalidData { reason: String },
43:     #[error("Validation error: {reason}")]
44:     Validation { reason: String },
45:     #[error("Session error: {reason}")]
46:     Session { reason: String },
47: }
48: 
49: impl From<paykit_lib::PaykitError> for PaykitFfiError {
50:     fn from(err: paykit_lib::PaykitError) -> Self {
51:         match err {
52:             paykit_lib::PaykitError::Transport { context, source } => PaykitFfiError::Transport {
53:                 reason: format!("{context}: {source}"),
54:             },
55:             paykit_lib::PaykitError::NotFound(msg) => PaykitFfiError::NotFound { reason: msg },
56:             paykit_lib::PaykitError::InvalidData { context, source } => {
57:                 let detail = source.map(|s| format!("{context}: {s}")).unwrap_or(context);
58:                 PaykitFfiError::InvalidData { reason: detail }
59:             }
60:             paykit_lib::PaykitError::Validation(msg) => PaykitFfiError::Validation { reason: msg },
61:         }
62:     }
63: }
64: 
65: #[derive(uniffi::Record, Debug, Clone)]
66: pub struct FfiPaymentEntry {
67:     pub method_id: String,
68:     pub endpoint_data: String,
69: }
70: 
71: // ---------------------------------------------------------------------------
72: // Global state
73: // ---------------------------------------------------------------------------
74: 
75: static RUNTIME: OnceCell<Runtime> = OnceCell::new();
76: static PUBKY: OnceCell<Pubky> = OnceCell::new();
77: 
78: struct SessionState {
79:     transport: PubkyAuthenticatedTransport,
80:     session: PubkySession,
81: }
82: 
83: static SESSION: OnceCell<TokioMutex<Option<SessionState>>> = OnceCell::new();
84: 
85: fn ensure_runtime() -> &'static Runtime {
86:     RUNTIME.get_or_init(|| Runtime::new().expect("Failed to create Tokio runtime"))
87: }
88: 
89: fn get_session_lock() -> &'static TokioMutex<Option<SessionState>> {
90:     SESSION.get_or_init(|| TokioMutex::new(None))
91: }
92: 
93: fn get_pubky_client() -> Result<&'static Pubky, PaykitFfiError> {
94:     PUBKY.get().ok_or_else(|| PaykitFfiError::Session {
95:         reason: "Paykit not initialized. Call paykit_initialize() first.".into(),
96:     })
97: }
98: 
99: fn parse_public_key(pk_str: &str) -> Result<PublicKey, PaykitFfiError> {
100:     pk_str
101:         .parse::<PublicKey>()
102:         .map_err(|e| PaykitFfiError::Validation {
103:             reason: format!("Invalid public key '{pk_str}': {e}"),
104:         })
105: }
106: 
107: fn make_reader(pubky: &Pubky) -> PubkyUnauthenticatedTransport {
108:     PubkyUnauthenticatedTransport::new(pubky.public_storage())
109: }
110: 
111: fn runtime_err(e: tokio::task::JoinError) -> PaykitFfiError {
112:     PaykitFfiError::Session {
113:         reason: format!("Runtime error: {e}"),
114:     }
115: }
116: 
117: /// Clone the transport out of the session lock so network I/O doesn't hold it.
118: async fn get_authenticated_transport() -> Result<PubkyAuthenticatedTransport, PaykitFfiError> {
119:     let guard = get_session_lock().lock().await;
120:     let state = guard.as_ref().ok_or_else(|| PaykitFfiError::Session {
121:         reason: "No active session. Call paykit_import_session or paykit_sign_in first.".into(),
122:     })?;
123:     Ok(state.transport.clone())
124: }
125: 
126: // ---------------------------------------------------------------------------
127: // Initialization
128: // ---------------------------------------------------------------------------
129: 
130: /// Create the Pubky SDK facade and initialize logging. Call once at app startup.
131: ///
132: /// Targets the **production** network.
133: ///
134: /// Safe to call multiple times — subsequent calls are no-ops if the first
135: /// succeeded. If it fails (e.g. network issue), call it again to retry.
136: #[uniffi::export]
137: pub async fn paykit_initialize() -> Result<(), PaykitFfiError> {
138:     #[cfg(target_os = "android")]
139:     init_android_logger();
140: 
141:     let rt = ensure_runtime();
142:     rt.spawn(async {
143:         PUBKY.get_or_try_init(|| {
144:             Pubky::new().map_err(|e| PaykitFfiError::Session {
145:                 reason: format!("Failed to initialize Pubky SDK: {e}"),
146:             })
147:         })?;
148:         let _ = get_session_lock();
149:         Ok(())
150:     })
151:     .await
152:     .unwrap_or_else(|e| Err(runtime_err(e)))
153: }
154: 
155: // ---------------------------------------------------------------------------
156: // Session queries
157: // ---------------------------------------------------------------------------
158: 
159: /// Returns `true` if an authenticated session is currently active.
160: #[uniffi::export]
161: pub async fn paykit_is_authenticated() -> bool {
162:     let rt = ensure_runtime();
163:     rt.spawn(async {
164:         let guard = get_session_lock().lock().await;
165:         guard.is_some()
166:     })
167:     .await
168:     .unwrap_or(false)
169: }
170: 
171: /// Returns the public key of the currently authenticated user, or `None`.
172: #[uniffi::export]
173: pub async fn paykit_get_current_public_key() -> Option<String> {
174:     let rt = ensure_runtime();
175:     rt.spawn(async {
176:         let guard = get_session_lock().lock().await;
177:         guard
178:             .as_ref()
179:             .map(|s| s.session.info().public_key().to_string())
180:     })
181:     .await
182:     .unwrap_or(None)
183: }
184: 
185: /// Exports the current session secret for persistence across app restarts.
186: ///
187: /// Returns the compact `<pubkey_z32>:<cookie_secret>` string that can be
188: /// passed back to `paykit_import_session` on next cold start.
189: #[uniffi::export]
190: pub async fn paykit_export_session() -> Result<String, PaykitFfiError> {
191:     let rt = ensure_runtime();
192:     rt.spawn(async {
193:         let guard = get_session_lock().lock().await;
194:         let state = guard.as_ref().ok_or_else(|| PaykitFfiError::Session {
195:             reason: "No active session to export.".into(),
196:         })?;
197:         Ok(state.session.export_secret())
198:     })
199:     .await
200:     .unwrap_or_else(|e| Err(runtime_err(e)))
201: }
202: 
203: // ---------------------------------------------------------------------------
204: // Read operations
205: // ---------------------------------------------------------------------------
206: 
207: /// Fetch all published payment methods for a user.
208: #[uniffi::export]
209: pub async fn paykit_get_payment_list(
210:     public_key: String,
211: ) -> Result<Vec<FfiPaymentEntry>, PaykitFfiError> {
212:     let rt = ensure_runtime();
213:     rt.spawn(async move {
214:         let pubky = get_pubky_client()?;
215:         let pk = parse_public_key(&public_key)?;
216:         let reader = make_reader(pubky);
217:         let payments = paykit_lib::get_payment_list(&reader, &pk).await?;
218:         Ok(payments
219:             .entries
220:             .into_iter()
221:             .map(|(method, data)| FfiPaymentEntry {
222:                 method_id: method.as_str().to_string(),
223:                 endpoint_data: data.into_inner(),
224:             })
225:             .collect())
226:     })
227:     .await
228:     .unwrap_or_else(|e| Err(runtime_err(e)))
229: }
230: 
231: /// Fetch a single payment endpoint for a user and method. Returns `None` if not set.
232: #[uniffi::export]
233: pub async fn paykit_get_payment_endpoint(
234:     public_key: String,
235:     method_id: String,
236: ) -> Result<Option<String>, PaykitFfiError> {
237:     let rt = ensure_runtime();
238:     rt.spawn(async move {
239:         let pubky = get_pubky_client()?;
240:         let pk = parse_public_key(&public_key)?;
241:         let method = MethodId::new(method_id)?;
242:         let reader = make_reader(pubky);
243:         let endpoint = paykit_lib::get_payment_endpoint(&reader, &pk, &method).await?;
244:         Ok(endpoint.map(|d| d.into_inner()))
245:     })
246:     .await
247:     .unwrap_or_else(|e| Err(runtime_err(e)))
248: }
249: 
250: // ---------------------------------------------------------------------------
251: // Authentication
252: // ---------------------------------------------------------------------------
253: 
254: /// Import a session from a Pubky Ring auth flow.
255: ///
256: /// Accepts a compact session secret (`<pubkey_z32>:<cookie_secret>`) produced
257: /// by `PubkySession::export_secret()`. Validates with the homeserver and stores
258: /// the session for subsequent write operations.
259: #[uniffi::export]
260: pub async fn paykit_import_session(session_secret: String) -> Result<String, PaykitFfiError> {
261:     let rt = ensure_runtime();
262:     rt.spawn(async move {
263:         let pubky = get_pubky_client()?;
264:         let client = pubky.client().clone();
265:         let session = PubkySession::import_secret(&session_secret, Some(client))
266:             .await
267:             .map_err(|e| PaykitFfiError::Session {
268:                 reason: format!("Failed to import session: {e}"),
269:             })?;
270: 
271:         let public_key = session.info().public_key().to_string();
272:         let transport = PubkyAuthenticatedTransport::new(session.clone());
273: 
274:         let mut guard = get_session_lock().lock().await;
275:         *guard = Some(SessionState { transport, session });
276: 
277:         Ok(public_key)
278:     })
279:     .await
280:     .unwrap_or_else(|e| Err(runtime_err(e)))
281: }
282: 
283: /// Sign up for a new account using a raw secret key. Only available with
284: /// the `dev-auth` feature (enabled by default, disable for production builds).
285: #[cfg(feature = "dev-auth")]
286: #[uniffi::export]
287: pub async fn paykit_sign_up(
288:     secret_key_hex: String,
289:     homeserver_public_key: String,
290: ) -> Result<String, PaykitFfiError> {
291:     let rt = ensure_runtime();
292:     rt.spawn(async move {
293:         let pubky = get_pubky_client()?;
294:         let keypair = keypair_from_hex(&secret_key_hex)?;
295:         let hs_pk = parse_public_key(&homeserver_public_key)?;
296: 
297:         let signer = pubky.signer(keypair);
298:         let session = signer
299:             .signup(&hs_pk, None)
300:             .await
301:             .map_err(|e| PaykitFfiError::Session {
302:                 reason: format!("Signup failed: {e}"),
303:             })?;
304: 
305:         let public_key = session.info().public_key().to_string();
306:         let transport = PubkyAuthenticatedTransport::new(session.clone());
307: 
308:         let mut guard = get_session_lock().lock().await;
309:         *guard = Some(SessionState { transport, session });
310: 
311:         Ok(public_key)
312:     })
313:     .await
314:     .unwrap_or_else(|e| Err(runtime_err(e)))
315: }
316: 
317: /// Sign in with a raw secret key. Only available with the `dev-auth`
318: /// feature (enabled by default, disable for production builds).
319: ///
320: /// The homeserver is resolved automatically via PKDNS.
321: #[cfg(feature = "dev-auth")]
322: #[uniffi::export]
323: pub async fn paykit_sign_in(secret_key_hex: String) -> Result<String, PaykitFfiError> {
324:     let rt = ensure_runtime();
325:     rt.spawn(async move {
326:         let pubky = get_pubky_client()?;
327:         let keypair = keypair_from_hex(&secret_key_hex)?;
328: 
329:         let signer = pubky.signer(keypair);
330:         let session = signer.signin().await.map_err(|e| PaykitFfiError::Session {
331:             reason: format!("Signin failed: {e}"),
332:         })?;
333: 
334:         let public_key = session.info().public_key().to_string();
335:         let transport = PubkyAuthenticatedTransport::new(session.clone());
336: 
337:         let mut guard = get_session_lock().lock().await;
338:         *guard = Some(SessionState { transport, session });
339: 
340:         Ok(public_key)
341:     })
342:     .await
343:     .unwrap_or_else(|e| Err(runtime_err(e)))
344: }
345: 
346: // ---------------------------------------------------------------------------
347: // Write operations
348: // ---------------------------------------------------------------------------
349: 
350: /// Publish or update a payment endpoint for the authenticated user.
351: #[uniffi::export]
352: pub async fn paykit_set_payment_endpoint(
353:     method_id: String,
354:     endpoint_data: String,
355: ) -> Result<(), PaykitFfiError> {
356:     let rt = ensure_runtime();
357:     rt.spawn(async move {
358:         let method = MethodId::new(method_id)?;
359:         let data = EndpointData::new(endpoint_data);
360:         let transport = get_authenticated_transport().await?;
361: 
362:         paykit_lib::set_payment_endpoint(&transport, method, data).await?;
363:         Ok(())
364:     })
365:     .await
366:     .unwrap_or_else(|e| Err(runtime_err(e)))
367: }
368: 
369: /// Remove a payment endpoint for the authenticated user.
370: #[uniffi::export]
371: pub async fn paykit_remove_payment_endpoint(method_id: String) -> Result<(), PaykitFfiError> {
372:     let rt = ensure_runtime();
373:     rt.spawn(async move {
374:         let method = MethodId::new(method_id)?;
375:         let transport = get_authenticated_transport().await?;
376: 
377:         paykit_lib::remove_payment_endpoint(&transport, method).await?;
378:         Ok(())
379:     })
380:     .await
381:     .unwrap_or_else(|e| Err(runtime_err(e)))
382: }
383: 
384: /// End the current session on the homeserver and clear local state.
385: ///
386: /// If the server request fails the session is restored so no data is lost.
387: #[uniffi::export]
388: pub async fn paykit_sign_out() -> Result<(), PaykitFfiError> {
389:     let rt = ensure_runtime();
390:     rt.spawn(async move {
391:         let mut guard = get_session_lock().lock().await;
392:         let state = guard.take().ok_or_else(|| PaykitFfiError::Session {
393:             reason: "No active session to sign out of.".into(),
394:         })?;
395: 
396:         match state.session.signout().await {
397:             Ok(()) => Ok(()),
398:             Err((e, returned_session)) => {
399:                 *guard = Some(SessionState {
400:                     transport: PubkyAuthenticatedTransport::new(returned_session.clone()),
401:                     session: returned_session,
402:                 });
403:                 Err(PaykitFfiError::Session {
404:                     reason: format!("Signout failed: {e}"),
405:                 })
406:             }
407:         }
408:     })
409:     .await
410:     .unwrap_or_else(|e| Err(runtime_err(e)))
411: }
412: 
413: /// Discard the local session without contacting the homeserver.
414: ///
415: /// Idempotent — safe to call even when no session exists.
416: /// The server-side session will expire on its own.
417: #[uniffi::export]
418: pub async fn paykit_force_sign_out() {
419:     let rt = ensure_runtime();
420:     let _ = rt
421:         .spawn(async move {
422:             let mut guard = get_session_lock().lock().await;
423:             guard.take();
424:         })
425:         .await;
426: }
427: 
428: // ---------------------------------------------------------------------------
429: // Helpers
430: // ---------------------------------------------------------------------------
431: 
432: #[cfg(feature = "dev-auth")]
433: fn keypair_from_hex(hex_str: &str) -> Result<Keypair, PaykitFfiError> {
434:     let bytes = hex::decode(hex_str).map_err(|e| PaykitFfiError::Validation {
435:         reason: format!("Invalid hex secret key: {e}"),
436:     })?;
437:     let secret: [u8; 32] = bytes
438:         .try_into()
439:         .map_err(|v: Vec<u8>| PaykitFfiError::Validation {
440:             reason: format!(
441:                 "Secret key must be exactly 32 bytes (64 hex chars), got {} bytes",
442:                 v.len()
443:             ),
444:         })?;
445:     Ok(Keypair::from_secret(&secret))
446: }

(End of file - total 446 lines)
</content>
```

---

## Assistant (Plan · Claude Opus 4.6 · 5.0s)

_Thinking:_

Now let me look at the remaining parts of lib.rs - specifically the serialization helpers, the private path computation, and the rest of the encrypted link functions.

Let me check the serialization helpers and remaining utility functions:

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs",
  "offset": 1240,
  "limit": 500
}
```

**Output:**
```
<path>/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs</path>
<type>file</type>
<content>
1240:     Ok(HandshakeProgress::Complete(EncryptedLink {
1241:         encryptor: handshake.encryptor,
1242:         recipient: handshake.remote_pubkey,
1243:         config: handshake.config,
1244:         max_send_retries: DEFAULT_MAX_SEND_RETRIES,
1245:     }))
1246: }
1247: 
1248: #[cfg(feature = "pubky")]
1249: /// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot.
1250: ///
1251: /// Use this to resume an in-progress handshake after an app restart. A fresh
1252: /// [`pubky_noise::PubkyNoiseConfig`] is built from the supplied session and key
1253: /// material, then replay restore reconstructs the handshake state from the
1254: /// persisted snapshot and homeserver data.
1255: ///
1256: /// # Parameters
1257: /// - `session` — authenticated Pubky session for writing handshake messages
1258: ///   (a fresh session after app restart).
1259: /// - `secret_key` — 32-byte Ed25519 secret key of the local peer (same key
1260: ///   used in the original [`initiate_encrypted_link`] or
1261: ///   [`accept_encrypted_link`] call).
1262: /// - `remote_pubkey` — public key of the remote peer.
1263: /// - `outbox_client` — HTTP client for reading from the remote homeserver.
1264: /// - `snapshot` — saved in-progress handshake snapshot (from
1265: ///   [`EncryptedLinkHandshake::snapshot`] or
1266: ///   [`EncryptedLinkHandshakeSnapshot::deserialize`]).
1267: ///
1268: /// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
1269: /// inconsistent caller input and is rejected.
1270: ///
1271: /// # Restore behavior
1272: ///
1273: /// Restored handshakes always reset recovery tuning to defaults:
1274: /// - `recovery_attempts = 0`
1275: /// - `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`
1276: ///
1277: /// # Errors
1278: /// Returns [`PaykitError::Transport`] if the Noise configuration cannot be
1279: /// created or if the underlying `restore()` fails. Returns
1280: /// [`PaykitError::Validation`] when `remote_pubkey` does not match the
1281: /// recipient embedded in `snapshot`, or when the snapshot is not in handshake
1282: /// phase.
1283: #[instrument(skip(session, secret_key, outbox_client, snapshot))]
1284: pub async fn restore_encrypted_link_handshake(
1285:     session: pubky::PubkySession,
1286:     secret_key: [u8; 32],
1287:     remote_pubkey: &PublicKey,
1288:     outbox_client: pubky::Pubky,
1289:     snapshot: EncryptedLinkHandshakeSnapshot,
1290: ) -> Result<EncryptedLinkHandshake> {
1291:     debug!("restoring encrypted link handshake from snapshot (raw params)");
1292: 
1293:     let (write_path, read_path) = compute_private_payment_paths(&secret_key, remote_pubkey);
1294: 
1295:     let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
1296:         secret_key,
1297:         0,
1298:         "XX",
1299:         session,
1300:         write_path,
1301:         read_path,
1302:         outbox_client,
1303:     )
1304:     .map_err(|err| PaykitError::Transport {
1305:         context: format!("failed to create encryptor config for handshake restore: {err:?}"),
1306:         source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
1307:     })?;
1308: 
1309:     restore_encrypted_link_handshake_inner(config, remote_pubkey, snapshot).await
1310: }
1311: 
1312: #[cfg(feature = "pubky")]
1313: /// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot
1314: /// using an existing Noise configuration.
1315: ///
1316: /// This is the in-process variant of [`restore_encrypted_link_handshake`] — use
1317: /// it when the original `Arc<PubkyNoiseConfig>` is still available.
1318: ///
1319: /// # Parameters
1320: /// - `config` — shared Noise configuration matching the original handshake
1321: ///   session.
1322: /// - `remote_pubkey` — public key of the remote peer.
1323: /// - `snapshot` — saved in-progress handshake snapshot.
1324: ///
1325: /// # Restore behavior
1326: ///
1327: /// Restored handshakes always reset recovery tuning to defaults:
1328: /// - `recovery_attempts = 0`
1329: /// - `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`
1330: ///
1331: /// # Errors
1332: /// Returns [`PaykitError::Transport`] if the underlying `restore()` fails.
1333: /// Returns [`PaykitError::Validation`] when `remote_pubkey` does not match the
1334: /// recipient embedded in `snapshot`, or when the snapshot is not in handshake
1335: /// phase.
1336: #[instrument(skip(config, snapshot))]
1337: pub async fn restore_encrypted_link_handshake_from_config(
1338:     config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
1339:     remote_pubkey: &PublicKey,
1340:     snapshot: EncryptedLinkHandshakeSnapshot,
1341: ) -> Result<EncryptedLinkHandshake> {
1342:     debug!("restoring encrypted link handshake from snapshot (existing config)");
1343:     restore_encrypted_link_handshake_inner(config, remote_pubkey, snapshot).await
1344: }
1345: 
1346: /// Shared implementation for both handshake restore variants.
1347: #[cfg(feature = "pubky")]
1348: async fn restore_encrypted_link_handshake_inner(
1349:     config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
1350:     remote_pubkey: &PublicKey,
1351:     snapshot: EncryptedLinkHandshakeSnapshot,
1352: ) -> Result<EncryptedLinkHandshake> {
1353:     if snapshot.recipient() != remote_pubkey {
1354:         return Err(PaykitError::Validation(format!(
1355:             "remote_pubkey does not match snapshot recipient (remote={}, snapshot={})",
1356:             remote_pubkey,
1357:             snapshot.recipient(),
1358:         )));
1359:     }
1360: 
1361:     if !matches!(
1362:         snapshot.state.phase,
1363:         pubky_noise::snow_crypto::NoisePhase::HandShake
1364:     ) {
1365:         return Err(PaykitError::Validation(format!(
1366:             "handshake restore requires handshake-phase snapshot, got {:?}",
1367:             snapshot.state.phase,
1368:         )));
1369:     }
1370: 
1371:     let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
1372:         config.clone(),
1373:         snapshot.state,
1374:         remote_pubkey.clone(),
1375:     )
1376:     .await
1377:     .map_err(|err| PaykitError::Transport {
1378:         context: format!("failed to restore encrypted link handshake: {err:?}"),
1379:         source: anyhow::anyhow!("pubky-noise handshake restore failed: {err:?}"),
1380:     })?;
1381: 
1382:     debug!("encrypted link handshake restored successfully (recovery tuning reset to defaults)");
1383: 
1384:     Ok(EncryptedLinkHandshake {
1385:         encryptor,
1386:         remote_pubkey: remote_pubkey.clone(),
1387:         config,
1388:         recovery_attempts: 0,
1389:         max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
1390:     })
1391: }
1392: 
1393: #[cfg(feature = "pubky")]
1394: /// Closes an encrypted link and cleans up the Noise session state.
1395: ///
1396: /// After calling this function, the [`EncryptedLink`] is consumed and can no
1397: /// longer be used for encryption or decryption.
1398: #[instrument(skip(link))]
1399: pub async fn close_encrypted_link(mut link: EncryptedLink) -> Result<()> {
1400:     debug!("closing encrypted link");
1401:     link.encryptor.close();
1402:     debug!("encrypted link closed successfully");
1403:     Ok(())
1404: }
1405: 
1406: #[cfg(feature = "pubky")]
1407: /// Restores an [`EncryptedLink`] from a previously saved snapshot.
1408: ///
1409: /// Use this to resume an encrypted session after an app restart without
1410: /// re-doing the Noise handshake. The restore mechanism replays all handshake
1411: /// messages from the homeservers through a fresh Noise state built with the
1412: /// same ephemeral key material, then transitions to transport mode and sets
1413: /// the nonces/counter from the saved state.
1414: ///
1415: /// # Parameters
1416: /// - `session` — authenticated Pubky session for writing messages
1417: ///   (a fresh session after app restart).
1418: /// - `secret_key` — 32-byte Ed25519 secret key of the local peer (same key
1419: ///   used in the original [`initiate_encrypted_link`] or
1420: ///   [`accept_encrypted_link`] call).
1421: /// - `remote_pubkey` — public key of the remote peer.
1422: /// - `outbox_client` — HTTP client for reading from the remote homeserver.
1423: /// - `snapshot` — the saved snapshot (from [`EncryptedLink::snapshot`] or
1424: ///   [`EncryptedLinkSnapshot::deserialize`]).
1425: ///
1426: /// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
1427: /// inconsistent caller input and is rejected.
1428: ///
1429: /// # Errors
1430: /// Returns [`PaykitError::Transport`] if the Noise configuration cannot be
1431: /// created or if the underlying `restore()` fails (e.g. handshake messages
1432: /// are no longer available on the homeservers, or the replayed handshake
1433: /// hash does not match the saved one). Returns [`PaykitError::Validation`]
1434: /// when `remote_pubkey` does not match the recipient embedded in `snapshot`.
1435: #[instrument(skip(session, secret_key, outbox_client, snapshot))]
1436: pub async fn restore_encrypted_link(
1437:     session: pubky::PubkySession,
1438:     secret_key: [u8; 32],
1439:     remote_pubkey: &PublicKey,
1440:     outbox_client: pubky::Pubky,
1441:     snapshot: EncryptedLinkSnapshot,
1442: ) -> Result<EncryptedLink> {
1443:     debug!("restoring encrypted link from snapshot (raw params)");
1444: 
1445:     let (write_path, read_path) = compute_private_payment_paths(&secret_key, remote_pubkey);
1446: 
1447:     let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
1448:         secret_key,
1449:         0,
1450:         "XX",
1451:         session,
1452:         write_path,
1453:         read_path,
1454:         outbox_client,
1455:     )
1456:     .map_err(|err| PaykitError::Transport {
1457:         context: format!("failed to create encryptor config for restore: {err:?}"),
1458:         source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
1459:     })?;
1460: 
1461:     restore_encrypted_link_inner(config, remote_pubkey, snapshot).await
1462: }
1463: 
1464: #[cfg(feature = "pubky")]
1465: /// Restores an [`EncryptedLink`] from a previously saved snapshot using an
1466: /// existing Noise configuration.
1467: ///
1468: /// This is the in-process variant of [`restore_encrypted_link`] — use it when
1469: /// the original `Arc<PubkyNoiseConfig>` is still available (e.g. the link
1470: /// needs rebuilding without an app restart). For cross-restart recovery, use
1471: /// [`restore_encrypted_link`] instead.
1472: ///
1473: /// # Parameters
1474: /// - `config` — the shared Noise configuration (must match the original
1475: ///   session's write/read paths and keypair).
1476: /// - `remote_pubkey` — public key of the remote peer.
1477: /// - `snapshot` — the saved snapshot.
1478: ///
1479: /// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
1480: /// inconsistent caller input and is rejected.
1481: ///
1482: /// # Errors
1483: /// Returns [`PaykitError::Transport`] if the underlying `restore()` fails.
1484: /// Returns [`PaykitError::Validation`] when `remote_pubkey` does not match the
1485: /// recipient embedded in `snapshot`.
1486: #[instrument(skip(config, snapshot))]
1487: pub async fn restore_encrypted_link_from_config(
1488:     config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
1489:     remote_pubkey: &PublicKey,
1490:     snapshot: EncryptedLinkSnapshot,
1491: ) -> Result<EncryptedLink> {
1492:     debug!("restoring encrypted link from snapshot (existing config)");
1493:     restore_encrypted_link_inner(config, remote_pubkey, snapshot).await
1494: }
1495: 
1496: /// Shared implementation for both restore variants.
1497: #[cfg(feature = "pubky")]
1498: async fn restore_encrypted_link_inner(
1499:     config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
1500:     remote_pubkey: &PublicKey,
1501:     snapshot: EncryptedLinkSnapshot,
1502: ) -> Result<EncryptedLink> {
1503:     if snapshot.recipient() != remote_pubkey {
1504:         return Err(PaykitError::Validation(format!(
1505:             "remote_pubkey does not match snapshot recipient (remote={}, snapshot={})",
1506:             remote_pubkey,
1507:             snapshot.recipient(),
1508:         )));
1509:     }
1510: 
1511:     if !matches!(
1512:         snapshot.state.phase,
1513:         pubky_noise::snow_crypto::NoisePhase::Transport
1514:     ) {
1515:         return Err(PaykitError::Validation(format!(
1516:             "encrypted link restore requires transport-phase snapshot, got {:?}",
1517:             snapshot.state.phase,
1518:         )));
1519:     }
1520: 
1521:     let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
1522:         config.clone(),
1523:         snapshot.state,
1524:         remote_pubkey.clone(),
1525:     )
1526:     .await
1527:     .map_err(|err| PaykitError::Transport {
1528:         context: format!("failed to restore encrypted link: {err:?}"),
1529:         source: anyhow::anyhow!("pubky-noise restore failed: {err:?}"),
1530:     })?;
1531: 
1532:     debug!("encrypted link restored successfully");
1533:     Ok(EncryptedLink {
1534:         encryptor,
1535:         recipient: remote_pubkey.clone(),
1536:         config,
1537:         max_send_retries: DEFAULT_MAX_SEND_RETRIES,
1538:     })
1539: }
1540: 
1541: fn map_error(label: &'static str, err: PaykitError) -> PaykitError {
1542:     match err {
1543:         PaykitError::Transport { context, source } => PaykitError::Transport {
1544:             context: format!("{label}: {context}"),
1545:             source,
1546:         },
1547:         PaykitError::NotFound(msg) => PaykitError::NotFound(format!("{label}: {msg}")),
1548:         PaykitError::InvalidData { context, source } => PaykitError::InvalidData {
1549:             context: format!("{label}: {context}"),
1550:             source,
1551:         },
1552:         PaykitError::Validation(msg) => PaykitError::Validation(format!("{label}: {msg}")),
1553:     }
1554: }
1555: 
1556: /// Unit tests for input validation (no network required).
1557: #[cfg(test)]
1558: mod validation_tests {
1559:     use super::*;
1560: 
1561:     // ── MethodId: valid inputs ──────────────────────────────────────────
1562: 
1563:     #[test]
1564:     fn test_method_id_valid_simple_names() {
1565:         for name in ["bitcoin-bolt11", "bitcoin-bolt12", "bitcoin-p2tr"] {
1566:             assert!(MethodId::new(name).is_ok(), "expected '{name}' to be valid");
1567:         }
1568:     }
1569: 
1570:     #[test]
1571:     fn test_method_id_valid_with_dots() {
1572:         let m = MethodId::new("method.v2").unwrap();
1573:         assert_eq!(m.as_str(), "method.v2");
1574:     }
1575: 
1576:     #[test]
1577:     fn test_method_id_valid_with_underscores() {
1578:         let m = MethodId::new("my_method").unwrap();
1579:         assert_eq!(m.as_str(), "my_method");
1580:     }
1581: 
1582:     #[test]
1583:     fn test_method_id_valid_mixed_case() {
1584:         let m = MethodId::new("LnUrl-Pay").unwrap();
1585:         assert_eq!(m.as_str(), "LnUrl-Pay");
1586:     }
1587: 
1588:     #[test]
1589:     fn test_method_id_valid_max_length() {
1590:         let name = "a".repeat(METHOD_ID_MAX_LEN);
1591:         assert!(MethodId::new(&name).is_ok());
1592:     }
1593: 
1594:     #[test]
1595:     fn test_method_id_valid_single_char() {
1596:         assert!(MethodId::new("x").is_ok());
1597:     }
1598: 
1599:     #[test]
1600:     fn test_method_id_display() {
1601:         let m = MethodId::new("lightning").unwrap();
1602:         assert_eq!(format!("{m}"), "lightning");
1603:     }
1604: 
1605:     #[test]
1606:     fn test_method_id_as_ref() {
1607:         let m = MethodId::new("onchain").unwrap();
1608:         let s: &str = m.as_ref();
1609:         assert_eq!(s, "onchain");
1610:     }
1611: 
1612:     // ── MethodId: invalid inputs ────────────────────────────────────────
1613: 
1614:     #[test]
1615:     fn test_method_id_reject_empty() {
1616:         let err = MethodId::new("").unwrap_err();
1617:         assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("empty")));
1618:     }
1619: 
1620:     #[test]
1621:     fn test_method_id_reject_path_traversal_dotdot() {
1622:         assert!(MethodId::new("..").is_err());
1623:     }
1624: 
1625:     #[test]
1626:     fn test_method_id_reject_path_traversal_dot() {
1627:         assert!(MethodId::new(".").is_err());
1628:     }
1629: 
1630:     #[test]
1631:     fn test_method_id_reject_path_traversal_sequence() {
1632:         // Slashes are rejected by the character allowlist, but verify the
1633:         // specific traversal pattern is caught.
1634:         assert!(MethodId::new("../etc/passwd").is_err());
1635:         assert!(MethodId::new("../../foo").is_err());
1636:     }
1637: 
1638:     #[test]
1639:     fn test_method_id_reject_forward_slash() {
1640:         assert!(MethodId::new("foo/bar").is_err());
1641:     }
1642: 
1643:     #[test]
1644:     fn test_method_id_reject_backslash() {
1645:         assert!(MethodId::new("a\\b").is_err());
1646:     }
1647: 
1648:     #[test]
1649:     fn test_method_id_reject_null_byte() {
1650:         assert!(MethodId::new("foo\0bar").is_err());
1651:     }
1652: 
1653:     #[test]
1654:     fn test_method_id_reject_too_long() {
1655:         let name = "a".repeat(METHOD_ID_MAX_LEN + 1);
1656:         let err = MethodId::new(&name).unwrap_err();
1657:         assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("exceed")));
1658:     }
1659: 
1660:     #[test]
1661:     fn test_method_id_reject_space() {
1662:         assert!(MethodId::new("foo bar").is_err());
1663:     }
1664: 
1665:     #[test]
1666:     fn test_method_id_reject_special_chars() {
1667:         for bad in ["foo@bar", "foo:bar", "foo?bar", "foo#bar", "foo=bar"] {
1668:             assert!(
1669:                 MethodId::new(bad).is_err(),
1670:                 "expected '{bad}' to be rejected"
1671:             );
1672:         }
1673:     }
1674: 
1675:     #[test]
1676:     fn test_method_id_reject_unicode() {
1677:         assert!(MethodId::new("⚡lightning").is_err());
1678:     }
1679: 
1680:     #[test]
1681:     fn test_method_id_reject_triple_dots() {
1682:         assert!(MethodId::new("...").is_err());
1683:     }
1684: 
1685:     #[test]
1686:     fn test_method_id_reject_reserved_private() {
1687:         let err = MethodId::new("private").unwrap_err();
1688:         assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("reserved")));
1689:     }
1690: 
1691:     // ── EndpointData: basic accessors ───────────────────────────────────
1692: 
1693:     #[test]
1694:     fn test_endpoint_data_new_and_accessors() {
1695:         let d = EndpointData::new("ln...");
1696:         assert_eq!(d.as_str(), "ln...");
1697:         assert_eq!(format!("{d}"), "ln...");
1698:     }
1699: 
1700:     #[test]
1701:     fn test_endpoint_data_into_inner() {
1702:         let d = EndpointData::new("payload");
1703:         assert_eq!(d.into_inner(), "payload");
1704:     }
1705: 
1706:     #[test]
1707:     fn test_endpoint_data_as_ref() {
1708:         let d = EndpointData::new("data");
1709:         let s: &str = d.as_ref();
1710:         assert_eq!(s, "data");
1711:     }
1712: }
1713: 
1714: /// Integration tests (require `pubky` feature and ephemeral testnet).
1715: #[cfg(all(test, feature = "pubky"))]
1716: mod tests {
1717:     use std::collections::HashMap;
1718: 
1719:     use super::*;
1720:     use pubky::PubkySession;
1721:     use pubky_testnet::{pubky::Keypair, EphemeralTestnet};
1722: 
1723:     struct TestSetup {
1724:         _testnet: EphemeralTestnet,
1725:         session_transport: PubkyAuthenticatedTransport,
1726:         reader_transport: PubkyUnauthenticatedTransport,
1727:         raw_session: PubkySession,
1728:         public_key: PublicKey,
1729:     }
1730: 
1731:     impl TestSetup {
1732:         async fn new() -> Self {
1733:             let testnet = EphemeralTestnet::builder()
1734:                 .with_embedded_postgres()
1735:                 .build()
1736:                 .await
1737:                 .unwrap();
1738: 
1739:             let homeserver = testnet.homeserver_app();

(Showing lines 1240-1739 of 2726. Use offset=1740 to continue.)
</content>
```

---

## Assistant (Plan · Claude Opus 4.6 · 5.1s)

_Thinking:_

Let me also look at the rest of lib.rs to understand the serialization functions and the path derivation code.

**Tool: grep**

**Input:**
```json
{
  "pattern": "fn serialize_private_payments_json|fn parse_private_payments_json|fn compute_private_payment_paths|fn send_attempts_from_retries",
  "include": "*.rs"
}
```

**Output:**
```
Found 4 matches
/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs:
  Line 634: fn compute_private_payment_paths(
  Line 651: fn parse_private_payments_json(json: &str) -> Result<HashMap<MethodId, EndpointData>> {
  Line 671: fn serialize_private_payments_json(entries: &HashMap<MethodId, EndpointData>) -> Result<String> {
  Line 683: fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
```

---

## Assistant (Plan · Claude Opus 4.6 · 3.7s)

**Tool: read**

**Input:**
```json
{
  "filePath": "/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs",
  "offset": 620,
  "limit": 80
}
```

**Output:**
```
<path>/home/u/Projects/Synonym/Paykit/paykit-rs/paykit-lib/src/lib.rs</path>
<type>file</type>
<content>
620: /// read_path  = "{base}/{hex(SHA-256(domain || dh_secret || remote_pk))}"
621: /// ```
622: ///
623: /// # Returns
624: ///
625: /// A tuple `(write_path, read_path)` where:
626: /// - `write_path` — the full path the local party writes to on their own homeserver.
627: /// - `read_path` — the full path the local party reads from on the remote homeserver.
628: ///
629: /// # Correctness
630: ///
631: /// For parties Alice and Bob:
632: /// - `compute_private_paths(alice_sk, bob_pk).write == compute_private_paths(bob_sk, alice_pk).read`
633: /// - `compute_private_paths(alice_sk, bob_pk).read == compute_private_paths(bob_sk, alice_pk).write`
634: fn compute_private_payment_paths(
635:     local_secret_key: &[u8; 32],
636:     remote_pubkey: &PublicKey,
637: ) -> (String, String) {
638:     pubky_noise::path_derivation::derive_asymmetric_paths(
639:         local_secret_key,
640:         remote_pubkey,
641:         PAYKIT_PATH_DOMAIN,
642:         transport::pubky::PAYKIT_PRIVATE_PATH_PREFIX,
643:     )
644: }
645: 
646: #[cfg(feature = "pubky")]
647: /// Deserializes a private payments JSON blob into a map of method IDs to
648: /// endpoint data.
649: ///
650: /// The expected format is `{ "method_id": "endpoint_value", ... }`.
651: fn parse_private_payments_json(json: &str) -> Result<HashMap<MethodId, EndpointData>> {
652:     let map: HashMap<String, String> =
653:         serde_json::from_str(json).map_err(|err| PaykitError::InvalidData {
654:             context: format!("failed to parse private payments JSON: {err}"),
655:             source: Some(err.into()),
656:         })?;
657: 
658:     let mut result = HashMap::new();
659:     for (key, value) in map {
660:         let method_id = MethodId::new(&key).map_err(|err| PaykitError::InvalidData {
661:             context: format!("private payments blob contains invalid method identifier '{key}'"),
662:             source: Some(err.into()),
663:         })?;
664:         result.insert(method_id, EndpointData::new(value));
665:     }
666:     Ok(result)
667: }
668: 
669: /// Serializes a map of method IDs to endpoint data into a JSON string.
670: #[cfg(feature = "pubky")]
671: fn serialize_private_payments_json(entries: &HashMap<MethodId, EndpointData>) -> Result<String> {
672:     let map: HashMap<&str, &str> = entries
673:         .iter()
674:         .map(|(k, v)| (k.as_str(), v.as_str()))
675:         .collect();
676:     serde_json::to_string(&map).map_err(|err| PaykitError::InvalidData {
677:         context: format!("failed to serialize private payments JSON: {err}"),
678:         source: Some(err.into()),
679:     })
680: }
681: 
682: #[cfg(feature = "pubky")]
683: fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
684:     max_send_retries.saturating_add(1)
685: }
686: 
687: /// Stores or updates a payment endpoint via the injected authenticated client.
688: ///
689: /// # Examples
690: /// ```
691: /// # use paykit_lib::{set_payment_endpoint, MethodId, EndpointData, PublicKey};
692: /// # use paykit_lib::AuthenticatedTransport;
693: /// # async fn demo(client: &impl AuthenticatedTransport) -> paykit_lib::Result<()> {
694: /// let method = MethodId::new("bitcoin-bolt11")?;
695: /// let data = EndpointData::new("ln...");
696: /// set_payment_endpoint(client, method, data).await?;
697: /// # Ok(())
698: /// # }
699: /// ```

(Showing lines 620-699 of 2726. Use offset=700 to continue.)
</content>
```

---

## Assistant (Plan · Claude Opus 4.6 · 2499.9s)

_Thinking:_

Now I have a thorough understanding of the codebase. Let me also check a few more things before I write a plan:

1. How the EncryptedLink is used and whether there's a separate path derivation mechanism I need to understand for payment requests
2. What the tests look like to understand what patterns to follow

Let me now formulate the plan. Based on my analysis:

**Current State:**
- Private payments use `EncryptedLink` (which wraps `PubkyNoiseEncryptor`) to send/receive encrypted messages
- The JSON format for private payments is `{ "method_id": "endpoint_value", ... }` 


- Messages flow through the encrypted link's send and receive methods, with the same channel handling both directions
- Private payments are essentially encrypted versions of the public payment endpoint data

**What Payment Requests need:**
- A message from one peer to another requesting payment of a specific amount using supported methods
- The request includes details like supported payment methods, frequency, start/end times, amount, and currency
- At the library level, this needs to be simpler than the daemon-level spec

**Key Design Questions:**
1. Should payment requests share the same encrypted link as private payments, or use a separate one? Using the same link means messages get interleaved and need differentiation, while a separate path keeps things clean but adds handshake overhead

2. What structure should PaymentRequest have? It needs to include accepted payment methods, amount and currency, optional scheduling data, and an identifier for reference

3. How do I differentiate payment requests from other message types on the encrypted link?
   - Option A: Different base path with its own encrypted link
   - Option B: Same link but with a message envelope that distinguishes types
   - Option C: Same path derivation but different domain string

The user's request suggests using the same mechanism as private payments but with different payloads. The real constraint is that pubky-noise's message queue is sequential—if both payment requests and private payments use the same queue, they'd interleave without a way to distinguish them. The current implementation drains all messages and keeps only the latest, which won't work if I need to handle both types simultaneously.

I'm leaning toward a separate path prefix for payment requests to avoid this collision, though that means doubling the handshake overhead. Alternatively, I could use the same encrypted link with a message envelope wrapping the payload type, but that would break the current message handling logic. backward compatibility with the existing private payments format, and would also mean `get_private_payments` needs to understand envelopes.

I think the cleanest approach for this library-level feature is:

**Option A: Separate encrypted link with separate paths**
- New path prefix: `/pub/paykit/v0/requests`
- New domain: `b"paykit-requests-v0"`
- Requires separate handshake per peer for requests channel
- Pro: Clean separation, no backward compatibility concerns
- Con: Additional handshake overhead per peer relationship

**Option B: Message type envelope on the same link**
- Wrap all messages in `{ "type": "payments" | "request", "data": {...} }` 
- Change `get_private_payments` and `set_private_payments` to use the envelope
- Pro: Single handshake per peer relationship
- Con: Breaking change to existing message format, more complex receive

I should probably ask the user about this design decision. But thinking through it more carefully—the user said it should write messages with PaymentRequests using the same messaging mechanism. Looking at how private payments work, `set_private_payments` sends JSON via the encrypted link and `get_private_payments` receives from it, so if requests use the same link, I need to differentiate message types somehow. Actually, there's another option: payment requests could use a separate encrypted link instance that shares the same Noise session but uses different storage paths, which is how pubky-noise works. So I could define a new path prefix for requests and derive a separate request link from the same session. specific paths using the same DH secret but a different domain
3. Create a separate `PubkyNoiseEncryptor` for requests using those paths
4. Wrap both in a combined structure or provide separate functions

But pubky-noise requires a full Noise XX handshake per encryptor... so this would indeed double the handshake messages.

Let me look at the `PubkyNoiseConfig::new_with_paths` API more carefully. The paths get set during config creation, and each config-encryptor pair operates independently with its own handshake via `handle_handshake()`.

The real question is whether I want both channels sharing a single handshake (which would need changes to pubky-noise itself), separate handshakes for each channel (simpler but more overhead), or using message type tagging to multiplex them (though that breaks the existing format or requires migration).

I should present these tradeoffs to the user as design questions. Now I'm thinking about what a library-level PaymentRequest should look like. The README shows a full subscription structure, but paykit-lib needs simpler primitives since the daemon handles subscription logic. I'm sketching out a minimal PaymentRequest type that captures just what the library needs to handle.

Now I have a complete picture. Before writing the plan, I have a key design question:

**Tool: question**

**Input:**
```json
{
  "questions": [
    {
      "question": "How should payment requests be transported relative to private payments? They both use pubky-noise encrypted links, but each PubkyNoiseEncryptor instance has its own message queue with separate paths. Currently get_private_payments drains ALL messages from the queue and keeps only the latest, so mixing payment requests into the same queue would lose messages.",
      "header": "Transport channel design",
      "options": [
        {
          "label": "Separate encrypted link (Recommended)",
          "description": "New path prefix (/pub/paykit/v0/requests) with a separate PubkyNoiseEncryptor per peer. Clean separation, no backward-compat risk. Requires a separate Noise handshake per peer for the requests channel."
        },
        {
          "label": "Same link, tagged envelope",
          "description": "Wrap all messages in {\"type\": \"payments\"|\"request\", \"data\": {...}}. Single handshake per peer but breaks existing private payments format (migration needed) and complicates receive logic."
        }
      ]
    },
    {
      "question": "What fields should the PaymentRequest type contain at the library level? The README shows a daemon-level example with subscription fields (frequency, startsAt, endsAt). At the library level, the type should be simpler — the daemon can extend it.",
      "header": "PaymentRequest fields",
      "options": [
        {
          "label": "Minimal: methods + amount + currency",
          "description": "HashMap<MethodId, EndpointData> (accepted payment methods), optional amount (String), optional currency (String). Lean primitive, daemon adds scheduling."
        },
        {
          "label": "Include memo and expiration",
          "description": "Same as minimal plus optional memo (String) and optional expires_at (u64 unix timestamp). Useful for one-off requests without the daemon."
        },
        {
          "label": "Full: include subscription fields",
          "description": "Include frequency, starts_at, ends_at in addition to the above. Makes the library self-contained for recurring requests but may be premature."
        }
      ]
    }
  ]
}
```

**Output:**
```
User has answered your questions: "How should payment requests be transported relative to private payments? They both use pubky-noise encrypted links, but each PubkyNoiseEncryptor instance has its own message queue with separate paths. Currently get_private_payments drains ALL messages from the queue and keeps only the latest, so mixing payment requests into the same queue would lose messages."="Separate encrypted link (Recommended)", "What fields should the PaymentRequest type contain at the library level? The README shows a daemon-level example with subscription fields (frequency, startsAt, endsAt). At the library level, the type should be simpler — the daemon can extend it."="Full: include subscription fields". You can now continue with the user's answers in mind.
```

---

## Assistant (Plan · Claude Opus 4.6 · 510.2s)

_Thinking:_

The user chose:
1. **Separate encrypted link** with a new path prefix for payment requests
2. **Full fields** including subscription fields (frequency, starts_at, ends_at)

Now let me think about this more carefully and write a comprehensive plan.

For the separate encrypted link approach:
- We need a new path prefix: `/pub/paykit/v0/requests`
- A new domain for path derivation: `b"paykit-requests-v0"` 
- This means the caller will need to manage TWO encrypted links per peer relationship: one for private payments and one for payment requests
- We could bundle them together in a combined type, or keep them separate


- The PaymentRequest type needs to capture the full request details: accepted payment methods, optional amount and currency, a memo field, expiration time, and subscription parameters like frequency and start/end times.

Now I'm reconsidering whether amount should be a String or numeric type. The README shows it as a float (0.001), but using floats for monetary values is risky due to precision issues. String is safer at the library level since different payment methods might represent amounts differently, and the daemon can handle parsing and validation. Looking at the README example more carefully, I notice the "supported payment list" is actually a URL reference rather than an inline list, and the timestamp fields use string representations.

At the library level though, we should embed the payment methods directly rather than referencing external URLs. So I need to draft a comprehensive plan covering the new types, path constants, derivation functions, encrypted link handling, API functions, serialization, FFI bindings, tests, and documentation. For the encrypted link mechanism, I'm considering whether to implement it as a completely separate lifecycle with its own initiation and handshake functions, or integrate it differently with the existing structure.

Looking at the options: a fully separate `RequestLink` type gives maximum flexibility but duplicates code, bundling both channels into one `EncryptedLink` simplifies the caller experience but complicates internal state, or reusing `EncryptedLink` with request-specific path derivation keeps things lean while the API functions handle the request-specific operations.

Option C feels right—it leverages the existing encrypted messaging infrastructure without introducing new types. The caller manages two `EncryptedLink` instances following the same lifecycle pattern, which is straightforward once documented clearly. There's a detail about `max_send_retries` that needs consideration though.

So the implementation would involve adding a `PaymentRequest` struct and a new path domain for requests, then creating a path derivation function and corresponding init/accept/send/receive functions that mirror the existing payment flow but handle request serialization instead. I need to decide whether request messages should all be stored or just the latest one—unlike private payments where the entire map gets replaced, each payment request is independent, so I should collect them all. Then I'm outlining the serialization functions, FFI bindings, tests, and documentation needed to complete the implementation.

Now I'm defining the PaymentRequest struct with fields for accepted payment methods, optional amount and currency, a memo, expiration timestamp, and recurrence interval support...

Defining the struct fields...

And here's what that looks like when serialized to JSON, with concrete examples of payment methods, amounts, and recurring payment dates.

Now I'm thinking through how `get_payment_requests` should work—unlike private payments where we keep only the latest, payment requests should accumulate since each message represents a distinct request. The receive function would return all unread requests as a vector. I think one message per request is the cleanest approach, so the sender calls the set function for each request they want to send.

There's a constraint I need to consider though: the 1000-byte message limit could be tight for a PaymentRequest with all the optional fields like subscription details. I'll need to validate the size. Also, I'm wondering if requests should have an ID so the payer can reference which specific request they're responding to.

Actually, I'm overthinking this. Let me keep it simple with the core fields we agreed on—methods, amount, currency, memo, and the timing fields—and let users extend it later if needed. For the link functions, keeping `initiate_request_link` and `initiate_encrypted_link` as separate functions is clearer than trying to parameterize them, since it makes the intent explicit. I still need to figure out what the restore functions would need to handle.

Looking at how the restore functions work, they take the session, secret key, remote pubkey, and snapshot, then internally compute the paths. For request links, I'd need variants that compute the right paths, but that means duplicating a lot of restore function logic just to handle different path computations.

The `_from_config` variants are actually the key insight here—they accept a pre-built `PubkyNoiseConfig` that already has the paths baked in, so they work generically for both encrypted and request links without needing separate versions.

So my approach is to create new functions for the raw-parameter restore variants that use request paths instead, but skip creating `_from_config` duplicates since those already handle both cases. Functions like `advance_handshake` and `close_encrypted_link` don't need variants at all since they're path-agnostic.

Now I'm thinking through the API details—specifically whether `get_payment_requests` should return all accumulated requests or just the latest one. Since each request is independent (unlike private payments where the latest replaces everything), I should drain and collect all queued messages rather than keeping only the most recent.

Good choices. Now I have everything needed to write a comprehensive plan. Let me also check one more thing about the receive semantics:

**Tool: question**

**Input:**
```json
{
  "questions": [
    {
      "question": "For private payments, get_private_payments drains all queued messages and keeps only the latest (because each message replaces the entire map). For payment requests, each message is a distinct request. Should get_payment_requests return ALL queued requests (collect every unread message)?",
      "header": "Request receive semantics",
      "options": [
        {
          "label": "Return all queued requests (Recommended)",
          "description": "Drain the queue and return Vec<PaymentRequest> with every unread message. Each request is independent and should not be discarded."
        },
        {
          "label": "Return only the latest request",
          "description": "Same drain-and-keep-latest as private payments. Simpler but means intermediate requests are lost."
        }
      ]
    }
  ]
}
```

**Output:**
```
User has answered your questions: "For private payments, get_private_payments drains all queued messages and keeps only the latest (because each message replaces the entire map). For payment requests, each message is a distinct request. Should get_payment_requests return ALL queued requests (collect every unread message)?"="Return all queued requests (Recommended)". You can now continue with the user's answers in mind.
```

---

## Assistant (Plan · Claude Opus 4.6 · 54.8s)

_Thinking:_

Now I have all the design decisions I need. Let me write the complete plan.

Summary of decisions:
1. Separate encrypted link with new path prefix for payment requests
2. Full fields: methods, amount, currency, memo, expires_at, frequency, starts_at, ends_at
3. Return all queued requests (Vec<PaymentRequest>)

Let me now write the comprehensive implementation plan.

Now I have all the design decisions. Here's the complete implementation plan:

---

## Payment Request Feature — Implementation Plan

### Design Decisions (Agreed)

- **Transport**: Separate encrypted link with its own path prefix (`/pub/paykit/v0/requests`)
- **Fields**: Full — including subscription fields (frequency, starts_at, ends_at)
- **Receive semantics**: Return all queued requests (`Vec<PaymentRequest>`), don't discard intermediates

---

### 1. New types in `paykit-lib/src/lib.rs`

**`PaymentRequest`** struct:

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct PaymentRequest {
    /// Payment methods the requester will accept, with their endpoint data.
    pub methods: HashMap<MethodId, EndpointData>,
    /// Requested payment amount (string to stay format-agnostic across currencies).
    pub amount: Option<String>,
    /// Currency/denomination (e.g., "BTC", "USD", "sats").
    pub currency: Option<String>,
    /// Human-readable memo or description.
    pub memo: Option<String>,
    /// Unix timestamp after which this request expires.
    pub expires_at: Option<u64>,
    /// Recurrence interval for subscription requests (e.g., "1d", "1w", "1m").
    pub frequency: Option<String>,
    /// Unix timestamp when the recurring request begins.
    pub starts_at: Option<u64>,
    /// Unix timestamp when the recurring request ends.
    pub ends_at: Option<u64>,
}
```

Derives: `Clone, Debug, PartialEq, Eq`. The `methods` field reuses existing `MethodId` and `EndpointData` so we get path-safety and validation for free.

---

### 2. New constants

In `transport/pubky/mod.rs`:
```rust
pub const PAYKIT_REQUESTS_PATH_PREFIX: &str = "/pub/paykit/v0/requests";
```

In `lib.rs` (internal):
```rust
const PAYKIT_REQUESTS_PATH_DOMAIN: &[u8] = b"paykit-requests-v0";
```

A distinct domain ensures the DH-derived paths for requests never collide with private payment paths, even for the same peer pair.

---

### 3. Path derivation

New internal function (mirrors `compute_private_payment_paths`):

```rust
fn compute_request_paths(
    local_secret_key: &[u8; 32],
    remote_pubkey: &PublicKey,
) -> (String, String) {
    pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_pubkey,
        PAYKIT_REQUESTS_PATH_DOMAIN,
        transport::pubky::PAYKIT_REQUESTS_PATH_PREFIX,
    )
}
```

---

### 4. Encrypted link lifecycle for requests

Four new public functions that mirror the private-payment link functions but use request paths. The handshake/advance/close/restore-from-config functions are **reused unchanged** since they're path-agnostic.

| New function | Mirrors | Difference |
|---|---|---|
| `initiate_request_link` | `initiate_encrypted_link` | Uses `compute_request_paths` |
| `accept_request_link` | `accept_encrypted_link` | Uses `compute_request_paths` |
| `restore_request_link` | `restore_encrypted_link` | Uses `compute_request_paths` |
| `restore_request_link_handshake` | `restore_encrypted_link_handshake` | Uses `compute_request_paths` |

**Unchanged/reused** (work for both payment and request links):
- `advance_handshake` — path-agnostic polling
- `close_encrypted_link` — just calls `encryptor.close()`
- `restore_encrypted_link_from_config` — config already has paths baked in
- `restore_encrypted_link_handshake_from_config` — same

To reduce duplication, the init/accept/restore functions should delegate to shared `_inner` helpers parameterized by a path computation function (or pass the computed paths directly).

---

### 5. Request API functions

Two new `#[cfg(feature = "pubky")]` public functions:

**`set_payment_request`** — sends a single payment request over the request link:
```rust
pub async fn set_payment_request(
    link: &mut EncryptedLink,
    request: &PaymentRequest,
) -> Result<()>
```

Behavior:
1. Serialize `PaymentRequest` to JSON via `serialize_payment_request_json`
2. Validate payload size against `PUBKY_NOISE_MSG_LEN` (1000 bytes)
3. Send via `link.encryptor.send_message()` with retry logic (same as `set_private_payments`)

**`get_payment_requests`** — receives all queued payment requests:
```rust
pub async fn get_payment_requests(
    link: &mut EncryptedLink,
) -> Result<Vec<PaymentRequest>>
```

Behavior:
1. Loop calling `link.encryptor.receive_message()` until queue is empty
2. **Collect all** messages (unlike `get_private_payments` which keeps only the latest)
3. Trim zero-padding, parse UTF-8, deserialize each as `PaymentRequest`
4. Return `Vec<PaymentRequest>` (empty vec if no messages)

---

### 6. Serialization / deserialization

Two new internal helper functions:

**JSON format:**
```json
{
  "methods": {
    "bitcoin-bolt11": "lnbc...",
    "bitcoin-p2tr": "bc1p..."
  },
  "amount": "0.001",
  "currency": "BTC",
  "memo": "Monthly hosting",
  "expires_at": 1736445571,
  "frequency": "1m",
  "starts_at": 1736415571,
  "ends_at": 1767951571
}
```

- `serialize_payment_request_json(request: &PaymentRequest) -> Result<String>` — builds a `serde_json::Value` map, omitting `None` fields to save space within the 1000-byte limit
- `parse_payment_request_json(json: &str) -> Result<PaymentRequest>` — parses JSON, validates `MethodId`s in the `methods` map (using `MethodId::new` to catch invalid keys from the network, returning `PaykitError::InvalidData`), optional fields default to `None`

Since this uses `serde_json`, it's gated behind `#[cfg(feature = "pubky")]` like the existing serialization helpers.

---

### 7. Validation

`PaymentRequest` needs a builder or validation helper to catch bad input early:
- `methods` must not be empty (you can't request payment without specifying at least one method)
- `amount` if present must be non-empty
- `currency` if present must be non-empty
- `starts_at` must be <= `ends_at` when both are present
- `frequency` is required if `starts_at` or `ends_at` are present (it's a subscription)

This could be a `PaymentRequest::validate(&self) -> Result<()>` method, called by `set_payment_request` before serialization. Errors use `PaykitError::Validation`.

---

### 8. MethodId reserved word

Currently `"private"` is reserved in `MethodId::new()`. The new path prefix `requests` doesn't conflict with `MethodId` since it's a directory-level prefix, not a method identifier. No changes needed to `MethodId` validation.

---

### 9. FFI bindings (`paykit-ffi/src/lib.rs`)

New types:
```rust
#[derive(uniffi::Record)]
pub struct FfiPaymentRequest {
    pub methods: Vec<FfiPaymentEntry>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub memo: Option<String>,
    pub expires_at: Option<u64>,
    pub frequency: Option<String>,
    pub starts_at: Option<u64>,
    pub ends_at: Option<u64>,
}
```

New exported functions (pattern follows existing FFI functions):
- `paykit_send_payment_request(...)` — wraps `set_payment_request`
- `paykit_receive_payment_requests(...)` — wraps `get_payment_requests`
- Request-link lifecycle functions (init, accept, restore)

The encrypted link state management for request links will follow the same pattern as for payment links — the FFI layer manages the `EncryptedLink` lifecycle.

Note: The FFI layer currently doesn't expose encrypted link management at all (the existing FFI only has public payment CRUD). Adding encrypted link FFI support may be a prerequisite or parallel effort. This plan scopes the **library-level** implementation; FFI bindings for the full encrypted link lifecycle (both payment and request channels) could be a follow-up.

---

### 10. Tests

**Unit tests** (no network):
- `test_payment_request_serialization_roundtrip` — serialize/deserialize with all fields
- `test_payment_request_serialization_optional_fields` — serialize/deserialize with `None` optionals
- `test_payment_request_validation_empty_methods` — rejects empty methods map
- `test_payment_request_validation_starts_after_ends` — rejects `starts_at > ends_at`
- `test_payment_request_validation_frequency_required` — rejects missing frequency when start/end set
- `test_payment_request_json_invalid_method_id` — `InvalidData` on bad method key from network
- `test_payment_request_size_limit` — payload exceeding 1000 bytes returns `Validation` error

**Integration tests** (require `pubky` feature + ephemeral testnet):
- `test_send_receive_payment_request` — basic roundtrip between two peers
- `test_receive_multiple_payment_requests` — verify all queued requests are returned (not just latest)
- `test_send_payment_request_empty_queue` — receiving with no messages returns empty vec
- `test_request_link_separate_from_payment_link` — verify request and payment channels don't interfere

---

### 11. Documentation

- `///` doc comments on all new public types and functions
- Doc examples (embedded as `cargo test`-runnable) on `set_payment_request` and `get_payment_requests`
- Update `README.md` to reference the new library-level payment request support

---

### 12. File change summary

| File | Changes |
|---|---|
| `paykit-lib/src/lib.rs` | `PaymentRequest` type, validation, serialization, `compute_request_paths`, 4 link lifecycle functions, `set_payment_request`, `get_payment_requests`, unit + integration tests |
| `paykit-lib/src/transport/pubky/mod.rs` | Add `PAYKIT_REQUESTS_PATH_PREFIX` constant |
| `paykit-ffi/src/lib.rs` | `FfiPaymentRequest` type, FFI wrapper functions (scoped to after encrypted link FFI exists) |

---

### 13. Implementation order

1. Add path constant and domain constant
2. Add `PaymentRequest` type with validation
3. Add serialization/deserialization helpers
4. Add `compute_request_paths`
5. Add link lifecycle functions (`initiate_request_link`, `accept_request_link`, restore variants)
6. Add `set_payment_request` and `get_payment_requests`
7. Write unit tests
8. Write integration tests
9. Add doc comments and examples
10. FFI bindings (if encrypted link FFI exists or as follow-up)
11. `cargo fmt && cargo clippy --all-targets --all-features && cargo test`

---

Shall I proceed with implementation?

---

