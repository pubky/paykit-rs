//! `switch-enroll`: enrols test identities on a running Pubky Switch service and approves
//! their apps with the real signer, for the browser rig (`rig/switch-rig.mjs`, which runs it
//! once per `POST /rig/world`).
//!
//! **Test rig code only.** Its keys come from a seed prefix printed in its output, and it
//! reaches the service over plain HTTP on loopback. It exists because the signer
//! (`pubky-switch-signer`) does not build for wasm32: this native program does the signer's
//! part, and the browser test joins the approved apps itself. On wasm32 it is an empty `main`.
//!
//! # Command line
//!
//! ```text
//! switch-enroll --switch-url http://127.0.0.1:<port> --origin https://<host>[:<port>]
//!     --seed-prefix <32 lowercase hex digits> --identity <name>...
//!     [--app <identity>:<app>]...
//! ```
//!
//! - `--switch-url`: the service's plain HTTP listener, `http://127.0.0.1:<port>` or
//!   `http://localhost:<port>`; nothing else is accepted.
//! - `--origin`: the service's https origin (Switch wire 3.3), which the requests are signed
//!   for and every `identity.json` names. Only the connection goes to `--switch-url`.
//! - `--seed-prefix`: exactly 32 lowercase hex digits. A fixed length keeps two prefixes from
//!   giving the same seed: `BLAKE3(prefix + label)` with prefixes of different lengths could
//!   collide (`ab` + `cpayer/...` and `abc` + `payer/...`).
//! - `--identity`: an identity to enrol, once each, at least one.
//! - `--app`: an app of a declared identity to approve, once each. Each app is approved right
//!   after its identity is enrolled, in the order given. Every app asks for the scopes of
//!   `paykit_sdk::AppScopes::new().pay().request()`, so it can act as payer and as payee.
//!
//! Names are 1 to 32 characters: a lowercase ASCII letter, then lowercase letters, digits and
//! `-`. On success it prints one JSON object on stdout, with a newline, and exits 0. On any
//! failure it prints one line to stderr and exits non-zero: 2 for a bad command line, 1
//! otherwise. Progress goes to stderr. No secret key is printed.
//!
//! # Output
//!
//! ```text
//! {
//!   "seed_prefix": "<the --seed-prefix>",
//!   "origin": "<the --origin>",
//!   "identities": {
//!     "<name>": {
//!       "id": "<the identity's Pubky ID, as IdentityKey::id spells it>",
//!       "identity_json": "<base64url of the identity's latest identity.json bytes>"
//!     }
//!   },
//!   "apps": {
//!     "<identity>:<app>": {
//!       "identity": "<identity name>",
//!       "app": "<app name>",
//!       "app_id": "<the App ID, as AppKeys::app_id spells it>",
//!       "scopes": ["<scope>", ...],
//!       "request": "<base64url of the signed app request's JSON, a SignedDoc>",
//!       "approval": "<base64url of the approval's JSON bytes>"
//!     }
//!   }
//! }
//! ```
//!
//! Every byte value is base64url without padding, the Switch's own encoding (Switch wire 1.2):
//! `pubky_switch_core::enc::b64` writes it and `pubky_switch_core::enc::unb64` reads it back,
//! natively and on wasm32 alike. Map keys come out sorted. Each `identity_json` is the file the identity's signer published after
//! the last approval: it lists every approved app of that identity and names the service key
//! (`inbox.service_key`) the apps pin.
//!
//! # What the browser side rebuilds
//!
//! The keys come from `pubky_switch_client::testing` with `Seeds::new(seed_prefix)`:
//!
//! - identity `<name>`: `TestIdentity::new(&seeds, "<name>")`, whose key is the seed
//!   `<name>/identity`;
//! - app `<app>` of it: `identity.app_keys("<app>")`, the seeds `<name>/<app>/signing` and
//!   `<name>/<app>/bundle`.
//!
//! The app request is signed over `AppRequestBody::new(&keys, scopes, &auth)`, where `auth` is
//! the `AuthRequest` the scripted signer (`TestSigner::approve`) uses: `client_id`
//! `"app.example"`, `client_key` `[7; 32]`, `caps` `"<app_record_path(&keys.app_id_bytes())>:rw"`
//! and `secret` `[8; 32]`. The browser needs none of it to join: it takes the request and the
//! approval from the output. To join app `<identity>:<app>`:
//!
//! 1. put each identity's `identity_json` bytes on the in-memory Homeserver the apps read
//!    (`TestHomeserver::put(identity.public(), IDENTITY_PATH, bytes)`);
//! 2. build `JoinRequest { identity: identity.public(), keys: identity.app_keys("<app>"),
//!    request, approval }`, with `request` = `AppRequest::verify` of the `SignedDoc` that
//!    `request` decodes to, and `approval` = the decoded `approval` bytes;
//! 3. call `Paykit::join` with an environment whose transport reaches `origin`.
//!
//! Each run approves new apps on the service, and an app joins once: every run needs a fresh
//! seed prefix.

