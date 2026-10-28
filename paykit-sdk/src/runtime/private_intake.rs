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

/// Freshness evidence for one supervised Linked Peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateIntakePeerFreshness {
    /// Current SSE transport and reconciliation state.
    pub transport_state: PrivateIntakeTransportState,
    /// Last audit that proved the next expected slot absent.
    pub last_successful_audit_at: Option<DateTime<Utc>>,
}

/// Result of explicitly processing a bounded durable ciphertext batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferedPrivateStreamIntakeReport {
    /// Canonical private-stream persistence result.
    pub report: crate::PrivateStreamIntakeReport,
    /// Durable packets still waiting for this peer.
    pub remaining_buffered_packets: usize,
    /// Supervisor freshness; queue emptiness alone never proves remote absence.
    pub freshness: PrivateIntakePeerFreshness,
}

/// Explicit lifecycle handle for SSE-assisted private intake.
pub struct PrivateIntakeSupervisor {
    pub(super) lease_id: u64,
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

    pub(super) async fn peer_freshness(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<PrivateIntakePeerFreshness> {
        let health = self.health.read().await;
        Some(PrivateIntakePeerFreshness {
            transport_state: *health.peers.get(counterparty)?,
            last_successful_audit_at: health.last_successful_audit_at.get(counterparty).copied(),
        })
    }

    pub(super) fn notify_capacity_released(&self) {
        self.work.notify_waiters();
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
    PendingHandshake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateIntakePeerPhase {
    PendingHandshake { generation: u64 },
    Established,
}

fn private_intake_peer_phase(
    state: &crate::storage::StorageState,
    counterparty: &PubkyPublicKey,
) -> Option<PrivateIntakePeerPhase> {
    let peer = state.linked_peers.get(counterparty)?;
    let link = state.encrypted_link_states.get(counterparty)?;
    match peer.state {
        LinkedPeerState::Linking
            if link.link_snapshot.is_none()
                && link.handshake_snapshot.is_some()
                && link.handshake_role.is_some()
                && state
                    .private_intake_peers
                    .get(counterparty)
                    .and_then(|intake| intake.cancelled_handshake_through_generation)
                    .is_none_or(|cancelled| link.generation > cancelled) =>
        {
            Some(PrivateIntakePeerPhase::PendingHandshake {
                generation: link.generation,
            })
        }
        LinkedPeerState::Linked if link.link_snapshot.is_some() => {
            Some(PrivateIntakePeerPhase::Established)
        }
        _ => None,
    }
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

fn group_private_intake_peers_by_homeserver(
    resolved: Vec<(String, paykit_lib::PublicKey, PubkyPublicKey)>,
) -> Vec<SubscriptionGroup> {
    let mut by_homeserver: HashMap<String, (paykit_lib::PublicKey, Vec<PubkyPublicKey>)> =
        HashMap::new();
    for (homeserver_name, homeserver, peer) in resolved {
        by_homeserver
            .entry(homeserver_name)
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
    groups
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
    peers
        .iter()
        .filter(|peer| {
            matches!(
                private_intake_peer_phase(state, peer),
                Some(PrivateIntakePeerPhase::PendingHandshake { .. })
            ) || (global_count < crate::storage::PRIVATE_INTAKE_MAX_PACKETS_GLOBAL
                && state.private_intake_peers.get(*peer).is_none_or(|record| {
                    record.buffered_packets.len()
                        < crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER
                }))
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
            | PrivatePacketAcquisition::CapacityReached
            | PrivatePacketAcquisition::PendingHandshake => return Ok(outcome),
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
    reason = "expected-slot acquisition keeps routing, generation, cursor, and injected transport explicit"
)]
async fn acquire_expected_private_packet_with_fetch<S, F, Fut>(
    storage: &S,
    counterparty: PubkyPublicKey,
    snapshot: &paykit_lib::EncryptedLinkSnapshot,
    local: &paykit_lib::PublicKey,
    secret: &[u8; 32],
    generation: u64,
    cursor: Option<PrivateIntakeCursorRecord>,
    buffered_at: DateTime<Utc>,
    fetch: F,
) -> Result<PrivatePacketAcquisition>
where
    S: StorageAdapter,
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<Option<Vec<u8>>>>,
{
    let snapshot_counter = snapshot
        .next_private_application_message_resource(local, secret)?
        .1;
    let counterparty_for_tail = counterparty.clone();
    let receive_counter = storage
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
    let (resource, receive_counter) =
        snapshot.private_application_message_resource_at_counter(local, secret, receive_counter)?;
    let Some(ciphertext) = fetch(resource).await? else {
        if let Some(cursor) = cursor {
            storage
                .transaction(move |tx| tx.advance_private_intake_cursor(cursor))
                .await?;
        }
        return Ok(PrivatePacketAcquisition::Absent);
    };
    let packet = BufferedPrivatePacketRecord {
        counterparty,
        link_generation: generation,
        receive_counter,
        ciphertext,
        source_cursor: cursor,
        buffered_at,
    };
    storage
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
                        Some((peer, Ok(PrivatePacketAcquisition::PendingHandshake))) => {
                            work.notify_waiters();
                            set_peer_state(health, peer, PrivateIntakeTransportState::Replaying).await;
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
        let (access, _, peers) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                let state = tx.export_storage_state();
                let mut peers = state
                    .linked_peers
                    .keys()
                    .filter(|peer| private_intake_peer_phase(&state, peer).is_some())
                    .cloned()
                    .collect::<Vec<_>>();
                peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                Ok(peers)
            })
            .await?;
        let access = access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        access.paykit_noise_secret_key()?;
        self.validate_local_noise_key_authorization(&access).await?;
        let mut resolved = Vec::new();
        for peer in peers.expect("active session reads private intake peers") {
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
            resolved.push((homeserver.to_string(), homeserver, peer));
        }
        Ok(group_private_intake_peers_by_homeserver(resolved))
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
            lease_id,
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
                Ok(PrivatePacketAcquisition::PendingHandshake) => {
                    work.notify_waiters();
                    set_peer_state(health, peer.clone(), PrivateIntakeTransportState::Replaying)
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
        let counterparty_for_phase = counterparty.clone();
        let phase = self
            .storage
            .transaction(move |tx| {
                Ok(private_intake_peer_phase(
                    &tx.export_storage_state(),
                    &counterparty_for_phase,
                ))
            })
            .await?;
        if matches!(phase, Some(PrivateIntakePeerPhase::PendingHandshake { .. })) {
            let report = self.advance_link_handshake(counterparty.clone()).await?;
            if report.state == LinkedPeerState::Linking {
                if let Some(cursor) = cursor {
                    self.storage
                        .transaction(move |tx| tx.advance_private_intake_cursor(cursor))
                        .await?;
                }
                return Ok(PrivatePacketAcquisition::PendingHandshake);
            }
        } else if phase.is_none() {
            return Err(PaykitSdkError::RecoveryRequired {
                context: "Linked Peer has no current private intake checkpoint".into(),
                source: None,
            });
        }
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
        let generation = snapshot.state.generation;
        let public_storage = access.outbox_client.public_storage();
        acquire_expected_private_packet_with_fetch(
            &self.storage,
            counterparty,
            &snapshot.link,
            local,
            &secret,
            generation,
            cursor,
            self.clock.now(),
            move |resource| async move {
                let response = match public_storage.get(resource).await {
                    Ok(response) => response,
                    Err(error) if is_pubky_not_found(&error) => return Ok(None),
                    Err(error) => {
                        return Err(map_pubky_transport_error(
                            "fetch expected private intake slot",
                            error,
                        ))
                    }
                };
                read_public_response(
                    response,
                    crate::storage::PRIVATE_INTAKE_PACKET_BYTES,
                    "read expected private intake slot",
                )
                .await
            },
        )
        .await
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
                || *status == pubky::StatusCode::NOT_IMPLEMENTED
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn local_http_response(response: &'static [u8]) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(response).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    fn intake_snapshot(
        local: &pubky::PublicKey,
        counterparty: &pubky::PublicKey,
        read_counter: u32,
    ) -> (paykit_lib::EncryptedLinkSnapshot, [u8; 32]) {
        use pubky_noise::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};

        let secret = [2; 32];
        let state = pubky_noise::serializer::PubkyNoiseSessionState {
            version: pubky_noise::serializer::SESSION_STATE_VERSION,
            phase: NoisePhase::Transport,
            pattern: HandshakePattern::PatternXX,
            initiator: true,
            ephemeral_secret: [1; 32],
            static_secret: Some(secret),
            counter: 3,
            noise_step: NoiseStep::Final,
            sub_step_index: 0,
            handshake_hash: Some([3; 32]),
            link_id: Some([4; 32]),
            sending_nonce: 0,
            receiving_nonce: 0,
            write_counter: 3,
            read_counter,
            endpoint_pubkey: counterparty.to_bytes(),
            handshake_messages: vec![vec![5; 96]],
        };
        let remote_noise = pubky::Keypair::random().public_key();
        let mut bytes = state.serialize();
        bytes.extend_from_slice(&remote_noise.to_bytes());
        bytes.extend_from_slice(&[0; 72]);
        let snapshot = paykit_lib::EncryptedLinkSnapshot::deserialize(&bytes).unwrap();
        // Exercise path derivation now so malformed fixture state fails locally.
        snapshot
            .next_private_application_message_resource(local, &secret)
            .unwrap();
        (snapshot, secret)
    }

    async fn seed_intake_link(
        storage: &crate::storage::InMemoryStorage,
        counterparty: PubkyPublicKey,
        snapshot: &paykit_lib::EncryptedLinkSnapshot,
        generation: u64,
    ) {
        storage
            .transaction(move |tx| {
                tx.save_encrypted_link_state(crate::storage::EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: Some(snapshot.serialize()),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation,
                    checkpointed_at: Utc::now(),
                });
                Ok(())
            })
            .await
            .unwrap();
    }

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
    fn test_private_intake_groups_same_and_cross_homeserver_peers() {
        let homeserver_a = pubky::Keypair::random().public_key();
        let homeserver_b = pubky::Keypair::random().public_key();
        let peers = users(3);

        let groups = group_private_intake_peers_by_homeserver(vec![
            (
                "homeserver-a".into(),
                homeserver_a.clone(),
                peers[0].clone(),
            ),
            ("homeserver-b".into(), homeserver_b, peers[2].clone()),
            ("homeserver-a".into(), homeserver_a, peers[1].clone()),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].homeserver_name, "homeserver-a");
        assert_eq!(groups[0].peers.len(), 2);
        assert_eq!(groups[1].homeserver_name, "homeserver-b");
        assert_eq!(groups[1].peers, vec![peers[2].clone()]);
    }

    #[test]
    fn test_private_intake_same_homeserver_fifty_first_peer_uses_second_stream() {
        let homeserver = pubky::Keypair::random().public_key();
        let resolved = users(51)
            .into_iter()
            .map(|peer| ("homeserver-a".into(), homeserver.clone(), peer))
            .collect();

        let groups = group_private_intake_peers_by_homeserver(resolved);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].peers.len(), 50);
        assert_eq!(groups[1].peers.len(), 1);
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

    #[test]
    fn test_private_intake_treats_not_implemented_event_stream_as_unsupported() {
        let error = pubky::Error::Request(pubky::errors::RequestError::Server {
            status: pubky::StatusCode::NOT_IMPLEMENTED,
            message: "event stream is not implemented".into(),
        });

        assert!(is_unsupported_event_stream(&error));
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

    #[tokio::test]
    async fn test_private_intake_stop_marks_every_peer_stopped() {
        let peers = users(2);
        let (stop, _) = watch::channel(false);
        let supervisor = PrivateIntakeSupervisor {
            lease_id: 1,
            stop,
            tasks: StdMutex::new(Vec::new()),
            health: Arc::new(AsyncRwLock::new(PrivateIntakeHealth {
                peers: HashMap::from([
                    (peers[0].clone(), PrivateIntakeTransportState::CaughtUp),
                    (peers[1].clone(), PrivateIntakeTransportState::Stale),
                ]),
                reserved_streams: 2,
                last_successful_audit_at: HashMap::new(),
            })),
            work: Arc::new(Notify::new()),
        };

        supervisor.stop().await;

        assert!(supervisor
            .health()
            .await
            .peers
            .values()
            .all(|state| *state == PrivateIntakeTransportState::Stopped));
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

    #[tokio::test]
    async fn test_expected_slot_fetch_derives_exact_resource_and_atomically_buffers_cursor() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let expected = snapshot
            .private_application_message_resource_at_counter(&local, &secret, 3)
            .unwrap()
            .0;
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let cursor = PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-a".into(),
            event_id: 42,
        };

        let outcome = acquire_expected_private_packet_with_fetch(
            &storage,
            peer.clone(),
            &snapshot,
            &local,
            &secret,
            7,
            Some(cursor.clone()),
            Utc::now(),
            {
                let observed = Arc::clone(&observed);
                move |resource| {
                    observed.lock().unwrap().push(resource);
                    async { Ok(Some(vec![9; crate::storage::PRIVATE_INTAKE_PACKET_BYTES])) }
                }
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, PrivatePacketAcquisition::Inserted));
        assert_eq!(*observed.lock().unwrap(), vec![expected]);
        let state = storage.snapshot().unwrap();
        assert_eq!(state.private_intake_cursors[&peer], cursor);
        let packet = &state.private_intake_peers[&peer].buffered_packets[0];
        assert_eq!(packet.receive_counter, 3);
        assert_eq!(
            packet.ciphertext,
            vec![9; crate::storage::PRIVATE_INTAKE_PACKET_BYTES]
        );
    }

    #[tokio::test]
    async fn test_expected_slot_fetch_grows_contiguous_tail_without_noise_advance() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let expected = [3_u64, 4].map(|counter| {
            snapshot
                .private_application_message_resource_at_counter(&local, &secret, counter)
                .unwrap()
                .0
        });
        let observed = Arc::new(StdMutex::new(Vec::new()));

        for byte in [3_u8, 4] {
            acquire_expected_private_packet_with_fetch(
                &storage,
                peer.clone(),
                &snapshot,
                &local,
                &secret,
                7,
                None,
                Utc::now(),
                {
                    let observed = Arc::clone(&observed);
                    move |resource| {
                        observed.lock().unwrap().push(resource);
                        async move {
                            Ok(Some(vec![
                                byte;
                                crate::storage::PRIVATE_INTAKE_PACKET_BYTES
                            ]))
                        }
                    }
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(*observed.lock().unwrap(), expected);
        let state = storage.snapshot().unwrap();
        assert_eq!(
            state.private_intake_peers[&peer]
                .buffered_packets
                .iter()
                .map(|packet| packet.receive_counter)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(
            paykit_lib::EncryptedLinkSnapshot::deserialize(
                state.encrypted_link_states[&peer]
                    .link_snapshot
                    .as_ref()
                    .unwrap()
            )
            .unwrap()
            .next_private_application_message_resource(&local, &secret)
            .unwrap()
            .1,
            3
        );
    }

    #[tokio::test]
    async fn test_expected_slot_fetch_failure_leaves_cursor_for_replay() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let cursor = PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-a".into(),
            event_id: 42,
        };

        let result = acquire_expected_private_packet_with_fetch(
            &storage,
            peer.clone(),
            &snapshot,
            &local,
            &secret,
            7,
            Some(cursor),
            Utc::now(),
            |_| async {
                Err(PaykitSdkError::Transport {
                    context: "offline expected-slot GET".into(),
                    source: None,
                })
            },
        )
        .await;
        assert!(result.is_err());
        let state = storage.snapshot().unwrap();
        assert!(!state.private_intake_cursors.contains_key(&peer));
        assert!(!state.private_intake_peers.contains_key(&peer));
    }

    #[tokio::test]
    async fn test_expected_slot_oversize_response_leaves_cursor_for_replay() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let cursor = PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-a".into(),
            event_id: 42,
        };
        let response = local_http_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n",
        )
        .await;

        let result = acquire_expected_private_packet_with_fetch(
            &storage,
            peer.clone(),
            &snapshot,
            &local,
            &secret,
            7,
            Some(cursor),
            Utc::now(),
            |_| async move {
                read_public_response(
                    response,
                    crate::storage::PRIVATE_INTAKE_PACKET_BYTES,
                    "read expected private intake slot",
                )
                .await
            },
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
        let state = storage.snapshot().unwrap();
        assert!(!state.private_intake_cursors.contains_key(&peer));
        assert!(!state.private_intake_peers.contains_key(&peer));
    }

    #[tokio::test]
    async fn test_expected_slot_cursor_commit_failure_rolls_back_packet_for_replay() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let old_cursor = PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-a".into(),
            event_id: 41,
        };
        storage
            .transaction({
                let old_cursor = old_cursor.clone();
                move |tx| tx.advance_private_intake_cursor(old_cursor)
            })
            .await
            .unwrap();
        let moved_cursor = PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-b".into(),
            event_id: 42,
        };

