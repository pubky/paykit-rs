use super::*;
use crate::storage::{
    BufferedPrivatePacketRecord, PrivateIntakeBufferOutcome, PrivateIntakeCursorRecord,
    PrivateIntakeSupervisorLease,
};
use futures_util::StreamExt;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{watch, Mutex as AsyncMutex, Notify, RwLock as AsyncRwLock};

const MAX_EVENT_STREAM_USERS: usize = crate::storage::PRIVATE_INTAKE_MAX_EVENT_STREAM_USERS;
const DEFAULT_MAX_EVENT_BYTES: usize = 4096;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_STREAM_CAPACITY: usize = 1024;
const DEFAULT_HEALTHY_AUDIT_INTERVAL: Duration = Duration::from_secs(60);
const MIN_HEALTHY_AUDIT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_HEALTHY_AUDIT_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_AUDIT_JITTER: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const TOPOLOGY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
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
    tasks: AsyncMutex<Vec<tokio::task::JoinHandle<()>>>,
    task_handles: Vec<tokio::task::AbortHandle>,
    health: Arc<AsyncRwLock<PrivateIntakeHealth>>,
    work: Arc<Notify>,
    capacity_released: Arc<Notify>,
}

#[derive(Clone)]
struct PrivateIntakeNotifications {
    work: Arc<Notify>,
    capacity_released: Arc<Notify>,
    topology_changed: Arc<Notify>,
}

impl PrivateIntakeSupervisor {
    /// Return whether every task owned by this supervisor has terminated.
    ///
    /// This reports task termination, including autonomous lease or topology
    /// failure, rather than transport health. It remains false while an
    /// explicit [`Self::stop`] waits for any live task.
    pub fn is_finished(&self) -> bool {
        self.task_handles
            .iter()
            .all(tokio::task::AbortHandle::is_finished)
    }

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
        self.capacity_released.notify_one();
    }

    /// Stop all streams and wait for their cancellation-safe exits.
    pub async fn stop(&self) {
        let _ = self.stop.send(true);
        let mut tasks = self.tasks.lock().await;
        for task in tasks.iter_mut() {
            let _ = task.await;
        }
        tasks.clear();
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
        self.tasks.get_mut().clear();
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SubscriptionGroup {
    homeserver: paykit_lib::PublicKey,
    homeserver_name: String,
    peers: Vec<PubkyPublicKey>,
}

fn active_subscription_group(
    group: &SubscriptionGroup,
    peers: Vec<PubkyPublicKey>,
) -> SubscriptionGroup {
    SubscriptionGroup {
        homeserver: group.homeserver.clone(),
        homeserver_name: group.homeserver_name.clone(),
        peers,
    }
}

enum PrivatePacketAcquisition {
    Absent,
    Inserted,
    InsertedAtCapacity,
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

#[cfg(test)]
fn reserve_private_intake_admission(
    admitted_homeservers: &mut HashMap<PubkyPublicKey, String>,
    counterparty: PubkyPublicKey,
    homeserver: String,
    capacity: usize,
) -> Result<()> {
    let mut proposed = admitted_homeservers.clone();
    proposed.insert(counterparty, homeserver);
    reserve_stream_capacity(
        capacity,
        crate::storage::private_intake_stream_count(&proposed),
    )?;
    *admitted_homeservers = proposed;
    Ok(())
}

fn admitted_homeservers(groups: &[SubscriptionGroup]) -> HashMap<PubkyPublicKey, String> {
    groups
        .iter()
        .flat_map(|group| {
            group
                .peers
                .iter()
                .cloned()
                .map(|peer| (peer, group.homeserver_name.clone()))
        })
        .collect()
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
            | PrivatePacketAcquisition::InsertedAtCapacity
            | PrivatePacketAcquisition::AlreadyBuffered
            | PrivatePacketAcquisition::CapacityReached
            | PrivatePacketAcquisition::PendingHandshake => return Ok(outcome),
        }
    }
    Ok(PrivatePacketAcquisition::Inserted)
}

