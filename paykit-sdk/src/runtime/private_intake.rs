use super::*;
use crate::storage::{
    BufferedPrivatePacketRecord, PaykitAppOperationLease, PrivateIntakeBufferOutcome,
    PrivateIntakeCursorRecord,
};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
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
const SUPERVISOR_LEASE_APP_ID: &str = "paykit-private-intake-supervisor";

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

fn supervisor_lease_app_id() -> paykit_lib::PaykitAppId {
    paykit_lib::PaykitAppId::new(SUPERVISOR_LEASE_APP_ID)
        .expect("private intake supervisor lease app id is valid")
}

async fn claim_private_intake_lease<S: StorageAdapter>(
    storage: &S,
    now: DateTime<Utc>,
) -> Result<PaykitAppOperationLease> {
    storage
        .transaction(move |tx| {
            tx.claim_paykit_app_operation(
                &supervisor_lease_app_id(),
                now,
                now + SUPERVISOR_LEASE_TIMEOUT,
            )?
            .ok_or_else(|| PaykitSdkError::Policy {
                context: "private intake supervisor is already running for this identity".into(),
                source: None,
            })
        })
        .await
}

async fn release_private_intake_lease<S: StorageAdapter>(storage: &S, lease_id: u64) -> Result<()> {
    storage
        .transaction(move |tx| {
            tx.release_paykit_app_operation(&supervisor_lease_app_id(), lease_id);
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
                                Ok(tx.renew_paykit_app_operation(
                                    &supervisor_lease_app_id(),
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
                    let peers = group.peers.clone();
                    let homeserver = group.homeserver_name.clone();
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
                &group.homeserver,
            )
            .add_users(users)
            {
                Ok(builder) => builder,
                Err(_) => {
                    set_group_state(&health, &group, PrivateIntakeTransportState::Unsupported)
                        .await;
                    break;
                }
            };
            let mut stream = match builder
                .path(paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX)
                .max_event_bytes(config.max_event_bytes)
                .live()
                .subscribe()
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    if is_unsupported_event_stream(&error) {
                        set_group_state(&health, &group, PrivateIntakeTransportState::Unsupported)
                            .await;
                        break;
                    }
                    set_group_state(&health, &group, PrivateIntakeTransportState::Stale).await;
                    if wait_reconnect_or_stop(&mut stop).await {
                        break;
                    }
                    continue;
                }
            };
            set_group_state(&health, &group, PrivateIntakeTransportState::Replaying).await;
            let mut audit = tokio::time::interval_at(
                tokio::time::Instant::now() + audit_jitter(&group),
                config.healthy_audit_interval,
            );
            audit.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() { return; }
                    }
                    _ = audit.tick() => {
                        self.audit_private_intake_group(&group, &health, &work).await;
                    }
                    event = stream.next() => match event {
                        Some(Ok(event)) => {
                            let peer = PubkyPublicKey::from_public_key(&event.resource.owner);
                            if group.peers.contains(&peer) {
                                let cursor = PrivateIntakeCursorRecord {
                                    subscribed_user: peer.clone(),
                                    homeserver: group.homeserver_name.clone(),
                                    event_id: event.cursor.id(),
                                };
                                match self.acquire_expected_private_packet(peer.clone(), Some(cursor)).await {
                                    Ok(PrivatePacketAcquisition::Inserted) => {
                                        work.notify_waiters();
                                        set_peer_state(&health, peer, PrivateIntakeTransportState::Replaying).await;
                                    }
                                    Ok(PrivatePacketAcquisition::AlreadyBuffered) => {
                                        set_peer_state(&health, peer, PrivateIntakeTransportState::Replaying).await;
                                    }
                                    Ok(PrivatePacketAcquisition::Absent) => {
                                        set_peer_state(&health, peer, PrivateIntakeTransportState::CaughtUp).await;
                                    }
                                    Ok(PrivatePacketAcquisition::CapacityReached) => {
                                        set_peer_state(&health, peer, PrivateIntakeTransportState::Backpressured).await;
                                    }
                                    Err(_) => set_peer_state(&health, peer, PrivateIntakeTransportState::Stale).await,
                                }
                            }
                        }
                        Some(Err(_)) | None => {
                            set_group_state(&health, &group, PrivateIntakeTransportState::Stale).await;
                            break;
                        }
                    }
                }
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
        for peer in &group.peers {
            match self
                .acquire_expected_private_packet(peer.clone(), None)
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
}