        let result = acquire_expected_private_packet_with_fetch(
            &storage,
            peer.clone(),
            &snapshot,
            &local,
            &secret,
            7,
            Some(moved_cursor),
            Utc::now(),
            |_| async { Ok(Some(vec![9; crate::storage::PRIVATE_INTAKE_PACKET_BYTES])) },
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Storage { .. })));
        let state = storage.snapshot().unwrap();
        assert_eq!(state.private_intake_cursors[&peer], old_cursor);
        assert!(!state.private_intake_peers.contains_key(&peer));
    }

    #[tokio::test]
    async fn test_absent_expected_slot_commits_replay_cursor_without_buffer() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let cursor = PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-a".into(),
            event_id: 42,
        };

        let outcome = acquire_expected_private_packet_with_fetch(
            &storage,
            peer.clone(),
            &snapshot,
            &local,
            &secret,
            7,
            Some(cursor.clone()),
            Utc::now(),
            |_| async { Ok(None) },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, PrivatePacketAcquisition::Absent));
        let state = storage.snapshot().unwrap();
        assert_eq!(state.private_intake_cursors[&peer], cursor);
        assert!(!state.private_intake_peers.contains_key(&peer));
    }

    #[tokio::test]
    async fn test_connected_stream_event_runs_exact_fetch_and_atomic_storage_path() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let expected_resource = snapshot
            .private_application_message_resource_at_counter(&local, &secret, 3)
            .unwrap()
            .0;
        let snapshot_bytes = snapshot.serialize();
        let observed_resources = Arc::new(StdMutex::new(Vec::new()));
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![peer.clone()],
        };
        let event = pubky::Event {
            event_type: pubky::EventType::Put {
                content_hash: blake3::Hash::from_bytes([7; 32]),
            },
            resource: format!(
                "pubky://{}/pub/paykit/v0/private/untrusted-hint",
                remote.z32()
            )
            .parse()
            .unwrap(),
            cursor: pubky::EventCursor::new(42),
        };
        let acquire: PrivateIntakeAcquire = Arc::new({
            let storage = storage.clone();
            let local = local.clone();
            let snapshot_bytes = snapshot_bytes.clone();
            let observed_resources = Arc::clone(&observed_resources);
            move |requested_peer, cursor| {
                let storage = storage.clone();
                let local = local.clone();
                let snapshot_bytes = snapshot_bytes.clone();
                let observed_resources = Arc::clone(&observed_resources);
                Box::pin(async move {
                    let snapshot = paykit_lib::EncryptedLinkSnapshot::deserialize(&snapshot_bytes)?;
                    acquire_expected_private_packet_with_fetch(
                        &storage,
                        requested_peer,
                        &snapshot,
                        &local,
                        &secret,
                        7,
                        cursor,
                        Utc::now(),
                        move |resource| {
                            observed_resources.lock().unwrap().push(resource);
                            async { Ok(Some(vec![9; crate::storage::PRIVATE_INTAKE_PACKET_BYTES])) }
                        },
                    )
                    .await
                })
            }
        });
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let (_stop, stop_rx) = watch::channel(false);

        let exit = run_connected_private_intake_stream(
            &group,
            Box::pin(stream::iter([Ok(event)])),
            Duration::from_secs(60),
            Duration::from_secs(60),
            stop_rx,
            &health,
            &work,
            acquire,
            Arc::new(|| Box::pin(async {})),
        )
        .await;

        assert!(matches!(exit, PrivateIntakeEventLoopExit::Ended));
        assert_eq!(*observed_resources.lock().unwrap(), vec![expected_resource]);
        let state = storage.snapshot().unwrap();
        assert_eq!(state.private_intake_cursors[&peer].event_id, 42);
        assert_eq!(
            state.private_intake_peers[&peer].buffered_packets[0].receive_counter,
            3
        );
    }

    #[tokio::test]
    async fn test_startup_audit_runs_storage_tail_and_exact_fetch_until_absent() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let fetched = Arc::new(StdMutex::new(0_usize));
        let snapshot_bytes = snapshot.serialize();

        let outcome = audit_private_intake_peer(peer.clone(), {
            let storage = storage.clone();
            let fetched = Arc::clone(&fetched);
            let local = local.clone();
            move |requested_peer, cursor| {
                let storage = storage.clone();
                let fetched = Arc::clone(&fetched);
                let local = local.clone();
                let snapshot_bytes = snapshot_bytes.clone();
                async move {
                    let snapshot = paykit_lib::EncryptedLinkSnapshot::deserialize(&snapshot_bytes)?;
                    acquire_expected_private_packet_with_fetch(
                        &storage,
                        requested_peer,
                        &snapshot,
                        &local,
                        &secret,
                        7,
                        cursor,
                        Utc::now(),
                        move |_| {
                            let attempt = {
                                let mut fetched = fetched.lock().unwrap();
                                let attempt = *fetched;
                                *fetched += 1;
                                attempt
                            };
                            async move {
                                Ok((attempt < 2).then(|| {
                                    vec![attempt as u8; crate::storage::PRIVATE_INTAKE_PACKET_BYTES]
                                }))
                            }
                        },
                    )
                    .await
                }
            }
        })
        .await
        .unwrap();

        assert!(matches!(outcome, PrivatePacketAcquisition::Absent));
        assert_eq!(*fetched.lock().unwrap(), 3);
        assert_eq!(
            storage.snapshot().unwrap().private_intake_peers[&peer]
                .buffered_packets
                .iter()
                .map(|packet| packet.receive_counter)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[tokio::test]
    async fn test_grouped_stream_detaches_full_peer_keeps_sibling_and_resumes_after_drain() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remotes = [
            pubky::Keypair::random().public_key(),
            pubky::Keypair::random().public_key(),
        ];
        let peers = remotes
            .iter()
            .map(PubkyPublicKey::from_public_key)
            .collect::<Vec<_>>();
        let (full_snapshot, secret) = intake_snapshot(&local, &remotes[0], 3);
        let (sibling_snapshot, _) = intake_snapshot(&local, &remotes[1], 3);
        seed_intake_link(&storage, peers[0].clone(), &full_snapshot, 7).await;
        seed_intake_link(&storage, peers[1].clone(), &sibling_snapshot, 7).await;
        storage
            .transaction({
                let full_peer = peers[0].clone();
                move |tx| {
                    for receive_counter in
                        3..3 + u64::try_from(crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER)
                            .unwrap()
                    {
                        assert!(matches!(
                            tx.buffer_private_intake_packet(BufferedPrivatePacketRecord {
                                counterparty: full_peer.clone(),
                                link_generation: 7,
                                receive_counter,
                                ciphertext: vec![
                                    receive_counter as u8;
                                    crate::storage::PRIVATE_INTAKE_PACKET_BYTES
                                ],
                                source_cursor: None,
                                buffered_at: Utc::now(),
                            })?,
                            PrivateIntakeBufferOutcome::Inserted
                        ));
                    }
                    Ok(())
                }
            })
            .await
            .unwrap();
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: peers.clone(),
        };
        let fetches = Arc::new(StdMutex::new(HashMap::<PubkyPublicKey, usize>::new()));
        let acquire: PrivateIntakeAcquire = Arc::new({
            let storage = storage.clone();
            let local = local.clone();
            let fetches = Arc::clone(&fetches);
            move |peer, cursor| {
                let storage = storage.clone();
                let local = local.clone();
                let fetches = Arc::clone(&fetches);
                Box::pin(async move {
                    let snapshot_bytes = storage.snapshot().unwrap().encrypted_link_states[&peer]
                        .link_snapshot
                        .clone()
                        .unwrap();
                    let snapshot = paykit_lib::EncryptedLinkSnapshot::deserialize(&snapshot_bytes)?;
                    let fetched_peer = peer.clone();
                    acquire_expected_private_packet_with_fetch(
                        &storage,
                        peer,
                        &snapshot,
                        &local,
                        &secret,
                        7,
                        cursor,
                        Utc::now(),
                        move |_| {
                            *fetches.lock().unwrap().entry(fetched_peer).or_default() += 1;
                            async { Ok(Some(vec![9; crate::storage::PRIVATE_INTAKE_PACKET_BYTES])) }
                        },
                    )
                    .await
                })
            }
        });
        let event = |owner: &pubky::PublicKey, event_id| pubky::Event {
            event_type: pubky::EventType::Put {
                content_hash: blake3::Hash::from_bytes([7; 32]),
            },
            resource: format!(
                "pubky://{}/pub/paykit/v0/private/untrusted-hint",
                owner.z32()
            )
            .parse()
            .unwrap(),
            cursor: pubky::EventCursor::new(event_id),
        };
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let (_stop, stop_rx) = watch::channel(false);

        let first_exit = run_connected_private_intake_stream(
            &group,
            Box::pin(stream::iter([
                Ok(event(&remotes[0], 42)),
                Ok(event(&remotes[1], 43)),
            ])),
            Duration::from_secs(60),
            Duration::from_secs(60),
            stop_rx,
            &health,
            &work,
            Arc::clone(&acquire),
            Arc::new(|| Box::pin(async {})),
        )
        .await;
        assert!(matches!(
            first_exit,
            PrivateIntakeEventLoopExit::Backpressured
        ));
        assert_eq!(fetches.lock().unwrap().get(&peers[0]), Some(&1));
        assert!(!fetches.lock().unwrap().contains_key(&peers[1]));
        assert!(!storage
            .snapshot()
            .unwrap()
            .private_intake_cursors
            .contains_key(&peers[0]));

        let active = subscribable_private_intake_peers(&storage.snapshot().unwrap(), &peers);
        assert_eq!(active, vec![peers[1].clone()]);
        let sibling_group = SubscriptionGroup {
            peers: active,
            ..group.clone()
        };
        let (_stop, stop_rx) = watch::channel(false);
        let sibling_exit = run_connected_private_intake_stream(
            &sibling_group,
            Box::pin(stream::iter([Ok(event(&remotes[1], 43))])),
            Duration::from_secs(60),
            Duration::from_secs(60),
            stop_rx,
            &health,
            &work,
            Arc::clone(&acquire),
            Arc::new(|| Box::pin(async {})),
        )
        .await;
        assert!(matches!(sibling_exit, PrivateIntakeEventLoopExit::Ended));
        assert_eq!(fetches.lock().unwrap().get(&peers[1]), Some(&1));
        assert_eq!(
            storage.snapshot().unwrap().private_intake_cursors[&peers[1]].event_id,
            43
        );

        let (advanced_snapshot, _) = intake_snapshot(&local, &remotes[0], 4);
        storage
            .transaction({
                let full_peer = peers[0].clone();
                move |tx| {
                    tx.remove_buffered_private_intake_packet(&full_peer, 7, 3)?;
                    tx.save_encrypted_link_state(crate::storage::EncryptedLinkStateRecord {
                        counterparty: full_peer,
                        link_snapshot: Some(advanced_snapshot.serialize()),
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: 7,
                        checkpointed_at: Utc::now(),
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();
        assert_eq!(
            subscribable_private_intake_peers(&storage.snapshot().unwrap(), &peers),
            peers
        );
        let (_stop, stop_rx) = watch::channel(false);
        let resumed_exit = run_connected_private_intake_stream(
            &group,
            Box::pin(stream::iter([Ok(event(&remotes[0], 44))])),
            Duration::from_secs(60),
            Duration::from_secs(60),
            stop_rx,
            &health,
            &work,
            acquire,
            Arc::new(|| Box::pin(async {})),
        )
        .await;
        assert!(matches!(resumed_exit, PrivateIntakeEventLoopExit::Ended));
        assert_eq!(fetches.lock().unwrap().get(&peers[0]), Some(&2));
        let state = storage.snapshot().unwrap();
        assert_eq!(state.private_intake_cursors[&peers[0]].event_id, 44);
        assert_eq!(
            state.private_intake_peers[&peers[0]]
                .buffered_packets
                .last()
                .unwrap()
                .receive_counter,
            3 + u64::try_from(crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER).unwrap()
        );
    }

    #[test]
    fn test_pending_handshake_is_supervised_without_expiry() {
        for role in [
            EncryptedLinkHandshakeRole::Initiator,
            EncryptedLinkHandshakeRole::Responder,
        ] {
            let peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
            let mut state = crate::storage::StorageState::default();
            state.linked_peers.insert(
                peer.clone(),
                LinkedPeerRecord {
                    counterparty: peer.clone(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(Utc::now() - chrono::Duration::days(365)),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                    noise_key_authorization: None,
                },
            );
            state.encrypted_link_states.insert(
                peer.clone(),
                crate::storage::EncryptedLinkStateRecord {
                    counterparty: peer.clone(),
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(role),
                    generation: 7,
                    checkpointed_at: Utc::now() - chrono::Duration::days(365),
                },
            );

            assert_eq!(
                private_intake_peer_phase(&state, &peer),
                Some(PrivateIntakePeerPhase::PendingHandshake { generation: 7 })
            );
            state.private_intake_peers.insert(
                peer.clone(),
                crate::storage::PrivateIntakePeerRecord {
                    counterparty: peer.clone(),
                    link_generation: 8,
                    buffered_packets: Vec::new(),
                    cancelled_handshake_through_generation: Some(7),
                },
            );
            assert_eq!(private_intake_peer_phase(&state, &peer), None);
        }
    }
}
