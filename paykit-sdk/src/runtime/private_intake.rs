use super::*;
use crate::storage::{
    BufferedPrivatePacketRecord, PrivateIntakeBufferOutcome, PrivateIntakeCursorRecord,
    PrivateIntakeSupervisorLease,
};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::sync::{watch, Notify, RwLock as AsyncRwLock};

const MAX_EVENT_STREAM_USERS: usize = 50;
const DEFAULT_MAX_EVENT_BYTES: usize = 4096;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_STREAM_CAPACITY: usize = 1024;
const DEFAULT_HEALTHY_AUDIT_INTERVAL: Duration = Duration::from_secs(60);
const MIN_HEALTHY_AUDIT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_HEALTHY_AUDIT_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_AUDIT_JITTER: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const SUPERVISOR_LEASE_TIMEOUT: chrono::Duration = chrono::Duration::minutes(2);
const SUPERVISOR_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
const MAX_AUDIT_ACQUISITIONS_PER_PEER: usize =
    crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER + 1;

/// Validated limits for SSE-assisted private intake.
#[derive(Clone, Debug)]
pub struct PrivateIntakeConfig {
    /// Maximum homeserver stream connections reserved for this supervisor (1..=1024).
    pub stream_capacity: usize,
    /// Maximum SSE frame size accepted from a homeserver (1..=65536 bytes).
    pub max_event_bytes: usize,
    /// Expected-slot reconciliation cadence while SSE is healthy (1 second..=1 hour).
    pub healthy_audit_interval: Duration,
}

impl PrivateIntakeConfig {
    /// Create config with bounded events and sixty-second healthy audits.
    pub fn new(stream_capacity: usize) -> Result<Self> {
        let config = Self {
            stream_capacity,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
            healthy_audit_interval: DEFAULT_HEALTHY_AUDIT_INTERVAL,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.stream_capacity == 0
            || self.stream_capacity > MAX_STREAM_CAPACITY
            || self.max_event_bytes == 0
            || self.max_event_bytes > MAX_EVENT_BYTES
            || self.healthy_audit_interval < MIN_HEALTHY_AUDIT_INTERVAL
            || self.healthy_audit_interval > MAX_HEALTHY_AUDIT_INTERVAL
        {
            return Err(PaykitSdkError::Policy {
                context: "private intake config is outside supported bounded limits".into(),
                source: None,
            });
        }
        Ok(())
    }
}

/// Current transport state for one supervised Linked Peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateIntakeTransportState {
    /// Connecting or reconnecting to homeserver SSE.
    Connecting,
    /// Replaying from durable per-user cursor.
    Replaying,
    /// Startup or healthy audit reached absent next expected slot.
    CaughtUp,
    /// Audit or stream failed; remote absence cannot be claimed.
    Stale,
    /// Durable packet capacity stopped cursor progress for this peer.
    Backpressured,
    /// Homeserver rejected or did not provide required SSE.
    Unsupported,
    /// Supervisor was explicitly stopped.
    Stopped,
}

/// Redacted supervisor health snapshot keyed by Linked Peer.
#[derive(Clone, Debug, Default)]
pub struct PrivateIntakeHealth {
    /// Per-peer transport state.
    pub peers: HashMap<PubkyPublicKey, PrivateIntakeTransportState>,
    /// Number of reserved homeserver stream connections.
    pub reserved_streams: usize,
    /// Last successful expected-slot audit for each Linked Peer.
    pub last_successful_audit_at: HashMap<PubkyPublicKey, DateTime<Utc>>,
}

/// Explicit lifecycle handle for SSE-assisted private intake.
pub struct PrivateIntakeSupervisor {
    stop: watch::Sender<bool>,
    tasks: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    health: Arc<AsyncRwLock<PrivateIntakeHealth>>,
    work: Arc<Notify>,
}

impl PrivateIntakeSupervisor {
    /// Return current redacted transport health.
    pub async fn health(&self) -> PrivateIntakeHealth {
        self.health.read().await.clone()
    }

    /// Notification fired after a packet becomes durable.
    ///
    /// Consumers must inspect durable state after registering and after wake-up;
    /// notifications are coalesced and are not durable work items.
    pub fn subscribe_work(&self) -> Arc<Notify> {
        Arc::clone(&self.work)
    }

    /// Stop all streams and wait for their cancellation-safe exits.
    pub async fn stop(&self) {
        let _ = self.stop.send(true);
        let tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for task in tasks {
            let _ = task.await;
        }
        let mut health = self.health.write().await;
        for state in health.peers.values_mut() {
            *state = PrivateIntakeTransportState::Stopped;
        }
    }
}

