use crate::flights::{AppId, Flights, Wake};
use crate::{fixed, runtime, WalletError, WalletHost};
use paykit_sdk::Stage;
use paykit_sdk::{AppEnv, Paykit, PaykitError};
use pubky_switch_client::store::StoreHandle;
use pubky_switch_client::transport::BoxFuture;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Mutex};

/// One wallet command: it runs on the wallet's worker with the SDK value to itself and sends
/// its own answer.
type Job = Box<dyn for<'a> FnOnce(&'a mut Paykit) -> BoxFuture<'a, ()> + Send>;

enum Command {
    Run(Job),
    /// Resume the payments, to record the late answers kept for this wallet.
    Settle,
    Shutdown(oneshot::Sender<Result<(), WalletError>>),
    /// Hand the SDK value to a test and wait for it to come back.
    #[cfg(test)]
    Lend(oneshot::Sender<Paykit>, oneshot::Receiver<Paykit>),
}

/// One open, exclusively owned Paykit wallet.
///
/// The wallet's worker, a task on the bindings' runtime, owns the SDK value and runs one
/// call at a time in the order the calls arrive. A host thread only waits for the answer.
/// A host that stops waiting (a cancelled coroutine, a dropped future) stops only its wait:
/// the call runs to its end, and its effect is in the store for the next read.
#[derive(uniffi::Object)]
pub struct NativeWallet {
    commands: mpsc::UnboundedSender<Command>,
    rail: Arc<crate::HostRail>,
    pub(crate) pace: Arc<Mutex<pubky_switch_client::app::WaitPace>>,
}

impl NativeWallet {
    pub(crate) fn opened(
        app: Paykit,
        rail: Arc<crate::HostRail>,
        ownership: Option<Arc<crate::LeaseOwner>>,
    ) -> Result<Arc<Self>, WalletError> {
        let (commands, queue) = mpsc::unbounded_channel();
        let flights = rail.flights().clone();
        runtime::spawn(work(app, flights, commands.downgrade(), queue, ownership))?;
        Ok(Arc::new(Self {
            commands,
            rail,
            pace: Arc::default(),
        }))
    }

    /// The rail of the host this wallet was opened with.
    pub(crate) fn rail(&self) -> Arc<crate::HostRail> {
        self.rail.clone()
    }

    /// Run `call` on the wallet's worker and wait for its answer. The call runs to its end
    /// whether or not the caller still waits.
    pub(crate) async fn with_wallet<T, F>(&self, call: F) -> Result<T, WalletError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Paykit) -> BoxFuture<'a, Result<T, WalletError>> + Send + 'static,
    {
        runtime::refuse_reentry()?;
        let (answer, answered) = oneshot::channel();
        let job: Job = Box::new(move |app| {
            Box::pin(async move {
                let _ = answer.send(call(app).await);
            })
        });
        self.commands
            .send(Command::Run(job))
            .map_err(|_| WalletError::Closed)?;
        answered.await.map_err(|_| WalletError::Closed)?
    }

    /// Lend the SDK value to a test that needs it with borrowed state of its own.
    #[cfg(test)]
    pub(crate) async fn lend<T>(
        &self,
        call: impl AsyncFnOnce(&mut Paykit) -> T,
    ) -> Result<T, WalletError> {
        let (lent, received) = oneshot::channel();
        let (give_back, back) = oneshot::channel();
        self.commands
            .send(Command::Lend(lent, back))
            .map_err(|_| WalletError::Closed)?;
        let mut app = received.await.map_err(|_| WalletError::Closed)?;
        let result = call(&mut app).await;
        give_back.send(app).map_err(|_| WalletError::Closed)?;
        Ok(result)
    }
}

tokio::task_local! {
    /// The App ID of the wallet whose worker runs the current command.
    static CURRENT: AppId;
}

/// Inside a wallet command: the App ID the command's payments belong to.
pub(crate) fn current_app() -> Option<AppId> {
    CURRENT.try_with(|app| *app).ok()
}

