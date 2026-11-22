use crate::{fixed, runtime, WalletError, WalletHost};
use paykit_sdk::{AppEnv, Paykit, PaykitError};
use pubky_switch_client::store::StoreHandle;
use pubky_switch_client::transport::BoxFuture;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// One wallet command: it runs on the wallet's worker with the SDK value to itself and sends
/// its own answer.
type Job = Box<dyn for<'a> FnOnce(&'a mut Paykit) -> BoxFuture<'a, ()> + Send>;

enum Command {
    Run(Job),
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
    pub(crate) pace: Arc<Mutex<pubky_switch_client::app::WaitPace>>,
}

impl NativeWallet {
    pub(crate) fn opened(
        app: Paykit,
        ownership: Option<Arc<crate::LeaseOwner>>,
    ) -> Result<Arc<Self>, WalletError> {
        let (commands, queue) = mpsc::unbounded_channel();
        runtime::spawn(work(app, commands.downgrade(), queue, ownership))?;
        Ok(Arc::new(Self {
            commands,
            pace: Arc::default(),
        }))
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
    /// The wallet whose worker runs the current command.
    static CURRENT: mpsc::WeakUnboundedSender<Command>;
}

/// Inside a wallet command: a way to queue a resume of that wallet's payments later, for a
/// late answer from the host's `pay`. Nothing is queued once the wallet is closed; its next
/// open resumes the payment through the host's lookup.
pub(crate) fn settle_target() -> Option<crate::flights::Settle> {
    let wallet = CURRENT.try_with(Clone::clone).ok()?;
    Some(Box::new(move || {
        let Some(commands) = wallet.upgrade() else {
            return;
        };
        let job: Job = Box::new(|app| {
            Box::pin(async move {
                // A failed resume leaves the attempt open for the next one.
                let _ = app.resume_payments().await;
            })
        });
        let _ = commands.send(Command::Run(job));
    }))
}

/// The wallet's worker. It stops after a shutdown, or once every handle is gone and the
/// calls already sent have run; then it drops the SDK value and releases the store.
async fn work(
    mut app: Paykit,
    own: mpsc::WeakUnboundedSender<Command>,
    mut queue: mpsc::UnboundedReceiver<Command>,
    ownership: Option<Arc<crate::LeaseOwner>>,
) {
    let mut stopped = None;
    while let Some(command) = queue.recv().await {
        match command {
            Command::Run(job) => CURRENT.scope(own.clone(), job(&mut app)).await,
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
                        Err(_) => return finish(ownership, stopped).await,
                    },
                };
            }
        }
    }
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
            Ok(app) => NativeWallet::opened(app, Some(owner)),
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
            Default::default(),
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