impl Drop for PrivateIntakeSupervisor {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        // Dropping JoinHandles detaches tasks. They observe stop, exit their
        // cancellation-safe loops, and release ownership after streams stop.
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SubscriptionGroup {
    homeserver: paykit_lib::PublicKey,
    homeserver_name: String,
    peers: Vec<PubkyPublicKey>,
}

enum PrivatePacketAcquisition {
    Absent,
    Inserted,
    AlreadyBuffered,
    CapacityReached,
}

fn next_private_intake_counter(
    state: &crate::storage::StorageState,
    counterparty: &PubkyPublicKey,
    generation: u64,
    snapshot_counter: u64,
) -> Result<u64> {
    let Some(buffered) = state.private_intake_peers.get(counterparty) else {
        return Ok(snapshot_counter);
    };
    if buffered.link_generation != generation {
        return Err(PaykitSdkError::RecoveryRequired {
            context: "buffered private intake generation does not match Encrypted Link".into(),
            source: None,
        });
    }
    let Some(tail) = buffered.buffered_packets.last() else {
        return Ok(snapshot_counter);
    };
    if buffered.buffered_packets[0].receive_counter != snapshot_counter {
        return Err(PaykitSdkError::RecoveryRequired {
            context: "buffered private intake does not begin at Noise read counter".into(),
            source: None,
        });
    }
    tail.receive_counter
        .checked_add(1)
        .ok_or_else(|| PaykitSdkError::RecoveryRequired {
            context: "private intake receive counter is exhausted".into(),
            source: None,
        })
}

fn partition_public_subscriptions(users: Vec<PubkyPublicKey>) -> Vec<Vec<PubkyPublicKey>> {
    users
        .chunks(MAX_EVENT_STREAM_USERS)
        .map(<[PubkyPublicKey]>::to_vec)
        .collect()
}

#[cfg(test)]
fn partition_private_subscriptions(users: Vec<PubkyPublicKey>) -> Vec<Vec<PubkyPublicKey>> {
    users.into_iter().map(|user| vec![user]).collect()
}

fn reserve_stream_capacity(capacity: usize, required: usize) -> Result<usize> {
    if required > capacity {
        return Err(PaykitSdkError::Policy {
            context: format!(
                "private intake requires {required} streams but caller reserved {capacity}"
            ),
            source: None,
        });
    }
    Ok(required)
}

fn subscribable_private_intake_peers(
    state: &crate::storage::StorageState,
    peers: &[PubkyPublicKey],
) -> Vec<PubkyPublicKey> {
    let global_count = state
        .private_intake_peers
        .values()
        .map(|peer| peer.buffered_packets.len())
        .sum::<usize>();
    if global_count >= crate::storage::PRIVATE_INTAKE_MAX_PACKETS_GLOBAL {
        return Vec::new();
    }
    peers
        .iter()
        .filter(|peer| {
            state.private_intake_peers.get(*peer).is_none_or(|record| {
                record.buffered_packets.len() < crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER
            })
        })
        .cloned()
        .collect()
}

fn route_private_intake_event(
    group: &SubscriptionGroup,
    event: pubky::Event,
) -> Option<(PubkyPublicKey, PrivateIntakeCursorRecord)> {
    let peer = PubkyPublicKey::from_public_key(&event.resource.owner);
    group.peers.contains(&peer).then(|| {
        let cursor = PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: group.homeserver_name.clone(),
            event_id: event.cursor.id(),
        };
        (peer, cursor)
    })
}

async fn audit_private_intake_peer<F, Fut>(
    peer: PubkyPublicKey,
    mut acquire: F,
) -> Result<PrivatePacketAcquisition>
where
    F: FnMut(PubkyPublicKey, Option<PrivateIntakeCursorRecord>) -> Fut,
    Fut: Future<Output = Result<PrivatePacketAcquisition>>,
{
    for _ in 0..MAX_AUDIT_ACQUISITIONS_PER_PEER {
        let outcome = acquire(peer.clone(), None).await?;
        match outcome {
            PrivatePacketAcquisition::Inserted => continue,
            PrivatePacketAcquisition::Absent
            | PrivatePacketAcquisition::AlreadyBuffered
            | PrivatePacketAcquisition::CapacityReached => return Ok(outcome),
        }
    }
    Ok(PrivatePacketAcquisition::Inserted)
}

enum PrivateIntakeEventLoopExit {
    Stopped,
    Ended,
    Backpressured,
    Failed,
}

type PrivateIntakeAcquisitionFuture =
    Pin<Box<dyn Future<Output = Result<PrivatePacketAcquisition>> + Send>>;
type PrivateIntakeAcquire = Arc<
    dyn Fn(PubkyPublicKey, Option<PrivateIntakeCursorRecord>) -> PrivateIntakeAcquisitionFuture
        + Send
        + Sync,
>;
type PrivateIntakeAuditFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type PrivateIntakeAudit = Arc<dyn Fn() -> PrivateIntakeAuditFuture + Send + Sync>;

async fn process_private_intake_event<F, Fut>(
    group: &SubscriptionGroup,
    event: pubky::Event,
    acquire: &mut F,
) -> Option<(PubkyPublicKey, Result<PrivatePacketAcquisition>)>
where
    F: FnMut(PubkyPublicKey, Option<PrivateIntakeCursorRecord>) -> Fut,
    Fut: Future<Output = Result<PrivatePacketAcquisition>>,
{
    let (peer, cursor) = route_private_intake_event(group, event)?;
    let outcome = acquire(peer.clone(), Some(cursor)).await;
    Some((peer, outcome))
}