/// The first wait before a settle that left a late answer unrecorded runs again, and the
/// longest the wait grows to.
const SETTLE_RETRY: Duration = Duration::from_secs(2);
const SETTLE_RETRY_CAP: Duration = Duration::from_secs(60);

/// Resume this wallet's payments after a host `pay` answered late: the resume takes each
/// answer kept for this wallet through the method's lookup and asks the host about the rest.
/// A kept answer is let go only once the journal no longer shows its attempt paying; a resume
/// that stops early, on a lookup or a store write, leaves it kept and is retried, with or
/// without kept answers. Returns whether the settle is done.
async fn settle(app: &mut Paykit, flights: &Flights, id: &AppId) -> bool {
    let kept = flights.late_of(id);
    if app.resume_payments().await.is_err() {
        return false;
    }
    if kept.is_empty() {
        return flights.late_of(id).is_empty();
    }
    let Ok(journal) = app.payments().await else {
        return false;
    };
    for attempt in kept {
        let paying = journal
            .iter()
            .any(|entry| entry.attempt_id.to_text() == attempt && entry.stage == Stage::Paying);
        if !paying {
            flights.recorded(&attempt);
        }
    }
    flights.late_of(id).is_empty()
}

/// The wallet's worker. It stops after a shutdown, or once every handle is gone and the
/// calls already sent have run; then it drops the SDK value and releases the store. While it
/// runs, a late answer from the host's `pay` for its App ID wakes it to settle, at most one
/// settle queued at a time, and an unrecorded answer is retried with a growing wait.
async fn work(
    mut app: Paykit,
    flights: Flights,
    own: mpsc::WeakUnboundedSender<Command>,
    mut queue: mpsc::UnboundedReceiver<Command>,
    ownership: Option<Arc<crate::LeaseOwner>>,
) {
    let id: AppId = app.app_id();
    let queued = Arc::new(AtomicBool::new(false));
    let wake: Wake = {
        let queued = queued.clone();
        Arc::new(move || {
            if queued.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Some(commands) = own.upgrade() {
                let _ = commands.send(Command::Settle);
            }
        })
    };
    flights.open(id, wake.clone());
    // Answers that came while no wallet for this App ID was open are settled before the first
    // host call runs.
    let mut retry = None;
    if !flights.late_of(&id).is_empty() && !CURRENT.scope(id, settle(&mut app, &flights, &id)).await
    {
        retry = Some((Instant::now() + SETTLE_RETRY, SETTLE_RETRY * 2));
    }
    let mut stopped = None;
    loop {
        let command = match retry {
            Some((at, _)) => tokio::select! {
                command = queue.recv() => command,
                () = tokio::time::sleep_until(at.into()) => Some(Command::Settle),
            },
            None => queue.recv().await,
        };
        let Some(command) = command else { break };
        match command {
            Command::Run(job) => CURRENT.scope(id, job(&mut app)).await,
            Command::Settle => {
                queued.store(false, Ordering::SeqCst);
                let done = CURRENT.scope(id, settle(&mut app, &flights, &id)).await;
                retry = match (done, retry) {
                    (true, _) => None,
                    (false, Some((_, wait))) => {
                        Some((Instant::now() + wait, (wait * 2).min(SETTLE_RETRY_CAP)))
                    }
                    (false, None) => Some((Instant::now() + SETTLE_RETRY, SETTLE_RETRY * 2)),
                };
            }
            Command::Shutdown(done) => {
                // Calls sent after this one find the wallet closed.
                queue.close();
                while let Ok(late) = queue.try_recv() {
                    drop(late);
                }
                stopped = Some(done);
                break;
            }
            #[cfg(test)]
            Command::Lend(lent, back) => {
                app = match lent.send(app) {
                    Err(app) => app,
                    Ok(()) => match back.await {
                        Ok(app) => app,
                        // The test kept the value: dropping it there released the store.
                        Err(_) => {
                            flights.close(&id, &wake);
                            return finish(ownership, stopped).await;
                        }
                    },
                };
            }
        }
    }
    flights.close(&id, &wake);
    drop(app);
    finish(ownership, stopped).await;
}