#[derive(Debug)]
enum PrivateIntakeEventLoopExit {
    Stopped,
    Ended,
    Backpressured,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateIntakeTopologyTrigger {
    Stopped,
    Refresh,
    CapacityReleased,
    TopologyChanged,
}

async fn wait_private_intake_topology_trigger(
    refresh: &mut tokio::time::Interval,
    capacity_released: &Notify,
    topology_changed: &Notify,
    stop: &mut watch::Receiver<bool>,
) -> PrivateIntakeTopologyTrigger {
    tokio::select! {
        changed = stop.changed() => {
            if changed.is_err() || *stop.borrow() {
                PrivateIntakeTopologyTrigger::Stopped
            } else {
                PrivateIntakeTopologyTrigger::Refresh
            }
        }
        _ = refresh.tick() => PrivateIntakeTopologyTrigger::Refresh,
        _ = capacity_released.notified() => PrivateIntakeTopologyTrigger::CapacityReleased,
        _ = topology_changed.notified() => PrivateIntakeTopologyTrigger::TopologyChanged,
    }
}

type PrivateIntakeAcquisitionFuture =
    Pin<Box<dyn Future<Output = Result<PrivatePacketAcquisition>> + Send>>;
type PrivateIntakeAcquire = Arc<
    dyn Fn(PubkyPublicKey, Option<PrivateIntakeCursorRecord>) -> PrivateIntakeAcquisitionFuture
        + Send
        + Sync,
>;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PrivateIntakeAuditOutcome {
    handshake_pending: bool,
    backpressured: bool,
}

type PrivateIntakeAuditFuture = Pin<Box<dyn Future<Output = PrivateIntakeAuditOutcome> + Send>>;
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
        .transaction(move |tx| {
            let counterparty = packet.counterparty.clone();
            let outcome = tx.buffer_private_intake_packet(packet)?;
            let at_capacity = matches!(outcome, PrivateIntakeBufferOutcome::Inserted)
                && subscribable_private_intake_peers(
                    &tx.export_storage_state(),
                    std::slice::from_ref(&counterparty),
                )
                .is_empty();
            Ok(match outcome {
                PrivateIntakeBufferOutcome::Inserted if at_capacity => {
                    PrivatePacketAcquisition::InsertedAtCapacity
                }
                PrivateIntakeBufferOutcome::Inserted => PrivatePacketAcquisition::Inserted,
                PrivateIntakeBufferOutcome::AlreadyBuffered => {
                    PrivatePacketAcquisition::AlreadyBuffered
                }
                PrivateIntakeBufferOutcome::CapacityReached => {
                    PrivatePacketAcquisition::CapacityReached
                }
            })
        })
        .await
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
    mut handshake_pending: bool,
) -> PrivateIntakeEventLoopExit {
    let mut audit =
        tokio::time::interval_at(tokio::time::Instant::now() + audit_delay, audit_interval);
    audit.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut handshake_retry = tokio::time::interval_at(
        tokio::time::Instant::now() + RECONNECT_DELAY,
        RECONNECT_DELAY,
    );
    handshake_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return PrivateIntakeEventLoopExit::Stopped;
                }
            }
            _ = audit.tick() => {
                let outcome = audit_group().await;
                if outcome.backpressured {
                    return PrivateIntakeEventLoopExit::Backpressured;
                }
                handshake_pending = outcome.handshake_pending;
            }
            _ = handshake_retry.tick(), if handshake_pending => {
                let outcome = audit_group().await;
                if outcome.backpressured {
                    return PrivateIntakeEventLoopExit::Backpressured;
                }
                handshake_pending = outcome.handshake_pending;
            }
            event = stream.next() => match event {
                Some(Ok(event)) => {
                    let mut acquire_one = |peer, cursor| acquire(peer, cursor);
                    match process_private_intake_event(group, event, &mut acquire_one).await {
                        Some((peer, Ok(PrivatePacketAcquisition::Inserted))) => {
                            work.notify_waiters();
                            set_peer_state(health, peer, PrivateIntakeTransportState::Replaying).await;
                        }
                        Some((peer, Ok(PrivatePacketAcquisition::InsertedAtCapacity))) => {
                            work.notify_waiters();
                            set_peer_state(health, peer, PrivateIntakeTransportState::Backpressured).await;
                            return PrivateIntakeEventLoopExit::Backpressured;
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
                            handshake_pending = true;
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
    stream_capacity: usize,
    admitted_homeservers: HashMap<PubkyPublicKey, String>,
) -> Result<PrivateIntakeSupervisorLease> {
    storage
        .transaction(move |tx| {
            tx.claim_private_intake_supervisor(
                now,
                now + SUPERVISOR_LEASE_TIMEOUT,
                stream_capacity,
                admitted_homeservers,
            )?
            .ok_or_else(|| PaykitSdkError::Policy {
                context: "private intake supervisor is already running for this identity".into(),
                source: None,
            })
        })
        .await
}

async fn renew_private_intake_lease<S: StorageAdapter, C: Clock>(
    storage: &S,
    clock: &C,
    lease_id: u64,
) -> Result<Option<DateTime<Utc>>> {
    let clock = clock.clone();
    storage
        .transaction(move |tx| {
            let now = clock.now();
            let expires_at = now + SUPERVISOR_LEASE_TIMEOUT;
            Ok(tx
                .renew_private_intake_supervisor(lease_id, now, expires_at)
                .then_some(expires_at))
        })
        .await
}

async fn renew_private_intake_lease_or_stop<S: StorageAdapter, C: Clock>(
    storage: &S,
    clock: &C,
    lease_id: u64,
    confirmed_expires_at: DateTime<Utc>,
    stop: &watch::Sender<bool>,
) -> Option<DateTime<Utc>> {
    let mut stopped = stop.subscribe();
    loop {
        match renew_private_intake_lease(storage, clock, lease_id).await {
            Ok(Some(expires_at)) => return Some(expires_at),
            Ok(None) => break,
            Err(error)
                if is_transient_private_intake_storage_error(&error)
                    && clock.now()
                        < confirmed_expires_at
                            - chrono::Duration::from_std(SUPERVISOR_LEASE_RENEW_INTERVAL)
                                .expect("renew interval fits chrono duration") =>
            {
                if wait_reconnect_or_stop(&mut stopped).await {
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    let _ = stop.send(true);
    None
}

fn is_transient_private_intake_storage_error(error: &PaykitSdkError) -> bool {
    matches!(
        error,
        PaykitSdkError::SharedStateBusy { .. }
            | PaykitSdkError::ConcurrentUpdate { .. }
            | PaykitSdkError::Transport { .. }
    )
}

async fn replace_private_intake_admissions<S: StorageAdapter, C: Clock>(
    storage: &S,
    clock: &C,
    lease_id: u64,
    admitted_homeservers: HashMap<PubkyPublicKey, String>,
) -> Result<bool> {
    let clock = clock.clone();
    storage
        .transaction(move |tx| {
            tx.replace_private_intake_admissions(lease_id, clock.now(), admitted_homeservers)
        })
        .await
}

async fn replace_private_intake_admissions_with_retry<S: StorageAdapter, C: Clock>(
    storage: &S,
    clock: &C,
    lease_id: u64,
    admitted_homeservers: HashMap<PubkyPublicKey, String>,
    stop: &mut watch::Receiver<bool>,
) -> Result<bool> {
    loop {
        match replace_private_intake_admissions(
            storage,
            clock,
            lease_id,
            admitted_homeservers.clone(),
        )
        .await
        {
            Err(error) if is_transient_private_intake_storage_error(&error) => {
                if wait_reconnect_or_stop(stop).await {
                    return Ok(false);
                }
            }
            result => return result,
        }
    }
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
        let lease = claim_private_intake_lease(
            &self.storage,
            self.clock.now(),
            config.stream_capacity,
            admitted_homeservers(&groups),
        )
        .await?;
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
        let capacity_released = Arc::new(Notify::new());
        let notifications = PrivateIntakeNotifications {
            work: Arc::clone(&work),
            capacity_released: Arc::clone(&capacity_released),
            topology_changed: Arc::clone(&self.private_intake_topology_changed),
        };
        let (stop, _) = watch::channel(false);
        let mut tasks = Vec::with_capacity(groups.len() + 1);
        {
            let sdk = Arc::clone(self);
            let mut stop_rx = stop.subscribe();
            let stop_tx = stop.clone();
            tasks.push(tokio::spawn(async move {
                let mut confirmed_expires_at = lease.expires_at;
                let mut renewal = tokio::time::interval(SUPERVISOR_LEASE_RENEW_INTERVAL);
                renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                renewal.tick().await;
                loop {
                    tokio::select! {
                        changed = stop_rx.changed() => {
                            if changed.is_err() || *stop_rx.borrow() { break; }
                        }
                        _ = renewal.tick() => {
                            let renewed = renew_private_intake_lease_or_stop(
                                &sdk.storage,
                                &sdk.clock,
                                lease_id,
                                confirmed_expires_at,
                                &stop_tx,
                            ).await;
                            let Some(expires_at) = renewed else { return; };
                            confirmed_expires_at = expires_at;
                        }
                    }
                }
            }));
        }
        {
            let sdk = Arc::clone(self);
            let stop_rx = stop.subscribe();
            let health = Arc::clone(&health);
            tasks.push(tokio::spawn(async move {
                sdk.clone()
                    .run_private_intake_topology(
                        groups,
                        config,
                        lease_id,
                        stop_rx,
                        health,
                        notifications,
                    )
                    .await;
                let _ = release_private_intake_lease(&sdk.storage, lease_id).await;
            }));
        }
        let task_handles = tasks
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect();
        Ok(PrivateIntakeSupervisor {
            lease_id,
            stop,
            tasks: AsyncMutex::new(tasks),
            task_handles,
            health,
            work,
            capacity_released,
        })
    }

    async fn run_private_intake_topology(
        self: Arc<Self>,
        mut groups: Vec<SubscriptionGroup>,
        config: PrivateIntakeConfig,
        lease_id: u64,
        mut stop: watch::Receiver<bool>,
        health: Arc<AsyncRwLock<PrivateIntakeHealth>>,
        notifications: PrivateIntakeNotifications,
    ) {
        let mut topology_refresh = tokio::time::interval(TOPOLOGY_REFRESH_INTERVAL);
        topology_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        topology_refresh.tick().await;
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
                let work = Arc::clone(&notifications.work);
                let topology_changed = Arc::clone(&notifications.topology_changed);
                let stop_rx = epoch_stop.subscribe();
                // Avoid another async layer: its nested future layout exceeds rustc's query depth.
                handles.push(tokio::spawn(sdk.run_private_intake_group(
                    group,
                    config,
                    stop_rx,
                    health,
                    work,
                    topology_changed,
                )));
            }

            let next_groups = loop {
                let trigger = wait_private_intake_topology_trigger(
                    &mut topology_refresh,
                    &notifications.capacity_released,
                    &notifications.topology_changed,
                    &mut stop,
                )
                .await;
                match trigger {
                    PrivateIntakeTopologyTrigger::Stopped => break None,
                    PrivateIntakeTopologyTrigger::CapacityReleased => {
                        break Some(groups.clone());
                    }
                    PrivateIntakeTopologyTrigger::Refresh
                    | PrivateIntakeTopologyTrigger::TopologyChanged => {
                        match self.resolve_private_intake_groups().await {
                            Ok(resolved) if resolved != groups => {
                                let admissions = admitted_homeservers(&resolved);
                                for group in &resolved {
                                    set_group_state(
                                        &health,
                                        group,
                                        PrivateIntakeTransportState::Stale,
                                    )
                                    .await;
                                }
                                let replaced = replace_private_intake_admissions_with_retry(
                                    &self.storage,
                                    &self.clock,
                                    lease_id,
                                    admissions,
                                    &mut stop,
                                )
                                .await;
                                match replaced {
                                    Ok(true) => break Some(resolved),
                                    Ok(false) | Err(_) => {
                                        for group in &resolved {
                                            set_group_state(
                                                &health,
                                                group,
                                                PrivateIntakeTransportState::Stale,
                                            )
                                            .await;
                                        }
                                        break None;
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(_) => {
                                for group in &groups {
                                    set_group_state(
                                        &health,
                                        group,
                                        PrivateIntakeTransportState::Stale,
                                    )
                                    .await;
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
        topology_changed: Arc<Notify>,
    ) {
        loop {
            if *stop.borrow() {
                break;
            }
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
            let active_group = active_subscription_group(&group, active_peers);
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
            let client = access.outbox_client.client().clone();
            drop(access);
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
            let builder =
                match pubky::EventStreamBuilder::for_homeserver(client, &active_group.homeserver)
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
                    let (state, retry_after, audit_outcome) =
                        audit_disconnected_private_intake_stream(
                            &error,
                            config.healthy_audit_interval,
                            || self.audit_private_intake_group(&active_group, &health, &work),
                        )
                        .await;
                    if audit_outcome.backpressured {
                        topology_changed.notify_one();
                    } else {
                        set_group_state(&health, &active_group, state).await;
                    }
                    if state == PrivateIntakeTransportState::Stale {
                        topology_changed.notify_one();
                    }
                    if wait_retry_or_stop(&mut stop, retry_after).await {
                        break;
                    }
                    continue;
                }
            };
            // Establish live delivery before startup audit. Pending handshakes can
            // publish their next packet during audit; subscribing first prevents
            // that packet from falling into the live-stream setup gap.
            let audit_outcome = self
                .audit_private_intake_group(&active_group, &health, &work)
                .await;
            if audit_outcome.backpressured {
                topology_changed.notify_one();
                if wait_reconnect_or_stop(&mut stop).await {
                    break;
                }
                continue;
            }
            let acquire: PrivateIntakeAcquire = {
                let sdk = Arc::clone(&self);
                Arc::new(move |peer, cursor| {
                    let sdk = Arc::clone(&sdk);
                    Box::pin(async move { sdk.acquire_expected_private_packet(peer, cursor).await })
                })
            };
            let audit_group: PrivateIntakeAudit = {
                let sdk = Arc::clone(&self);
                let group = active_group.clone();
                let health = Arc::clone(&health);
                let work = Arc::clone(&work);
                Arc::new(move || {
                    let sdk = Arc::clone(&sdk);
                    let group = group.clone();
                    let health = Arc::clone(&health);
                    let work = Arc::clone(&work);
                    Box::pin(
                        async move { sdk.audit_private_intake_group(&group, &health, &work).await },
                    )
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
                audit_outcome.handshake_pending,
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
                topology_changed.notify_one();
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
    ) -> PrivateIntakeAuditOutcome {
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
                return PrivateIntakeAuditOutcome::default();
            }
        };
        let mut audit = PrivateIntakeAuditOutcome::default();
        for peer in &group.peers {
            if !active_peers.contains(peer) {
                audit.backpressured = true;
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
                Ok(outcome) => {
                    audit.handshake_pending |=
                        matches!(&outcome, PrivatePacketAcquisition::PendingHandshake);
                    audit.backpressured |= matches!(
                        &outcome,
                        PrivatePacketAcquisition::InsertedAtCapacity
                            | PrivatePacketAcquisition::CapacityReached
                    );
                    if matches!(
                        &outcome,
                        PrivatePacketAcquisition::Inserted
                            | PrivatePacketAcquisition::InsertedAtCapacity
                            | PrivatePacketAcquisition::PendingHandshake
                    ) {
                        work.notify_waiters();
                    }
                    set_peer_audit_outcome(health, peer.clone(), outcome, self.clock.now()).await;
                }
                Err(_) => {
                    set_peer_state(health, peer.clone(), PrivateIntakeTransportState::Stale).await
                }
            }
        }
        audit
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
            .and_then(|(_, snapshot, _)| snapshot)
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

async fn set_peer_audit_outcome(
    health: &Arc<AsyncRwLock<PrivateIntakeHealth>>,
    peer: PubkyPublicKey,
    outcome: PrivatePacketAcquisition,
    audited_at: DateTime<Utc>,
) {
    let mut health = health.write().await;
    let proves_absence = matches!(&outcome, PrivatePacketAcquisition::Absent);
    let state = match outcome {
        PrivatePacketAcquisition::Absent => PrivateIntakeTransportState::CaughtUp,
        PrivatePacketAcquisition::Inserted => PrivateIntakeTransportState::Replaying,
        PrivatePacketAcquisition::InsertedAtCapacity
        | PrivatePacketAcquisition::CapacityReached => PrivateIntakeTransportState::Backpressured,
        PrivatePacketAcquisition::AlreadyBuffered | PrivatePacketAcquisition::PendingHandshake => {
            PrivateIntakeTransportState::Replaying
        }
    };
    health.peers.insert(peer.clone(), state);
    if proves_absence {
        health.last_successful_audit_at.insert(peer, audited_at);
    }
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
    wait_retry_or_stop(stop, RECONNECT_DELAY).await
}

async fn wait_retry_or_stop(stop: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        changed = stop.changed() => changed.is_err() || *stop.borrow(),
    }
}

fn disconnected_stream_retry(
    error: &pubky::Error,
    healthy_audit_interval: Duration,
) -> (PrivateIntakeTransportState, Duration) {
    if is_unsupported_event_stream(error) {
        (
            PrivateIntakeTransportState::Unsupported,
            healthy_audit_interval,
        )
    } else {
        (PrivateIntakeTransportState::Stale, RECONNECT_DELAY)
    }
}

async fn audit_disconnected_private_intake_stream<F, Fut>(
    error: &pubky::Error,
    healthy_audit_interval: Duration,
    audit: F,
) -> (
    PrivateIntakeTransportState,
    Duration,
    PrivateIntakeAuditOutcome,
)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = PrivateIntakeAuditOutcome>,
{
    let (state, retry_after) = disconnected_stream_retry(error, healthy_audit_interval);
    (state, retry_after, audit().await)
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
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[derive(Clone)]
    struct TransactionGateStorage {
        inner: crate::storage::InMemoryStorage,
        entered: Arc<StdMutex<Option<std::sync::mpsc::Sender<()>>>>,
        release: Arc<(StdMutex<bool>, std::sync::Condvar)>,
    }

    impl TransactionGateStorage {
        fn new(inner: crate::storage::InMemoryStorage) -> (Self, std::sync::mpsc::Receiver<()>) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            (
                Self {
                    inner,
                    entered: Arc::new(StdMutex::new(Some(entered_tx))),
                    release: Arc::new((StdMutex::new(false), std::sync::Condvar::new())),
                },
                entered_rx,
            )
        }

        fn release(&self) {
            let (released, condition) = &*self.release;
            *released.lock().unwrap() = true;
            condition.notify_one();
        }
    }

    #[async_trait::async_trait]
    impl crate::storage::StorageAdapter for TransactionGateStorage {
        async fn transaction_erased<'a>(
            &self,
            callback: crate::storage::StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn std::any::Any + Send>> {
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let (released, condition) = &*self.release;
            {
                let mut released = released.lock().unwrap();
                while !*released {
                    released = condition.wait(released).unwrap();
                }
            }
            self.inner.transaction_erased(callback).await
        }
    }

    #[derive(Clone)]
    struct ScriptedTransactionStorage {
        inner: crate::storage::InMemoryStorage,
        errors: Arc<StdMutex<VecDeque<PaykitSdkError>>>,
        attempts: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::storage::StorageAdapter for ScriptedTransactionStorage {
        async fn transaction_erased<'a>(
            &self,
            callback: crate::storage::StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn std::any::Any + Send>> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.errors.lock().unwrap().pop_front() {
                return Err(error);
            }
            self.inner.transaction_erased(callback).await
        }
    }

    #[derive(Clone)]
    struct MutableClock(Arc<StdMutex<DateTime<Utc>>>);

    impl MutableClock {
        fn new(now: DateTime<Utc>) -> Self {
            Self(Arc::new(StdMutex::new(now)))
        }

        fn set(&self, now: DateTime<Utc>) {
            *self.0.lock().unwrap() = now;
        }
    }

    impl Clock for MutableClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

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

    #[tokio::test]
    async fn test_topology_refresh_is_bounded_and_capacity_release_is_immediate() {
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        let capacity_released = Notify::new();
        let topology_changed = Notify::new();
        let mut refresh = tokio::time::interval(TOPOLOGY_REFRESH_INTERVAL);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        refresh.tick().await;

        let mut waiting = Box::pin(wait_private_intake_topology_trigger(
            &mut refresh,
            &capacity_released,
            &topology_changed,
            &mut stop_rx,
        ));
        tokio::select! {
            trigger = &mut waiting => panic!("topology refreshed at reconnect cadence: {trigger:?}"),
            _ = tokio::time::sleep(RECONNECT_DELAY + RECONNECT_DELAY) => {}
        }
        capacity_released.notify_one();
        assert_eq!(
            waiting.await,
            PrivateIntakeTopologyTrigger::CapacityReleased
        );
    }

    #[tokio::test]
    async fn test_topology_refresh_fires_on_its_own_cadence() {
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        let capacity_released = Notify::new();
        let topology_changed = Notify::new();
        let mut refresh = tokio::time::interval(Duration::from_millis(20));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        refresh.tick().await;

        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                wait_private_intake_topology_trigger(
                    &mut refresh,
                    &capacity_released,
                    &topology_changed,
                    &mut stop_rx,
                ),
            )
            .await
            .unwrap(),
            PrivateIntakeTopologyTrigger::Refresh
        );
    }

    #[tokio::test]
    async fn test_topology_change_trigger_is_immediate_without_advancing_healthy_cadence() {
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        let capacity_released = Notify::new();
        let topology_changed = Notify::new();
        let mut refresh = tokio::time::interval(TOPOLOGY_REFRESH_INTERVAL);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        refresh.tick().await;

        topology_changed.notify_one();
        assert_eq!(
            tokio::time::timeout(
                RECONNECT_DELAY,
                wait_private_intake_topology_trigger(
                    &mut refresh,
                    &capacity_released,
                    &topology_changed,
                    &mut stop_rx,
                ),
            )
            .await
            .unwrap(),
            PrivateIntakeTopologyTrigger::TopologyChanged
        );
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
    fn test_private_intake_admission_rejects_same_homeserver_fifty_first_peer() {
        let peers = users(51);
        let mut admissions = peers[..50]
            .iter()
            .cloned()
            .map(|peer| (peer, "homeserver-a".to_owned()))
            .collect::<HashMap<_, _>>();

        let error = reserve_private_intake_admission(
            &mut admissions,
            peers[50].clone(),
            "homeserver-a".into(),
            1,
        )
        .unwrap_err();

        assert!(matches!(error, PaykitSdkError::Policy { .. }));
        assert_eq!(admissions.len(), 50);
    }

    #[test]
    fn test_private_intake_admission_rejects_cross_homeserver_stream() {
        let peers = users(2);
        let mut admissions = HashMap::from([(peers[0].clone(), "homeserver-a".to_owned())]);

        let error = reserve_private_intake_admission(
            &mut admissions,
            peers[1].clone(),
            "homeserver-b".into(),
            1,
        )
        .unwrap_err();

        assert!(matches!(error, PaykitSdkError::Policy { .. }));
        assert_eq!(admissions.len(), 1);
    }

    #[test]
    fn test_private_intake_admission_rejects_topology_move_without_losing_old_coverage() {
        let peers = users(2);
        let mut admissions = HashMap::from([
            (peers[0].clone(), "homeserver-a".to_owned()),
            (peers[1].clone(), "homeserver-a".to_owned()),
        ]);

        let error = reserve_private_intake_admission(
            &mut admissions,
            peers[1].clone(),
            "homeserver-b".into(),
            1,
        )
        .unwrap_err();

        assert!(matches!(error, PaykitSdkError::Policy { .. }));
        assert_eq!(admissions[&peers[1]], "homeserver-a");
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
    async fn test_private_intake_audit_freshness_requires_absent_slot() {
        let peer = users(1).remove(0);
        let previous = Utc::now() - chrono::Duration::minutes(1);
        let audited_at = Utc::now();
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth {
            peers: HashMap::new(),
            reserved_streams: 1,
            last_successful_audit_at: HashMap::from([(peer.clone(), previous)]),
        }));

        for (outcome, expected_state) in [
            (
                PrivatePacketAcquisition::Inserted,
                PrivateIntakeTransportState::Replaying,
            ),
            (
                PrivatePacketAcquisition::AlreadyBuffered,
                PrivateIntakeTransportState::Replaying,
            ),
            (
                PrivatePacketAcquisition::CapacityReached,
                PrivateIntakeTransportState::Backpressured,
            ),
            (
                PrivatePacketAcquisition::PendingHandshake,
                PrivateIntakeTransportState::Replaying,
            ),
        ] {
            set_peer_audit_outcome(&health, peer.clone(), outcome, audited_at).await;
            let current = health.read().await;
            assert_eq!(current.peers[&peer], expected_state);
            assert_eq!(current.last_successful_audit_at[&peer], previous);
        }

        set_peer_audit_outcome(
            &health,
            peer.clone(),
            PrivatePacketAcquisition::Absent,
            audited_at,
        )
        .await;
        let current = health.read().await;
        assert_eq!(current.peers[&peer], PrivateIntakeTransportState::CaughtUp);
        assert_eq!(current.last_successful_audit_at[&peer], audited_at);
    }

    #[tokio::test]
    async fn test_private_intake_retries_unsupported_and_transient_event_stream_errors() {
        let healthy_cadence = Duration::from_secs(17);
        let audits = Arc::new(AtomicUsize::new(0));
        let storage = crate::storage::InMemoryStorage::new();
        let subscribed_user = users(1).remove(0);
        for status in [
            pubky::StatusCode::NOT_FOUND,
            pubky::StatusCode::METHOD_NOT_ALLOWED,
            pubky::StatusCode::NOT_IMPLEMENTED,
        ] {
            let error = pubky::Error::Request(pubky::errors::RequestError::Server {
                status,
                message: "event stream is unavailable".into(),
            });
            let (state, retry_after, outcome) =
                audit_disconnected_private_intake_stream(&error, healthy_cadence, {
                    let audits = Arc::clone(&audits);
                    let storage = storage.clone();
                    let subscribed_user = subscribed_user.clone();
                    move || async move {
                        let event_id = audits.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                        storage
                            .transaction(move |tx| {
                                tx.advance_private_intake_cursor(PrivateIntakeCursorRecord {
                                    subscribed_user,
                                    homeserver: "homeserver-a".into(),
                                    event_id,
                                })
                            })
                            .await
                            .unwrap();
                        PrivateIntakeAuditOutcome::default()
                    }
                })
                .await;
            assert_eq!(state, PrivateIntakeTransportState::Unsupported);
            assert_eq!(retry_after, healthy_cadence);
            assert_eq!(outcome, PrivateIntakeAuditOutcome::default());
        }
        let transient = pubky::Error::Request(pubky::errors::RequestError::Server {
            status: pubky::StatusCode::INTERNAL_SERVER_ERROR,
            message: "temporary proxy failure".into(),
        });
        let (state, retry_after, outcome) =
            audit_disconnected_private_intake_stream(&transient, healthy_cadence, {
                let audits = Arc::clone(&audits);
                let storage = storage.clone();
                let subscribed_user = subscribed_user.clone();
                move || async move {
                    let event_id = audits.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                    storage
                        .transaction(move |tx| {
                            tx.advance_private_intake_cursor(PrivateIntakeCursorRecord {
                                subscribed_user,
                                homeserver: "homeserver-a".into(),
                                event_id,
                            })
                        })
                        .await
                        .unwrap();
                    PrivateIntakeAuditOutcome::default()
                }
            })
            .await;
        assert_eq!(state, PrivateIntakeTransportState::Stale);
        assert_eq!(retry_after, RECONNECT_DELAY);
        assert_eq!(outcome, PrivateIntakeAuditOutcome::default());
        assert_eq!(audits.load(Ordering::SeqCst), 4);
        assert_eq!(
            storage.snapshot().unwrap().private_intake_cursors[&subscribed_user].event_id,
            4
        );
    }

    #[tokio::test]
    async fn test_private_intake_lease_is_identity_wide_across_sdk_handles() {
        let storage = crate::storage::InMemoryStorage::new();
        let first = claim_private_intake_lease(&storage, Utc::now(), 1, HashMap::new())
            .await
            .unwrap();
        let second =
            claim_private_intake_lease(&storage.clone(), Utc::now(), 1, HashMap::new()).await;
        assert!(matches!(second, Err(PaykitSdkError::Policy { .. })));

        release_private_intake_lease(&storage, first.lease_id)
            .await
            .unwrap();
        assert!(
            claim_private_intake_lease(&storage, Utc::now(), 1, HashMap::new())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_expired_private_intake_owner_cannot_renew_or_replace_before_reclaim() {
        let storage = crate::storage::InMemoryStorage::new();
        let peers = users(2);
        let claimed_at = Utc::now();
        let expires_at = claimed_at + SUPERVISOR_LEASE_TIMEOUT;
        let original = HashMap::from([(peers[0].clone(), "homeserver-a".to_owned())]);
        let lease = storage
            .transaction({
                let original = original.clone();
                move |tx| tx.claim_private_intake_supervisor(claimed_at, expires_at, 1, original)
            })
            .await
            .unwrap()
            .unwrap();
        let replacement = HashMap::from([(peers[1].clone(), "homeserver-b".to_owned())]);

        assert!(!storage
            .transaction({
                let replacement = replacement.clone();
                move |tx| {
                    tx.replace_private_intake_admissions(lease.lease_id, expires_at, replacement)
                }
            })
            .await
            .unwrap());
        assert!(!storage
            .transaction(move |tx| {
                Ok(tx.renew_private_intake_supervisor(
                    lease.lease_id,
                    expires_at,
                    expires_at + SUPERVISOR_LEASE_TIMEOUT,
                ))
            })
            .await
            .unwrap());

        let contender = storage
            .transaction({
                let replacement = replacement.clone();
                move |tx| {
                    tx.claim_private_intake_supervisor(
                        expires_at,
                        expires_at + SUPERVISOR_LEASE_TIMEOUT,
                        1,
                        replacement,
                    )
                }
            })
            .await
            .unwrap()
            .unwrap();
        assert_ne!(contender.lease_id, lease.lease_id);
        let snapshot = storage.snapshot().unwrap();
        assert_eq!(
            snapshot.private_intake_supervisor_lease.unwrap().lease_id,
            contender.lease_id
        );
        assert_eq!(snapshot.private_intake_admitted_homeservers, replacement);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_private_intake_renewal_samples_clock_after_storage_fence() {
        let inner = crate::storage::InMemoryStorage::new();
        let claimed_at = Utc::now();
        let expires_at = claimed_at + SUPERVISOR_LEASE_TIMEOUT;
        let lease = inner
            .transaction(move |tx| {
                tx.claim_private_intake_supervisor(claimed_at, expires_at, 1, HashMap::new())
            })
            .await
            .unwrap()
            .unwrap();
        let (storage, entered) = TransactionGateStorage::new(inner.clone());
        let clock = MutableClock::new(claimed_at + chrono::Duration::seconds(1));
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn({
            let storage = storage.clone();
            let clock = clock.clone();
            async move {
                renew_private_intake_lease_or_stop(
                    &storage,
                    &clock,
                    lease.lease_id,
                    expires_at,
                    &stop,
                )
                .await
            }
        });

        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("renewal must wait at storage fence");
        clock.set(expires_at);
        storage.release();

        assert!(task.await.unwrap().is_none());
        stopped.changed().await.unwrap();
        assert!(*stopped.borrow());
        let contender = claim_private_intake_lease(&inner, expires_at, 1, HashMap::new())
            .await
            .unwrap();
        assert_ne!(contender.lease_id, lease.lease_id);
    }

    #[tokio::test]
    async fn test_private_intake_renewal_retries_transient_storage_errors() {
        for error in [
            PaykitSdkError::SharedStateBusy {
                context: "busy".into(),
                source: None,
            },
            PaykitSdkError::ConcurrentUpdate {
                context: "conflict".into(),
                source: None,
            },
            PaykitSdkError::Transport {
                context: "transport".into(),
                source: None,
            },
        ] {
            let inner = crate::storage::InMemoryStorage::new();
            let claimed_at = Utc::now();
            let expires_at = claimed_at + SUPERVISOR_LEASE_TIMEOUT;
            let lease = claim_private_intake_lease(&inner, claimed_at, 1, HashMap::new())
                .await
                .unwrap();
            let storage = ScriptedTransactionStorage {
                inner,
                errors: Arc::new(StdMutex::new(VecDeque::from([error]))),
                attempts: Arc::new(AtomicUsize::new(0)),
            };
            let clock = MutableClock::new(claimed_at + chrono::Duration::seconds(30));
            let (stop, stopped) = watch::channel(false);

            assert!(renew_private_intake_lease_or_stop(
                &storage,
                &clock,
                lease.lease_id,
                expires_at,
                &stop,
            )
            .await
            .is_some());
            assert_eq!(storage.attempts.load(Ordering::SeqCst), 2);
            assert!(!*stopped.borrow());
        }

        let inner = crate::storage::InMemoryStorage::new();
        let claimed_at = Utc::now();
        let expires_at = claimed_at + SUPERVISOR_LEASE_TIMEOUT;
        let lease = claim_private_intake_lease(&inner, claimed_at, 1, HashMap::new())
            .await
            .unwrap();
        let storage = ScriptedTransactionStorage {
            inner,
            errors: Arc::new(StdMutex::new(VecDeque::from([PaykitSdkError::Transport {
                context: "transport".into(),
                source: None,
            }]))),
            attempts: Arc::new(AtomicUsize::new(0)),
        };
        let margin = chrono::Duration::from_std(SUPERVISOR_LEASE_RENEW_INTERVAL).unwrap();
        let clock = MutableClock::new(expires_at - margin);
        let (stop, mut stopped) = watch::channel(false);

        assert!(renew_private_intake_lease_or_stop(
            &storage,
            &clock,
            lease.lease_id,
            expires_at,
            &stop,
        )
        .await
        .is_none());
        stopped.changed().await.unwrap();
        assert!(*stopped.borrow());
        assert_eq!(storage.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_private_intake_admission_retries_transient_storage_errors() {
        for error in [
            PaykitSdkError::SharedStateBusy {
                context: "busy".into(),
                source: None,
            },
            PaykitSdkError::ConcurrentUpdate {
                context: "conflict".into(),
                source: None,
            },
            PaykitSdkError::Transport {
                context: "transport".into(),
                source: None,
            },
        ] {
            let inner = crate::storage::InMemoryStorage::new();
            let now = Utc::now();
            let lease = claim_private_intake_lease(&inner, now, 1, HashMap::new())
                .await
                .unwrap();
            let peer = users(1).remove(0);
            let replacement = HashMap::from([(peer, "homeserver-a".to_owned())]);
            let storage = ScriptedTransactionStorage {
                inner: inner.clone(),
                errors: Arc::new(StdMutex::new(VecDeque::from([error]))),
                attempts: Arc::new(AtomicUsize::new(0)),
            };
            let (_stop_tx, mut stop_rx) = watch::channel(false);

            assert!(replace_private_intake_admissions_with_retry(
                &storage,
                &MutableClock::new(now),
                lease.lease_id,
                replacement.clone(),
                &mut stop_rx,
            )
            .await
            .unwrap());
            assert_eq!(storage.attempts.load(Ordering::SeqCst), 2);
            assert_eq!(
                inner
                    .snapshot()
                    .unwrap()
                    .private_intake_admitted_homeservers,
                replacement
            );
        }

        let inner = crate::storage::InMemoryStorage::new();
        let now = Utc::now();
        let lease = claim_private_intake_lease(&inner, now, 1, HashMap::new())
            .await
            .unwrap();
        let storage = ScriptedTransactionStorage {
            inner,
            errors: Arc::new(StdMutex::new(VecDeque::new())),
            attempts: Arc::new(AtomicUsize::new(0)),
        };
        let (_stop_tx, mut stop_rx) = watch::channel(false);
        assert!(!replace_private_intake_admissions_with_retry(
            &storage,
            &MutableClock::new(now),
            lease.lease_id + 1,
            HashMap::new(),
            &mut stop_rx,
        )
        .await
        .unwrap());
        assert_eq!(storage.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_private_intake_replacement_samples_clock_after_storage_fence() {
        let inner = crate::storage::InMemoryStorage::new();
        let peers = users(2);
        let claimed_at = Utc::now();
        let expires_at = claimed_at + SUPERVISOR_LEASE_TIMEOUT;
        let original = HashMap::from([(peers[0].clone(), "homeserver-a".to_owned())]);
        let lease = inner
            .transaction({
                let original = original.clone();
                move |tx| tx.claim_private_intake_supervisor(claimed_at, expires_at, 1, original)
            })
            .await
            .unwrap()
            .unwrap();
        let replacement = HashMap::from([(peers[1].clone(), "homeserver-b".to_owned())]);
        let (storage, entered) = TransactionGateStorage::new(inner.clone());
        let clock = MutableClock::new(claimed_at + chrono::Duration::seconds(1));
        let task = tokio::spawn({
            let storage = storage.clone();
            let clock = clock.clone();
            let replacement = replacement.clone();
            async move {
                replace_private_intake_admissions(&storage, &clock, lease.lease_id, replacement)
                    .await
            }
        });

        entered
            .recv_timeout(Duration::from_secs(2))
            .expect("replacement must wait at storage fence");
        clock.set(expires_at + chrono::Duration::seconds(1));
        storage.release();

        assert!(!task.await.unwrap().unwrap());
        assert_eq!(
            inner
                .snapshot()
                .unwrap()
                .private_intake_admitted_homeservers,
            original
        );
        let contender = claim_private_intake_lease(&inner, expires_at, 1, replacement)
            .await
            .unwrap();
        assert_ne!(contender.lease_id, lease.lease_id);
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

        let supervisor = claim_private_intake_lease(&storage, now, 1, HashMap::new())
            .await
            .unwrap();
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
    async fn test_private_intake_capacity_admission_is_identity_wide_across_sdk_handles() {
        let storage = crate::storage::InMemoryStorage::new();
        let peers = users(51);
        let admissions = peers[..50]
            .iter()
            .cloned()
            .map(|peer| (peer, "homeserver-a".to_owned()))
            .collect();
        claim_private_intake_lease(&storage, Utc::now(), 1, admissions)
            .await
            .unwrap();

        let alternate_handle = storage.clone();
        let peer = peers[50].clone();
        let result = alternate_handle
            .transaction(move |tx| {
                tx.reserve_private_intake_admission(peer, "homeserver-a".into(), Utc::now())
            })
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .private_intake_admitted_homeservers
                .len(),
            50
        );
    }

    #[tokio::test]
    async fn test_private_intake_topology_overflow_preserves_covered_topology() {
        let storage = crate::storage::InMemoryStorage::new();
        let peers = users(2);
        let current = HashMap::from([
            (peers[0].clone(), "homeserver-a".to_owned()),
            (peers[1].clone(), "homeserver-a".to_owned()),
        ]);
        let lease = claim_private_intake_lease(&storage, Utc::now(), 1, current.clone())
            .await
            .unwrap();
        let moved = HashMap::from([
            (peers[0].clone(), "homeserver-a".to_owned()),
            (peers[1].clone(), "homeserver-b".to_owned()),
        ]);

        let result = storage
            .transaction(move |tx| {
                tx.replace_private_intake_admissions(lease.lease_id, Utc::now(), moved)
            })
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .private_intake_admitted_homeservers,
            current
        );
    }

    #[tokio::test]
    async fn test_private_intake_stop_marks_every_peer_stopped() {
        let peers = users(2);
        let (stop, _) = watch::channel(false);
        let mut stop_rx = stop.subscribe();
        let task = tokio::spawn(async move {
            let _ = stop_rx.changed().await;
        });
        let task_handles = vec![task.abort_handle()];
        let supervisor = PrivateIntakeSupervisor {
            lease_id: 1,
            stop,
            tasks: AsyncMutex::new(vec![task]),
            task_handles,
            health: Arc::new(AsyncRwLock::new(PrivateIntakeHealth {
                peers: HashMap::from([
                    (peers[0].clone(), PrivateIntakeTransportState::CaughtUp),
                    (peers[1].clone(), PrivateIntakeTransportState::Stale),
                ]),
                reserved_streams: 2,
                last_successful_audit_at: HashMap::new(),
            })),
            work: Arc::new(Notify::new()),
            capacity_released: Arc::new(Notify::new()),
        };

        assert!(!supervisor.is_finished());
        supervisor.stop().await;

        assert!(supervisor.is_finished());
        assert!(supervisor
            .health()
            .await
            .peers
            .values()
            .all(|state| *state == PrivateIntakeTransportState::Stopped));
    }

    #[tokio::test]
    async fn test_private_intake_is_not_finished_while_stop_waits_for_live_task() {
        let (stop, _) = watch::channel(false);
        let mut stop_rx = stop.subscribe();
        let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = stop_rx.changed().await;
            stopping_tx.send(()).unwrap();
            let _ = release_rx.await;
        });
        let task_handles = vec![task.abort_handle()];
        let supervisor = PrivateIntakeSupervisor {
            lease_id: 1,
            stop,
            tasks: AsyncMutex::new(vec![task]),
            task_handles,
            health: Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default())),
            work: Arc::new(Notify::new()),
            capacity_released: Arc::new(Notify::new()),
        };

        let mut first_stop = Box::pin(supervisor.stop());
        tokio::select! {
            _ = &mut first_stop => panic!("stop returned while its task was live"),
            result = stopping_rx => result.unwrap(),
        }
        assert!(!supervisor.is_finished());

        let mut second_stop = Box::pin(supervisor.stop());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut second_stop)
                .await
                .is_err(),
            "concurrent stop returned while a task was live"
        );

        release_tx.send(()).unwrap();
        first_stop.await;
        second_stop.await;
        assert!(supervisor.is_finished());
    }

    #[tokio::test]
    async fn test_private_intake_is_finished_after_autonomous_task_termination() {
        let peer = users(1).remove(0);
        let (stop, _) = watch::channel(false);
        let (finish_first, finished_first) = tokio::sync::oneshot::channel::<()>();
        let (finish_second, finished_second) = tokio::sync::oneshot::channel::<()>();
        let first_task = tokio::spawn(async move {
            let _ = finished_first.await;
        });
        let second_task = tokio::spawn(async move {
            let _ = finished_second.await;
        });
        let task_handles = vec![first_task.abort_handle(), second_task.abort_handle()];
        let supervisor = PrivateIntakeSupervisor {
            lease_id: 1,
            stop,
            tasks: AsyncMutex::new(vec![first_task, second_task]),
            task_handles,
            health: Arc::new(AsyncRwLock::new(PrivateIntakeHealth {
                peers: HashMap::from([(peer, PrivateIntakeTransportState::Stale)]),
                reserved_streams: 1,
                last_successful_audit_at: HashMap::new(),
            })),
            work: Arc::new(Notify::new()),
            capacity_released: Arc::new(Notify::new()),
        };

        assert!(!supervisor.is_finished());
        finish_first.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(!supervisor.is_finished());
        finish_second.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !supervisor.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(supervisor.is_finished());
    }

    #[tokio::test]
    async fn test_group_loop_audits_active_peers_so_healthy_sibling_stays_live() {
        let peers = users(2);
        let full_peer = peers[0].clone();
        let healthy_peer = peers[1].clone();
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers,
        };
        let mut state = crate::storage::StorageState::default();
        state.private_intake_peers.insert(
            full_peer.clone(),
            crate::storage::PrivateIntakePeerRecord {
                counterparty: full_peer.clone(),
                link_generation: 1,
                buffered_packets: (0..crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER)
                    .map(|receive_counter| BufferedPrivatePacketRecord {
                        counterparty: full_peer.clone(),
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
        let active_peers = subscribable_private_intake_peers(&state, &group.peers);
        let active_group = active_subscription_group(&group, active_peers);
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let audited = Arc::new(Notify::new());
        let (stop, stop_rx) = watch::channel(false);
        let audit: PrivateIntakeAudit = Arc::new({
            let audited = Arc::clone(&audited);
            let audited_peers = active_group.peers.clone();
            move || {
                let audited = Arc::clone(&audited);
                let backpressured = audited_peers.contains(&full_peer);
                Box::pin(async move {
                    audited.notify_one();
                    PrivateIntakeAuditOutcome {
                        handshake_pending: false,
                        backpressured,
                    }
                })
            }
        });
        let mut running = Box::pin(run_connected_private_intake_stream(
            &active_group,
            Box::pin(stream::pending()),
            Duration::from_millis(10),
            Duration::ZERO,
            stop_rx,
            &health,
            &work,
            Arc::new(|_, _| Box::pin(async { Ok(PrivatePacketAcquisition::Absent) })),
            audit,
            false,
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = audited.notified() => {}
                exit = &mut running => panic!("healthy sibling stream exited during audit: {exit:?}"),
            }
        })
        .await
        .expect("active sibling should reach live audit");
        assert_eq!(active_group.peers, vec![healthy_peer]);
        stop.send(true).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), running)
                .await
                .unwrap(),
            PrivateIntakeEventLoopExit::Stopped
        ));
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
        let audit: PrivateIntakeAudit =
            Arc::new(|| Box::pin(async { PrivateIntakeAuditOutcome::default() }));

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
            false,
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
    async fn test_pending_handshake_event_retries_before_healthy_audit() {
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
            resource: format!("pubky://{}/pub/paykit/v0/private/slot", owner.z32())
                .parse()
                .unwrap(),
            cursor: pubky::EventCursor::new(42),
        };
        let events = stream::once(async { Ok(event) }).chain(stream::pending());
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let retried = Arc::new(Notify::new());
        let (_stop, stop_rx) = watch::channel(false);
        let acquire: PrivateIntakeAcquire =
            Arc::new(|_, _| Box::pin(async { Ok(PrivatePacketAcquisition::PendingHandshake) }));
        let audit: PrivateIntakeAudit = Arc::new({
            let retried = Arc::clone(&retried);
            move || {
                let retried = Arc::clone(&retried);
                Box::pin(async move {
                    retried.notify_one();
                    PrivateIntakeAuditOutcome::default()
                })
            }
        });
        let mut running = Box::pin(run_connected_private_intake_stream(
            &group,
            Box::pin(events),
            Duration::from_secs(60),
            Duration::from_secs(60),
            stop_rx,
            &health,
            &work,
            acquire,
            audit,
            false,
        ));

        tokio::time::timeout(RECONNECT_DELAY + RECONNECT_DELAY, async {
            tokio::select! {
                _ = retried.notified() => {}
                exit = &mut running => panic!("connected stream exited before retry: {exit:?}"),
            }
        })
        .await
        .expect("pending handshake should retry before healthy audit cadence");
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
    async fn test_expected_slot_final_capacity_insert_reports_backpressure() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        storage
            .transaction({
                let peer = peer.clone();
                move |tx| {
                    for receive_counter in 3..3 + u64::try_from(
                        crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER - 1,
                    )
                    .unwrap()
                    {
                        assert!(matches!(
                            tx.buffer_private_intake_packet(BufferedPrivatePacketRecord {
                                counterparty: peer.clone(),
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

        let outcome = acquire_expected_private_packet_with_fetch(
            &storage,
            peer.clone(),
            &snapshot,
            &local,
            &secret,
            7,
            None,
            Utc::now(),
            |_| async { Ok(Some(vec![9; crate::storage::PRIVATE_INTAKE_PACKET_BYTES])) },
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            PrivatePacketAcquisition::InsertedAtCapacity
        ));
        assert_eq!(
            storage.snapshot().unwrap().private_intake_peers[&peer]
                .buffered_packets
                .len(),
            crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER
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
            Arc::new(|| Box::pin(async { PrivateIntakeAuditOutcome::default() })),
            false,
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
            Arc::new(|| Box::pin(async { PrivateIntakeAuditOutcome::default() })),
            false,
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
            Arc::new(|| Box::pin(async { PrivateIntakeAuditOutcome::default() })),
            false,
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
            Arc::new(|| Box::pin(async { PrivateIntakeAuditOutcome::default() })),
            false,
        )
        .await;
        assert!(matches!(
            resumed_exit,
            PrivateIntakeEventLoopExit::Backpressured
        ));
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