#[allow(
    clippy::too_many_arguments,
    reason = "connected loop receives explicit lifecycle, health, and injected I/O boundaries"
)]
async fn run_connected_private_intake_stream(
    group: &SubscriptionGroup,
    mut stream: Pin<
        Box<
            dyn futures_util::Stream<Item = std::result::Result<pubky::Event, pubky::Error>> + Send,
        >,
    >,
    audit_interval: Duration,
    audit_delay: Duration,
    mut stop: watch::Receiver<bool>,
    health: &Arc<AsyncRwLock<PrivateIntakeHealth>>,
    work: &Arc<Notify>,
    acquire: PrivateIntakeAcquire,
    audit_group: PrivateIntakeAudit,
) -> PrivateIntakeEventLoopExit {
    let mut audit =
        tokio::time::interval_at(tokio::time::Instant::now() + audit_delay, audit_interval);
    audit.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return PrivateIntakeEventLoopExit::Stopped;
                }
            }
            _ = audit.tick() => audit_group().await,
            event = stream.next() => match event {
                Some(Ok(event)) => {
                    let mut acquire_one = |peer, cursor| acquire(peer, cursor);
                    match process_private_intake_event(group, event, &mut acquire_one).await {
                        Some((peer, Ok(PrivatePacketAcquisition::Inserted))) => {
                            work.notify_waiters();
                            set_peer_state(health, peer, PrivateIntakeTransportState::Replaying).await;
                        }
                        Some((peer, Ok(PrivatePacketAcquisition::AlreadyBuffered))) => {
                            set_peer_state(health, peer, PrivateIntakeTransportState::Replaying).await;
                        }
                        Some((peer, Ok(PrivatePacketAcquisition::Absent))) => {
                            set_peer_state(health, peer, PrivateIntakeTransportState::CaughtUp).await;
                        }
                        Some((peer, Ok(PrivatePacketAcquisition::CapacityReached))) => {
                            set_peer_state(health, peer.clone(), PrivateIntakeTransportState::Backpressured).await;
                            return PrivateIntakeEventLoopExit::Backpressured;
                        }
                        Some((peer, Err(_))) => {
                            set_peer_state(health, peer, PrivateIntakeTransportState::Stale).await;
                        }
                        None => {}
                    }
                }
                Some(Err(_)) => return PrivateIntakeEventLoopExit::Failed,
                None => return PrivateIntakeEventLoopExit::Ended,
            }
        }
    }
}

async fn claim_private_intake_lease<S: StorageAdapter>(
    storage: &S,
    now: DateTime<Utc>,
) -> Result<PrivateIntakeSupervisorLease> {
    storage
        .transaction(move |tx| {
            tx.claim_private_intake_supervisor(now, now + SUPERVISOR_LEASE_TIMEOUT)?
                .ok_or_else(|| PaykitSdkError::Policy {
                    context: "private intake supervisor is already running for this identity"
                        .into(),
                    source: None,
                })
        })
        .await
}

async fn release_private_intake_lease<S: StorageAdapter>(storage: &S, lease_id: u64) -> Result<()> {
    storage
        .transaction(move |tx| {
            tx.release_private_intake_supervisor(lease_id);
            Ok(())
        })
        .await
}