async fn finish(
    ownership: Option<Arc<crate::LeaseOwner>>,
    stopped: Option<oneshot::Sender<Result<(), WalletError>>>,
) {
    let released = match ownership {
        Some(owner) => owner.finish().await,
        None => Ok(()),
    };
    if let Some(done) = stopped {
        let _ = done.send(released);
    }
}

#[uniffi::export]
impl NativeWallet {
    /// Finish the call in progress, refuse later calls and release writer ownership so
    /// another handle can load. Dropping the last handle does the same without waiting.
    pub async fn shutdown(&self) -> Result<(), WalletError> {
        runtime::refuse_reentry()?;
        let (done, stopped) = oneshot::channel();
        if self.commands.send(Command::Shutdown(done)).is_err() {
            return Ok(());
        }
        stopped.await.unwrap_or(Ok(()))
    }
    /// Read this installation's App ID.
    pub async fn app_id(&self) -> Result<Vec<u8>, WalletError> {
        self.with_wallet(|app| Box::pin(async move { Ok(app.app_id().into()) }))
            .await
    }
    /// Read the identity key that approved this wallet.
    pub async fn identity(&self) -> Result<Vec<u8>, WalletError> {
        self.with_wallet(|app| Box::pin(async move { Ok(app.identity().into()) }))
            .await
    }
    /// Produce the SDK backup for explicit protected storage by the host.
    pub async fn backup(&self) -> Result<Vec<u8>, WalletError> {
        self.with_wallet(|app| Box::pin(async move { Ok(app.backup().await?.to_bytes()?) }))
            .await
    }
}
impl WalletHost {
    pub(crate) async fn open_wallet(
        &self,
        id: [u8; 16],
        open: impl AsyncFnOnce(AppEnv) -> Result<Paykit, WalletError>,
    ) -> Result<Arc<NativeWallet>, WalletError> {
        let (env, owner) = self.session_env(id).await?;
        match open(env).await {
            Ok(app) => NativeWallet::opened(app, self.rail.clone(), Some(owner)),
            Err(error) => {
                // The failed SDK open has dropped its store. Wait for the physical
                // writer to release before a host can act on the failure and retry.
                owner.finish().await?;
                Err(error)
            }
        }
    }
    pub(crate) async fn session_env(
        &self,
        id: [u8; 16],
    ) -> Result<(AppEnv, Arc<crate::LeaseOwner>), WalletError> {
        let session = crate::WalletStoreSession::open(
            self.store.clone(),
            pubky_switch_client::store::client_namespace(&id),
        )
        .await?;
        let owner = session.ownership.clone();
        let env = AppEnv {
            store: StoreHandle::new(session),
            clock: self.clock.clone(),
            transport: self.transport.clone(),
            homeserver: self.home.clone(),
            homeserver_writer: Some(self.writer.clone()),
        };
        Ok((env, owner))
    }
    pub(crate) async fn load_app(&self, env: AppEnv, id: [u8; 16]) -> Result<Paykit, PaykitError> {
        Paykit::load(
            env,
            self.config.clone(),
            id,
            self.rail.clone(),
            self.timer.clone(),
        )
        .await
    }
}
#[uniffi::export]
impl WalletHost {
    /// Open the existing wallet under its original App ID and exclusive storage lease.
    pub async fn load(&self, app_id: Vec<u8>) -> Result<Arc<NativeWallet>, WalletError> {
        let id = fixed(app_id, "app id")?;
        let this = self.clone();
        runtime::run(async move {
            this.open_wallet(id, async |env| Ok(this.load_app(env, id).await?))
                .await
        })
        .await
    }
}