#![forbid(unsafe_code)]

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    native::main()
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use paykit_lib::signer::PaykitProtocol;
    use paykit_sdk::AppScopes;
    use pubky_switch_client::app::HomeserverReader;
    use pubky_switch_client::clock::{Clock, SystemClock};
    use pubky_switch_client::store::{MemoryStore, StoreHandle};
    use pubky_switch_client::testing::{Seeds, TestHomeserver, TestIdentity};
    use pubky_switch_client::transport::{
        BoxFuture, FirstSend, HttpConfig, HttpTransport, Request, Response, Transport,
    };
    use pubky_switch_client::TransportError;
    use pubky_switch_core::enc::b64;
    use pubky_switch_core::keys::AppKeys;
    use pubky_switch_core::records::{
        app_record_path, is_origin, AppRequest, AppRequestBody, ApprovalClaim, AuthRequest,
    };
    use pubky_switch_signer::{
        AddOutcome, Homeserver, IdentityKeyring, IdentitySecret, Protocols, SecretVault, Signer,
        SignerEnv, SignerStatus, VaultError,
    };
    use serde::Serialize;

    const USAGE: &str = "usage: switch-enroll --switch-url http://127.0.0.1:<port> \
        --origin https://<host>[:<port>] --seed-prefix <32 lowercase hex digits> \
        --identity <name>... [--app <identity>:<app>]...";

    /// The length of a seed prefix, in hex digits.
    const SEED_PREFIX_LEN: usize = 32;

    /// The longest identity or app name.
    const MAX_NAME_LEN: usize = 32;

    /// Why the program stops.
    enum Failure {
        /// The command line is wrong.
        Usage(String),
        /// Enrolment or an approval failed.
        Run(String),
    }

    fn usage(message: impl Into<String>) -> Failure {
        Failure::Usage(message.into())
    }

    /// A failure of step `what`, with the error that caused it.
    fn failed<E: std::fmt::Display>(what: impl std::fmt::Display) -> impl FnOnce(E) -> Failure {
        move |error| Failure::Run(format!("{what}: {error}"))
    }

    pub(crate) fn main() -> ExitCode {
        let printed = run(std::env::args().skip(1)).and_then(|output| {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{output}")
                .and_then(|()| stdout.flush())
                .map_err(failed("could not write the output"))
        });
        match printed {
            Ok(()) => ExitCode::SUCCESS,
            Err(Failure::Usage(message)) => {
                eprintln!("switch-enroll: {message}\n{USAGE}");
                ExitCode::from(2)
            }
            Err(Failure::Run(message)) => {
                eprintln!("switch-enroll: {message}");
                ExitCode::FAILURE
            }
        }
    }

    /// Parse the command line, enrol and approve, and return the output JSON.
    fn run(args: impl Iterator<Item = String>) -> Result<String, Failure> {
        let args = Args::parse(args)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(failed("could not start the runtime"))?;
        let world = runtime.block_on(Box::pin(enrol(&args)))?;
        serde_json::to_string(&world).map_err(failed("could not write the output JSON"))
    }

    /// The command line, checked.
    #[derive(Debug)]
    struct Args {
        switch_url: String,
        origin: String,
        seed_prefix: String,
        identities: Vec<String>,
        apps: Vec<AppArg>,
    }

    /// One `--app`.
    #[derive(Debug)]
    struct AppArg {
        identity: String,
        app: String,
    }

    impl Args {
        fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, Failure> {
            let mut switch_url = None;
            let mut origin = None;
            let mut seed_prefix = None;
            let mut identities: Vec<String> = Vec::new();
            let mut app_args: Vec<String> = Vec::new();
            while let Some(flag) = args.next() {
                let mut value = || {
                    args.next()
                        .ok_or_else(|| usage(format!("{flag} needs a value")))
                };
                match flag.as_str() {
                    "--switch-url" => set_once(&mut switch_url, &flag, value()?)?,
                    "--origin" => set_once(&mut origin, &flag, value()?)?,
                    "--seed-prefix" => set_once(&mut seed_prefix, &flag, value()?)?,
                    "--identity" => identities.push(value()?),
                    "--app" => app_args.push(value()?),
                    other => return Err(usage(format!("unknown argument {other:?}"))),
                }
            }
            let switch_url = switch_url.ok_or_else(|| usage("--switch-url is missing"))?;
            check_switch_url(&switch_url)?;
            let origin = origin.ok_or_else(|| usage("--origin is missing"))?;
            if !is_origin(&origin) {
                return Err(usage(format!(
                    "--origin {origin:?} is not an https origin without a path (Switch wire 3.3)"
                )));
            }
            let seed_prefix = seed_prefix.ok_or_else(|| usage("--seed-prefix is missing"))?;
            check_seed_prefix(&seed_prefix)?;
            if identities.is_empty() {
                return Err(usage("at least one --identity is needed"));
            }
            for (at, name) in identities.iter().enumerate() {
                check_name("--identity", name)?;
                if identities[..at].contains(name) {
                    return Err(usage(format!("--identity {name} is given twice")));
                }
            }
            let mut apps: Vec<AppArg> = Vec::new();
            for text in &app_args {
                let app = AppArg::parse(text)?;
                if !identities.contains(&app.identity) {
                    return Err(usage(format!(
                        "--app {text}: {} is no --identity",
                        app.identity
                    )));
                }
                if apps
                    .iter()
                    .any(|seen| seen.identity == app.identity && seen.app == app.app)
                {
                    return Err(usage(format!(
                        "--app {}:{} is given twice",
                        app.identity, app.app
                    )));
                }
                apps.push(app);
            }
            Ok(Self {
                switch_url,
                origin,
                seed_prefix,
                identities,
                apps,
            })
        }
    }

    impl AppArg {
        /// `<identity>:<app>`. Names hold no `:`, so a third part fails the app name's check.
        fn parse(text: &str) -> Result<Self, Failure> {
            let (identity, app) = text
                .split_once(':')
                .ok_or_else(|| usage(format!("--app {text}: need <identity>:<app>")))?;
            check_name("--app identity", identity)?;
            check_name("--app app", app)?;
            Ok(Self {
                identity: identity.to_owned(),
                app: app.to_owned(),
            })
        }
    }

    fn set_once(slot: &mut Option<String>, flag: &str, value: String) -> Result<(), Failure> {
        if slot.replace(value).is_some() {
            return Err(usage(format!("{flag} is given twice")));
        }
        Ok(())
    }

    /// Only the service's plain listener on loopback: `http://127.0.0.1:<port>` or
    /// `http://localhost:<port>`, the port without leading zeros.
    fn check_switch_url(url: &str) -> Result<(), Failure> {
        let refused = || {
            usage(format!(
                "--switch-url {url:?} is not http://127.0.0.1:<port> or http://localhost:<port>"
            ))
        };
        let rest = url.strip_prefix("http://").ok_or_else(refused)?;
        let (host, port) = rest.split_once(':').ok_or_else(refused)?;
        let port_ok = !port.starts_with('0')
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok();
        if (host == "127.0.0.1" || host == "localhost") && port_ok {
            Ok(())
        } else {
            Err(refused())
        }
    }

    fn check_seed_prefix(prefix: &str) -> Result<(), Failure> {
        let hex = prefix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if prefix.len() == SEED_PREFIX_LEN && hex {
            Ok(())
        } else {
            Err(usage(format!(
                "--seed-prefix must be exactly {SEED_PREFIX_LEN} lowercase hex digits"
            )))
        }
    }

    /// A lowercase ASCII letter, then lowercase letters, digits and `-`, at most 32 in all.
    /// No `/` or `:`, so a name never runs into the seed label or `--app` syntax around it.
    fn check_name(what: &str, name: &str) -> Result<(), Failure> {
        let mut bytes = name.bytes();
        let first_ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
        let rest_ok = bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if first_ok && rest_ok && name.len() <= MAX_NAME_LEN {
            Ok(())
        } else {
            Err(usage(format!(
                "{what} {name:?} is not a lowercase letter followed by up to 31 lowercase \
                 letters, digits or '-'"
            )))
        }
    }

    /// The output (see the file's docs, "Output").
    #[derive(Debug, Serialize)]
    struct World {
        seed_prefix: String,
        origin: String,
        identities: BTreeMap<String, IdentityOutput>,
        apps: BTreeMap<String, AppOutput>,
    }

    /// One identity of the output.
    #[derive(Debug, Serialize)]
    struct IdentityOutput {
        id: String,
        identity_json: String,
    }

    /// One app of the output.
    #[derive(Debug, Serialize)]
    struct AppOutput {
        identity: String,
        app: String,
        app_id: String,
        scopes: Vec<String>,
        request: String,
        approval: String,
    }

    /// Enrol each identity and approve its apps in order, then read each identity's latest
    /// `identity.json`.
    async fn enrol(args: &Args) -> Result<World, Failure> {
        // Payer and payee, with the scopes they require, checked as the signer checks an app
        // request (Paykit wire 3.1).
        let scopes = AppScopes::new()
            .pay()
            .request()
            .build()
            .map(|built| built.scopes)
            .map_err(failed("could not build the pay and request scopes"))?;
        let inner = HttpTransport::without_address_checks_for_loopback_tests(HttpConfig::default())
            .map_err(failed("could not build the HTTP transport"))?;
        let transport: Arc<dyn Transport> = Arc::new(LoopbackListener {
            inner,
            origin: args.origin.clone(),
            listener: args.switch_url.clone(),
        });
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        // The identities' Homeservers, for this run only: the browser side gets the files it
        // needs from the output.
        let files = Arc::new(TestHomeserver::new());
        let seeds = Seeds::new(&args.seed_prefix);

        let mut parties: BTreeMap<&str, (TestIdentity, Signer)> = BTreeMap::new();
        let mut apps = BTreeMap::new();
        for name in &args.identities {
            let identity = TestIdentity::new(&seeds, name);
            eprintln!("enrolling {name} ({})", identity.key().id());
            let env = SignerEnv {
                store: StoreHandle::inline(Arc::new(MemoryStore::new())),
                clock: Arc::clone(&clock),
                transport: Arc::clone(&transport),
                homeserver: Arc::new(OwnHomeserver {
                    identity: identity.public(),
                    files: Arc::clone(&files),
                }),
                protocols: Protocols::new(vec![Arc::new(PaykitProtocol)])
                    .map_err(failed("could not register Paykit's protocol"))?,
            };
            let mut signer = Box::pin(enrol_identity(env, &identity, &args.origin))
                .await
                .map_err(failed(format!("enrolling {name}")))?;
            for app in args.apps.iter().filter(|app| app.identity == *name) {
                let label = format!("{}:{}", app.identity, app.app);
                eprintln!("approving {label} for {}", scopes.join(","));
                let output = Box::pin(approve(&identity, &mut signer, app, &scopes))
                    .await
                    .map_err(failed(format!("approving {label}")))?;
                apps.insert(label, output);
            }
            parties.insert(name, (identity, signer));
        }

        let mut identities = BTreeMap::new();
        for (name, (identity, _)) in &parties {
            let public = identity.public();
            let doc = files.identity_doc(&public).ok_or_else(|| {
                Failure::Run(format!(
                    "{name}: no identity.json that verifies was published"
                ))
            })?;
            for app in args.apps.iter().filter(|app| app.identity == *name) {
                let app_id = identity.app_keys(&app.app).app_id_bytes();
                if !doc.body().apps.iter().any(|listed| listed.app_id == app_id) {
                    return Err(Failure::Run(format!(
                        "{name}: the latest identity.json does not list {}",
                        app.app
                    )));
                }
            }
            let bytes = files
                .file(public, pubky_switch_core::records::IDENTITY_PATH)
                .ok_or_else(|| Failure::Run(format!("{name}: identity.json is gone")))?;
            identities.insert(
                (*name).to_owned(),
                IdentityOutput {
                    id: identity.key().id(),
                    identity_json: b64(&bytes),
                },
            );
        }

        Ok(World {
            seed_prefix: args.seed_prefix.clone(),
            origin: args.origin.clone(),
            identities,
            apps,
        })
    }

    /// Set up `identity`'s signer for `origin` and enrol it: the service creates the account
    /// and takes the first `identity.json`, which the signer then publishes.
    async fn enrol_identity(
        env: SignerEnv,
        identity: &TestIdentity,
        origin: &str,
    ) -> Result<Signer, Box<dyn std::error::Error>> {
        let vault = MemoryVault(Mutex::new(Some(IdentitySecret::from_bytes(
            identity.key().secret_bytes(),
        ))));
        let keyring = IdentityKeyring::open(Arc::new(vault))?;
        let mut signer = Box::pin(Signer::create(env, keyring, origin)).await?;
        Box::pin(signer.resume()).await?;
        match signer.status() {
            SignerStatus::Signing => Ok(signer),
            other => Err(format!("the signer stopped at {other:?}").into()),
        }
    }

    /// The auth request an app's request is bound to: the one the scripted signer uses
    /// (`TestSigner::approve`), asking for the app's own record path only. Paykit adds no
    /// grant paths, so the signer finds these capabilities exact (Switch wire 3.9 step 3).
    fn auth_request(keys: &AppKeys) -> AuthRequest {
        AuthRequest {
            client_id: "app.example".into(),
            client_key: [7; 32],
            caps: format!("{}:rw", app_record_path(&keys.app_id_bytes())),
            secret: [8; 32],
        }
    }

    /// Sign app `app`'s request for `scopes`, have the signer check and approve it (Switch
    /// wire 3.9), and check the companion claim as a host does (Switch wire 3.10).
    async fn approve(
        identity: &TestIdentity,
        signer: &mut Signer,
        app: &AppArg,
        scopes: &[String],
    ) -> Result<AppOutput, Box<dyn std::error::Error>> {
        let keys = identity.app_keys(&app.app);
        let auth = auth_request(&keys);
        let request = AppRequest::sign(&keys, AppRequestBody::new(&keys, scopes.to_vec(), &auth))?;
        let checked = signer.review(request.signed(), &auth)?;
        let approved = Box::pin(signer.approve(checked)).await?;
        Box::pin(signer.claim_delivered(&approved.request)).await?;
        let claim = ApprovalClaim::from_plaintext(&approved.claim)?;
        let approval = claim.verify(&identity.public(), &auth)?;
        let request_json = serde_json::to_vec(request.signed())?;
        Ok(AppOutput {
            identity: app.identity.clone(),
            app: app.app.clone(),
            app_id: keys.app_id(),
            scopes: scopes.to_vec(),
            request: b64(&request_json),
            approval: b64(approval),
        })
    }

    /// Test rig code only: a transport that sends each request signed for the rig's https
    /// origin to the service's plain HTTP listener on loopback instead. The origin is never
    /// sent, so the signatures still cover the https origin, which the service checks them
    /// against. A request for any other origin is refused before anything is sent.
    struct LoopbackListener {
        /// The client's transport for test runs on loopback, which checks no origin or
        /// address and allows plain HTTP.
        inner: HttpTransport,
        /// The origin the requests are signed for.
        origin: String,
        /// Where they go instead.
        listener: String,
    }

    impl LoopbackListener {
        fn redirect(&self, request: Request) -> Result<Request, TransportError> {
            if request.origin != self.origin {
                return Err(TransportError::Refused(format!(
                    "{} is not the rig's origin",
                    request.origin
                )));
            }
            Ok(Request {
                origin: self.listener.clone(),
                ..request
            })
        }
    }

    impl Transport for LoopbackListener {
        fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
            match self.redirect(request) {
                Ok(request) => self.inner.send(request),
                Err(error) => Box::pin(std::future::ready(Err(error))),
            }
        }

        fn send_before<'a>(
            &'a self,
            request: Request,
            first: &'a FirstSend,
        ) -> BoxFuture<'a, Result<Response, TransportError>> {
            match self.redirect(request) {
                Ok(request) => self.inner.send_before(request, first),
                Err(error) => Box::pin(std::future::ready(Err(error))),
            }
        }
    }

    /// One identity's Homeserver, as its signer reads and writes it, on the run's in-memory
    /// Homeservers.
    struct OwnHomeserver {
        identity: [u8; 32],
        files: Arc<TestHomeserver>,
    }

    impl Homeserver for OwnHomeserver {
        fn get(&self, path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
            HomeserverReader::get(&*self.files, self.identity, path.to_owned())
        }

        fn put(&self, path: &str, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TransportError>> {
            self.files.put(self.identity, path, bytes);
            Box::pin(std::future::ready(Ok(())))
        }

        fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), TransportError>> {
            self.files.remove(self.identity, path);
            Box::pin(std::future::ready(Ok(())))
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The identity secret, in memory for the run.
    struct MemoryVault(Mutex<Option<IdentitySecret>>);

    impl SecretVault for MemoryVault {
        fn load(&self) -> Result<Option<IdentitySecret>, VaultError> {
            Ok(lock(&self.0).clone())
        }

        fn add(&self, secret: &IdentitySecret) -> Result<AddOutcome, VaultError> {
            let mut held = lock(&self.0);
            if held.is_some() {
                return Ok(AddOutcome::Occupied);
            }
            *held = Some(secret.clone());
            Ok(AddOutcome::Stored)
        }

        fn delete(&self) -> Result<(), VaultError> {
            *lock(&self.0) = None;
            Ok(())
        }
    }
}