fn audit_jitter(group: &SubscriptionGroup) -> Duration {
    let millis = group
        .peers
        .first()
        .map(|peer| {
            peer.as_str()
                .bytes()
                .fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(byte)))
                % (MAX_AUDIT_JITTER.as_millis() as u64 + 1)
        })
        .unwrap_or_default();
    Duration::from_millis(millis)
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter + 'static,
    K: PubkySessionProvider + 'static,
    P: PaymentAdapter + 'static,
    C: Clock,
{
    async fn resolve_private_intake_groups(&self) -> Result<Vec<SubscriptionGroup>> {
        let (access, peers) = self.private_receive_context(None).await?;
        let mut by_homeserver: HashMap<String, (paykit_lib::PublicKey, Vec<PubkyPublicKey>)> =
            HashMap::new();
        for (peer, snapshot) in peers {
            if snapshot.is_none() {
                continue;
            }
            let remote = peer.to_public_key()?;
            let homeserver = access
                .outbox_client
                .get_homeserver_of(&remote)
                .await
                .map_err(|error| PaykitSdkError::Transport {
                    context: "resolve private intake homeserver".into(),
                    source: Some(error.into()),
                })?
                .ok_or_else(|| PaykitSdkError::NotFound {
                    context: "Linked Peer has no Pubky homeserver".into(),
                    source: None,
                })?;
            by_homeserver
                .entry(homeserver.to_string())
                .or_insert_with(|| (homeserver, Vec::new()))
                .1
                .push(peer);
        }

        let mut groups = Vec::new();
        for (homeserver_name, (homeserver, mut peers)) in by_homeserver {
            peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            for peers in partition_public_subscriptions(peers) {
                groups.push(SubscriptionGroup {
                    homeserver: homeserver.clone(),
                    homeserver_name: homeserver_name.clone(),
                    peers,
                });
            }
        }
        groups.sort_by(|left, right| {
            left.homeserver_name
                .cmp(&right.homeserver_name)
                .then_with(|| left.peers[0].as_str().cmp(right.peers[0].as_str()))
        });
        Ok(groups)
    }

    /// Start caller-owned SSE-assisted intake for every established Linked Peer.
    ///
    /// SSE events are hints only. Each hint and healthy audit fetches only next
    /// expected immutable Noise slot, then commits ciphertext with per-user cursor.
    /// No Noise receive checkpoint advances until explicit buffered processing.
    pub async fn start_private_intake(
        self: &Arc<Self>,
        config: PrivateIntakeConfig,
    ) -> Result<PrivateIntakeSupervisor> {
        config.validate()?;
        let groups = self.resolve_private_intake_groups().await?;
        let reserved_streams = reserve_stream_capacity(config.stream_capacity, groups.len())?;
        let lease = claim_private_intake_lease(&self.storage, self.clock.now()).await?;
        let lease_id = lease.lease_id;
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth {
            peers: groups
                .iter()
                .flat_map(|group| group.peers.iter().cloned())
                .map(|peer| (peer, PrivateIntakeTransportState::Connecting))
                .collect(),
            reserved_streams,
            last_successful_audit_at: HashMap::new(),
        }));
        let work = Arc::new(Notify::new());
        let (stop, _) = watch::channel(false);
        let mut tasks = Vec::with_capacity(groups.len() + 1);
        {
            let sdk = Arc::clone(self);
            let mut stop_rx = stop.subscribe();
            let stop_tx = stop.clone();
            tasks.push(tokio::spawn(async move {
                let mut renewal = tokio::time::interval(SUPERVISOR_LEASE_RENEW_INTERVAL);
                renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                renewal.tick().await;
                loop {
                    tokio::select! {
                        changed = stop_rx.changed() => {
                            if changed.is_err() || *stop_rx.borrow() { break; }
                        }
                        _ = renewal.tick() => {
                            let now = sdk.clock.now();
                            let renewed = sdk.storage.transaction(move |tx| {
                                Ok(tx.renew_private_intake_supervisor(
                                    lease_id,
                                    now + SUPERVISOR_LEASE_TIMEOUT,
                                ))
                            }).await.unwrap_or(false);
                            if !renewed {
                                let _ = stop_tx.send(true);
                                return;
                            }
                        }
                    }
                }
            }));
        }
        {
            let sdk = Arc::clone(self);
            let stop_rx = stop.subscribe();
            let health = Arc::clone(&health);
            let work = Arc::clone(&work);
            tasks.push(tokio::spawn(async move {
                sdk.clone()
                    .run_private_intake_topology(groups, config, stop_rx, health, work)
                    .await;
                let _ = release_private_intake_lease(&sdk.storage, lease_id).await;
            }));
        }
        Ok(PrivateIntakeSupervisor {
            stop,
            tasks: StdMutex::new(tasks),
            health,
            work,
        })
    }

    async fn run_private_intake_topology(
        self: Arc<Self>,
        mut groups: Vec<SubscriptionGroup>,
        config: PrivateIntakeConfig,
        mut stop: watch::Receiver<bool>,
        health: Arc<AsyncRwLock<PrivateIntakeHealth>>,
        work: Arc<Notify>,
    ) {
        loop {
            if *stop.borrow() {
                return;
            }
            let mappings = groups
                .iter()
                .flat_map(|group| {
                    group
                        .peers
                        .iter()
                        .cloned()
                        .map(|peer| (peer, group.homeserver_name.clone()))
                })
                .collect::<Vec<_>>();
            if self
                .storage
                .transaction(move |tx| {
                    for (peer, homeserver) in mappings {
                        tx.reconcile_private_intake_cursor_homeserver(&peer, &homeserver);
                    }
                    Ok(())
                })
                .await
                .is_err()
            {
                for group in &groups {
                    set_group_state(&health, group, PrivateIntakeTransportState::Stale).await;
                }
                if wait_reconnect_or_stop(&mut stop).await {
                    return;
                }
                continue;
            }

            {
                let mut current = health.write().await;
                let active_peers = groups
                    .iter()
                    .flat_map(|group| group.peers.iter())
                    .cloned()
                    .collect::<std::collections::HashSet<_>>();
                current.peers.retain(|peer, _| active_peers.contains(peer));
                current
                    .last_successful_audit_at
                    .retain(|peer, _| active_peers.contains(peer));
                current.reserved_streams = groups.len();
                for peer in active_peers {
                    current
                        .peers
                        .entry(peer)
                        .or_insert(PrivateIntakeTransportState::Connecting);
                }
            }
            let (epoch_stop, _) = watch::channel(false);
            let mut handles = Vec::with_capacity(groups.len());
            for group in groups.clone() {
                let sdk = Arc::clone(&self);
                let config = config.clone();
                let health = Arc::clone(&health);
                let work = Arc::clone(&work);
                let stop_rx = epoch_stop.subscribe();
                handles.push(tokio::spawn(async move {
                    sdk.run_private_intake_group(group, config, stop_rx, health, work)
                        .await;
                }));
            }

            let next_groups = loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() { break None; }
                    }
                    _ = tokio::time::sleep(RECONNECT_DELAY) => {
                        match self.resolve_private_intake_groups().await {
                            Ok(resolved) if resolved != groups => {
                                if reserve_stream_capacity(config.stream_capacity, resolved.len()).is_ok() {
                                    break Some(resolved);
                                }
                                for group in &resolved {
                                    set_group_state(&health, group, PrivateIntakeTransportState::Stale).await;
                                }
                            }
                            Ok(_) => {}
                            Err(_) => {
                                for group in &groups {
                                    set_group_state(&health, group, PrivateIntakeTransportState::Stale).await;
                                }
                            }
                        }
                    }
                }
            };
            let _ = epoch_stop.send(true);
            for handle in handles {
                let _ = handle.await;
            }
            let Some(resolved) = next_groups else {
                return;
            };
            groups = resolved;
        }
    }

    async fn run_private_intake_group(
        self: Arc<Self>,
        group: SubscriptionGroup,
        config: PrivateIntakeConfig,
        mut stop: watch::Receiver<bool>,
        health: Arc<AsyncRwLock<PrivateIntakeHealth>>,
        work: Arc<Notify>,
    ) {
        loop {
            if *stop.borrow() {
                break;
            }
            self.audit_private_intake_group(&group, &health, &work)
                .await;
            let active_peers = match self
                .storage
                .transaction({
                    let peers = group.peers.clone();
                    move |tx| {
                        Ok(subscribable_private_intake_peers(
                            &tx.export_storage_state(),
                            &peers,
                        ))
                    }
                })
                .await
            {
                Ok(peers) => peers,
                Err(_) => {
                    set_group_state(&health, &group, PrivateIntakeTransportState::Stale).await;
                    if wait_reconnect_or_stop(&mut stop).await {
                        break;
                    }
                    continue;
                }
            };
            for peer in group
                .peers
                .iter()
                .filter(|peer| !active_peers.contains(peer))
            {
                set_peer_state(
                    &health,
                    peer.clone(),
                    PrivateIntakeTransportState::Backpressured,
                )
                .await;
            }
            if active_peers.is_empty() {
                if wait_reconnect_or_stop(&mut stop).await {
                    break;
                }
                continue;
            }
            let active_group = SubscriptionGroup {
                homeserver: group.homeserver.clone(),
                homeserver_name: group.homeserver_name.clone(),
                peers: active_peers,
            };
            let access = match self
                .load_session_access_for_initialized_identity("start private intake")
                .await
            {
                Ok(access) => access,
                Err(_) => {
                    set_group_state(&health, &group, PrivateIntakeTransportState::Stale).await;
                    if wait_reconnect_or_stop(&mut stop).await {
                        break;
                    }
                    continue;
                }
            };
            let cursors = match self
                .storage
                .transaction({
                    let peers = active_group.peers.clone();
                    let homeserver = active_group.homeserver_name.clone();
                    move |tx| {
                        let state = tx.export_storage_state();
                        Ok(peers
                            .into_iter()
                            .map(|peer| {
                                let cursor = state
                                    .private_intake_cursors
                                    .get(&peer)
                                    .filter(|cursor| cursor.homeserver == homeserver)
                                    .map(|cursor| pubky::EventCursor::new(cursor.event_id));
                                (peer, cursor)
                            })
                            .collect::<Vec<_>>())
                    }
                })
                .await
            {
                Ok(cursors) => cursors,
                Err(_) => {
                    set_group_state(&health, &group, PrivateIntakeTransportState::Stale).await;
                    if wait_reconnect_or_stop(&mut stop).await {
                        break;
                    }
                    continue;
                }
            };
            let public_keys = cursors
                .iter()
                .filter_map(|(peer, cursor)| peer.to_public_key().ok().map(|key| (key, *cursor)))
                .collect::<Vec<_>>();
            let users = public_keys.iter().map(|(key, cursor)| (key, *cursor));
            let builder = match pubky::EventStreamBuilder::for_homeserver(
                access.outbox_client.client().clone(),
                &active_group.homeserver,
            )
            .add_users(users)
            {
                Ok(builder) => builder,
                Err(_) => {
                    set_group_state(
                        &health,
                        &active_group,
                        PrivateIntakeTransportState::Unsupported,
                    )
                    .await;
                    break;
                }
            };
            let stream = match builder
                .path(paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX)
                .max_event_bytes(config.max_event_bytes)
                .live()
                .subscribe()
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    if is_unsupported_event_stream(&error) {
                        set_group_state(
                            &health,
                            &active_group,
                            PrivateIntakeTransportState::Unsupported,
                        )
                        .await;
                        break;
                    }
                    set_group_state(&health, &active_group, PrivateIntakeTransportState::Stale)
                        .await;
                    if wait_reconnect_or_stop(&mut stop).await {
                        break;
                    }
                    continue;
                }
            };
            set_group_state(
                &health,
                &active_group,
                PrivateIntakeTransportState::Replaying,
            )
            .await;
            let acquire: PrivateIntakeAcquire = {
                let sdk = Arc::clone(&self);
                Arc::new(move |peer, cursor| {
                    let sdk = Arc::clone(&sdk);
                    Box::pin(async move { sdk.acquire_expected_private_packet(peer, cursor).await })
                })
            };
            let audit_group: PrivateIntakeAudit = {
                let sdk = Arc::clone(&self);
                let group = group.clone();
                let health = Arc::clone(&health);
                let work = Arc::clone(&work);
                Arc::new(move || {
                    let sdk = Arc::clone(&sdk);
                    let group = group.clone();
                    let health = Arc::clone(&health);
                    let work = Arc::clone(&work);
                    Box::pin(async move {
                        sdk.audit_private_intake_group(&group, &health, &work).await;
                    })
                })
            };
            let exit = run_connected_private_intake_stream(
                &active_group,
                stream,
                config.healthy_audit_interval,
                audit_jitter(&active_group),
                stop.clone(),
                &health,
                &work,
                acquire,
                audit_group,
            )
            .await;
            if matches!(exit, PrivateIntakeEventLoopExit::Stopped) {
                return;
            }
            if matches!(
                exit,
                PrivateIntakeEventLoopExit::Ended | PrivateIntakeEventLoopExit::Failed
            ) {
                set_group_state(&health, &active_group, PrivateIntakeTransportState::Stale).await;
            }
            if wait_reconnect_or_stop(&mut stop).await {
                break;
            }
        }
    }

    async fn audit_private_intake_group(
        &self,
        group: &SubscriptionGroup,
        health: &Arc<AsyncRwLock<PrivateIntakeHealth>>,
        work: &Arc<Notify>,
    ) {
        let active_peers = match self
            .storage
            .transaction({
                let peers = group.peers.clone();
                move |tx| {
                    Ok(subscribable_private_intake_peers(
                        &tx.export_storage_state(),
                        &peers,
                    ))
                }
            })
            .await
        {
            Ok(peers) => peers,
            Err(_) => {
                set_group_state(health, group, PrivateIntakeTransportState::Stale).await;
                return;
            }
        };
        for peer in &group.peers {
            if !active_peers.contains(peer) {
                set_peer_state(
                    health,
                    peer.clone(),
                    PrivateIntakeTransportState::Backpressured,
                )
                .await;
                continue;
            }
            match audit_private_intake_peer(peer.clone(), |peer, cursor| {
                self.acquire_expected_private_packet(peer, cursor)
            })
            .await
            {
                Ok(PrivatePacketAcquisition::Inserted) => {
                    work.notify_waiters();
                    set_peer_audit_state(
                        health,
                        peer.clone(),
                        PrivateIntakeTransportState::Replaying,
                        self.clock.now(),
                    )
                    .await;
                }
                Ok(PrivatePacketAcquisition::AlreadyBuffered) => {
                    set_peer_audit_state(
                        health,
                        peer.clone(),
                        PrivateIntakeTransportState::Replaying,
                        self.clock.now(),
                    )
                    .await;
                }
                Ok(PrivatePacketAcquisition::Absent) => {
                    set_peer_audit_state(
                        health,
                        peer.clone(),
                        PrivateIntakeTransportState::CaughtUp,
                        self.clock.now(),
                    )
                    .await;
                }
                Ok(PrivatePacketAcquisition::CapacityReached) => {
                    set_peer_audit_state(
                        health,
                        peer.clone(),
                        PrivateIntakeTransportState::Backpressured,
                        self.clock.now(),
                    )
                    .await;
                }
                Err(_) => {
                    set_peer_state(health, peer.clone(), PrivateIntakeTransportState::Stale).await
                }
            }
        }
    }

    async fn acquire_expected_private_packet(
        &self,
        counterparty: PubkyPublicKey,
        cursor: Option<PrivateIntakeCursorRecord>,
    ) -> Result<PrivatePacketAcquisition> {
        let (access, snapshots) = self.private_receive_context(Some(&counterparty)).await?;
        let snapshot = snapshots
            .into_iter()
            .next()
            .and_then(|(_, snapshot)| snapshot)
            .ok_or_else(|| PaykitSdkError::RecoveryRequired {
                context: "Linked Peer has no current Encrypted Link snapshot".into(),
                source: None,
            })?;
        let secret = access.paykit_noise_secret_key()?;
        let session_info = access.session.info();
        let local = session_info.public_key();
        let snapshot_counter = snapshot
            .link
            .next_private_application_message_resource(local, &secret)?
            .1;
        let counterparty_for_tail = counterparty.clone();
        let generation = snapshot.state.generation;
        let receive_counter = self
            .storage
            .transaction(move |tx| {
                let state = tx.export_storage_state();
                next_private_intake_counter(
                    &state,
                    &counterparty_for_tail,
                    generation,
                    snapshot_counter,
                )
            })
            .await?;
        let (resource, receive_counter) = snapshot
            .link
            .private_application_message_resource_at_counter(local, &secret, receive_counter)?;
        let response = match access.outbox_client.public_storage().get(resource).await {
            Ok(response) => response,
            Err(error) if is_pubky_not_found(&error) => {
                return self.commit_absent_private_intake_cursor(cursor).await;
            }
            Err(error) => {
                return Err(map_pubky_transport_error(
                    "fetch expected private intake slot",
                    error,
                ))
            }
        };
        let ciphertext = read_public_response(
            response,
            crate::storage::PRIVATE_INTAKE_PACKET_BYTES,
            "read expected private intake slot",
        )
        .await?;
        let Some(ciphertext) = ciphertext else {
            return self.commit_absent_private_intake_cursor(cursor).await;
        };
        let packet = BufferedPrivatePacketRecord {
            counterparty,
            link_generation: snapshot.state.generation,
            receive_counter,
            ciphertext,
            source_cursor: cursor,
            buffered_at: self.clock.now(),
        };
        self.storage
            .transaction(move |tx| tx.buffer_private_intake_packet(packet))
            .await
            .map(|outcome| match outcome {
                PrivateIntakeBufferOutcome::Inserted => PrivatePacketAcquisition::Inserted,
                PrivateIntakeBufferOutcome::AlreadyBuffered => {
                    PrivatePacketAcquisition::AlreadyBuffered
                }
                PrivateIntakeBufferOutcome::CapacityReached => {
                    PrivatePacketAcquisition::CapacityReached
                }
            })
    }

    async fn commit_absent_private_intake_cursor(
        &self,
        cursor: Option<PrivateIntakeCursorRecord>,
    ) -> Result<PrivatePacketAcquisition> {
        if let Some(cursor) = cursor {
            self.storage
                .transaction(move |tx| tx.advance_private_intake_cursor(cursor))
                .await?;
        }
        Ok(PrivatePacketAcquisition::Absent)
    }
}

async fn set_peer_state(
    health: &Arc<AsyncRwLock<PrivateIntakeHealth>>,
    peer: PubkyPublicKey,
    state: PrivateIntakeTransportState,
) {
    health.write().await.peers.insert(peer, state);
}

async fn set_peer_audit_state(
    health: &Arc<AsyncRwLock<PrivateIntakeHealth>>,
    peer: PubkyPublicKey,
    state: PrivateIntakeTransportState,
    audited_at: DateTime<Utc>,
) {
    let mut health = health.write().await;
    health.peers.insert(peer.clone(), state);
    health.last_successful_audit_at.insert(peer, audited_at);
}

async fn set_group_state(
    health: &Arc<AsyncRwLock<PrivateIntakeHealth>>,
    group: &SubscriptionGroup,
    state: PrivateIntakeTransportState,
) {
    let mut health = health.write().await;
    for peer in &group.peers {
        health.peers.insert(peer.clone(), state);
    }
}

async fn wait_reconnect_or_stop(stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(RECONNECT_DELAY) => false,
        changed = stop.changed() => changed.is_err() || *stop.borrow(),
    }
}

fn is_unsupported_event_stream(error: &pubky::Error) -> bool {
    matches!(
        error,
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. })
            if *status == pubky::StatusCode::NOT_FOUND
                || *status == pubky::StatusCode::METHOD_NOT_ALLOWED
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn users(count: usize) -> Vec<PubkyPublicKey> {
        (0..count)
            .map(|_| PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()))
            .collect()
    }

    #[test]
    fn test_private_intake_partitions_public_users_at_fifty() {
        assert_eq!(partition_public_subscriptions(users(50)).len(), 1);
        let groups = partition_public_subscriptions(users(51));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 50);
        assert_eq!(groups[1].len(), 1);
    }

    #[test]
    fn test_private_intake_private_subscription_is_single_user() {
        let users = users(2);
        let groups = partition_private_subscriptions(users.clone());
        assert_eq!(groups, vec![vec![users[0].clone()], vec![users[1].clone()]]);
    }

    #[test]
    fn test_private_intake_requires_capacity_for_every_partition() {
        let error = reserve_stream_capacity(1, 2).unwrap_err();
        assert!(matches!(error, PaykitSdkError::Policy { .. }));
        assert_eq!(reserve_stream_capacity(2, 2).unwrap(), 2);
    }

    #[test]
    fn test_private_intake_audit_jitter_is_bounded() {
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver".into(),
            peers: users(1),
        };
        assert!(audit_jitter(&group) <= MAX_AUDIT_JITTER);
    }

    #[test]
    fn test_private_intake_config_requires_bounded_events_and_audit() {
        assert!(PrivateIntakeConfig::new(1).is_ok());
        assert!(PrivateIntakeConfig {
            stream_capacity: 1,
            max_event_bytes: 0,
            healthy_audit_interval: std::time::Duration::from_secs(60),
        }
        .validate()
        .is_err());
        assert!(PrivateIntakeConfig {
            stream_capacity: 1,
            max_event_bytes: 4096,
            healthy_audit_interval: std::time::Duration::ZERO,
        }
        .validate()
        .is_err());
        assert!(PrivateIntakeConfig {
            stream_capacity: MAX_STREAM_CAPACITY + 1,
            max_event_bytes: MAX_EVENT_BYTES + 1,
            healthy_audit_interval: MAX_HEALTHY_AUDIT_INTERVAL + std::time::Duration::from_secs(1),
        }
        .validate()
        .is_err());
    }

    #[tokio::test]
    async fn test_private_intake_lease_is_identity_wide_across_sdk_handles() {
        let storage = crate::storage::InMemoryStorage::new();
        let first = claim_private_intake_lease(&storage, Utc::now())
            .await
            .unwrap();
        let second = claim_private_intake_lease(&storage.clone(), Utc::now()).await;
        assert!(matches!(second, Err(PaykitSdkError::Policy { .. })));

        release_private_intake_lease(&storage, first.lease_id)
            .await
            .unwrap();
        assert!(claim_private_intake_lease(&storage, Utc::now())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_private_intake_lease_does_not_collide_with_real_paykit_app() {
        let storage = crate::storage::InMemoryStorage::new();
        let app_id = paykit_lib::PaykitAppId::new("paykit-private-intake-supervisor").unwrap();
        let now = Utc::now();
        let app_lease = storage
            .transaction(move |tx| {
                tx.claim_paykit_app_operation(&app_id, now, now + chrono::Duration::minutes(1))
            })
            .await
            .unwrap()
            .unwrap();

        let supervisor = claim_private_intake_lease(&storage, now).await.unwrap();
        assert_eq!(app_lease.lease_id, 0);
        assert_eq!(supervisor.lease_id, 0);
        assert!(storage
            .snapshot()
            .unwrap()
            .paykit_app_operation_leases
            .contains_key(
                &paykit_lib::PaykitAppId::new("paykit-private-intake-supervisor").unwrap()
            ));
        assert!(storage
            .snapshot()
            .unwrap()
            .private_intake_supervisor_lease
            .is_some());
    }

    #[test]
    fn test_backpressured_peer_is_detached_without_starving_sibling_and_resumes() {
        let peers = users(2);
        let mut state = crate::storage::StorageState::default();
        state.private_intake_peers.insert(
            peers[0].clone(),
            crate::storage::PrivateIntakePeerRecord {
                counterparty: peers[0].clone(),
                link_generation: 1,
                buffered_packets: (0..crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER)
                    .map(|receive_counter| BufferedPrivatePacketRecord {
                        counterparty: peers[0].clone(),
                        link_generation: 1,
                        receive_counter: receive_counter as u64,
                        ciphertext: vec![0; crate::storage::PRIVATE_INTAKE_PACKET_BYTES],
                        source_cursor: None,
                        buffered_at: Utc::now(),
                    })
                    .collect(),
                cancelled_handshake_through_generation: None,
            },
        );

        assert_eq!(
            subscribable_private_intake_peers(&state, &peers),
            vec![peers[1].clone()]
        );
        state
            .private_intake_peers
            .get_mut(&peers[0])
            .unwrap()
            .buffered_packets
            .pop();
        assert_eq!(subscribable_private_intake_peers(&state, &peers), peers);
    }

    #[tokio::test]
    async fn test_production_event_seam_routes_owner_and_exact_cursor_to_acquisition() {
        let peer = users(1).remove(0);
        let owner = peer.to_public_key().unwrap();
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![peer.clone()],
        };
        let event = pubky::Event {
            event_type: pubky::EventType::Put {
                content_hash: blake3::Hash::from_bytes([7; 32]),
            },
            resource: format!("pubky://{}/pub/paykit/v0/private/ignored-hint", owner.z32())
                .parse()
                .unwrap(),
            cursor: pubky::EventCursor::new(42),
        };

        let routed = route_private_intake_event(&group, event).unwrap();
        assert_eq!(routed.0, peer);
        assert_eq!(routed.1.subscribed_user, routed.0);
        assert_eq!(routed.1.homeserver, "homeserver-a");
        assert_eq!(routed.1.event_id, 42);
    }

    #[test]
    fn test_production_event_seam_ignores_unsubscribed_owner() {
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: users(1),
        };
        let owner = pubky::Keypair::random().public_key();
        let event = pubky::Event {
            event_type: pubky::EventType::Delete,
            resource: format!("pubky://{}/pub/paykit/v0/private/unrelated", owner.z32())
                .parse()
                .unwrap(),
            cursor: pubky::EventCursor::new(99),
        };

        assert!(route_private_intake_event(&group, event).is_none());
    }

    #[test]
    fn test_event_and_audit_acquisition_follow_contiguous_buffer_tail() {
        let peer = users(1).remove(0);
        let mut state = crate::storage::StorageState::default();
        state.private_intake_peers.insert(
            peer.clone(),
            crate::storage::PrivateIntakePeerRecord {
                counterparty: peer.clone(),
                link_generation: 7,
                buffered_packets: vec![3, 4]
                    .into_iter()
                    .map(|receive_counter| BufferedPrivatePacketRecord {
                        counterparty: peer.clone(),
                        link_generation: 7,
                        receive_counter,
                        ciphertext: vec![
                            receive_counter as u8;
                            crate::storage::PRIVATE_INTAKE_PACKET_BYTES
                        ],
                        source_cursor: None,
                        buffered_at: Utc::now(),
                    })
                    .collect(),
                cancelled_handshake_through_generation: None,
            },
        );

        assert_eq!(next_private_intake_counter(&state, &peer, 7, 3).unwrap(), 5);
        assert_eq!(next_private_intake_counter(&state, &peer, 7, 3).unwrap(), 5);
    }

    #[tokio::test]
    async fn test_startup_audit_catches_up_contiguous_backlog_to_absent() {
        let peer = users(1).remove(0);
        let outcomes = Arc::new(StdMutex::new(vec![
            PrivatePacketAcquisition::Absent,
            PrivatePacketAcquisition::Inserted,
            PrivatePacketAcquisition::Inserted,
        ]));
        let observed = Arc::new(StdMutex::new(Vec::new()));

        let result = audit_private_intake_peer(peer.clone(), {
            let outcomes = Arc::clone(&outcomes);
            let observed = Arc::clone(&observed);
            move |requested, cursor| {
                let outcome = outcomes.lock().unwrap().pop().unwrap();
                observed.lock().unwrap().push((requested, cursor));
                async move { Ok(outcome) }
            }
        })
        .await
        .unwrap();

        assert!(matches!(result, PrivatePacketAcquisition::Absent));
        assert_eq!(observed.lock().unwrap().len(), 3);
        assert!(observed
            .lock()
            .unwrap()
            .iter()
            .all(|(requested, cursor)| requested == &peer && cursor.is_none()));
    }

    #[tokio::test]
    async fn test_production_stream_loop_routes_event_then_stops_on_backpressure() {
        let peer = users(1).remove(0);
        let owner = peer.to_public_key().unwrap();
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![peer.clone()],
        };
        let events = stream::iter([
            Ok(pubky::Event {
                event_type: pubky::EventType::Put {
                    content_hash: blake3::Hash::from_bytes([7; 32]),
                },
                resource: format!("pubky://{}/pub/paykit/v0/private/slot", owner.z32())
                    .parse()
                    .unwrap(),
                cursor: pubky::EventCursor::new(42),
            }),
            Ok(pubky::Event {
                event_type: pubky::EventType::Delete,
                resource: format!("pubky://{}/pub/paykit/v0/private/slot", owner.z32())
                    .parse()
                    .unwrap(),
                cursor: pubky::EventCursor::new(43),
            }),
        ]);
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let (_stop, stop_rx) = watch::channel(false);
        let acquire: PrivateIntakeAcquire = Arc::new({
            let observed = Arc::clone(&observed);
            move |requested, cursor| {
                observed.lock().unwrap().push((requested, cursor));
                Box::pin(async { Ok(PrivatePacketAcquisition::CapacityReached) })
            }
        });
        let audit: PrivateIntakeAudit = Arc::new(|| Box::pin(async {}));

        let result = run_connected_private_intake_stream(
            &group,
            Box::pin(events),
            Duration::from_secs(60),
            Duration::from_secs(60),
            stop_rx,
            &health,
            &work,
            acquire,
            audit,
        )
        .await;

        assert!(matches!(result, PrivateIntakeEventLoopExit::Backpressured));
        let (observed_peer, observed_cursor) = {
            let observed = observed.lock().unwrap();
            assert_eq!(observed.len(), 1);
            (observed[0].0.clone(), observed[0].1.clone())
        };
        assert_eq!(observed_peer, peer);
        assert_eq!(observed_cursor.as_ref().unwrap().event_id, 42);
        assert_eq!(
            health.read().await.peers[&observed_peer],
            PrivateIntakeTransportState::Backpressured
        );
    }
}
