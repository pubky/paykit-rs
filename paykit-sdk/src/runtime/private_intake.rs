use super::*;
use crate::storage::{
    BufferedPrivatePacketRecord, PrivateIntakeBufferOutcome, PrivateIntakeCursorRecord,
    PrivateIntakeSupervisorLease,
};
use futures_util::StreamExt;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{watch, Mutex as AsyncMutex, Notify, RwLock as AsyncRwLock};

const MAX_EVENT_STREAM_USERS: usize = crate::storage::PRIVATE_INTAKE_MAX_EVENT_STREAM_USERS;
const DEFAULT_MAX_EVENT_BYTES: usize = 4096;
// The limit applies to an event's joined `data` lines. `MIN_EVENT_BYTES`
// leaves headroom above `LARGEST_EVENT_DATA_BYTES`, and the assertion below
// fails the build if a pubky-common path limit change outgrows it.
const MIN_EVENT_BYTES: usize = 2048;
// The largest `data` payload a homeserver event can carry: a PUT event,
// rendered by pubky-homeserver's `EventEntity::to_sse_data` as
// `pubky://<z32 key><path>\ncursor: <u64>\ncontent_hash: <base64 hash>`.
const LARGEST_EVENT_DATA_BYTES: usize = "pubky://".len()
    + 52 // z32-encoded Ed25519 public key
    + pubky_common::storage_path::MAX_STORAGE_PATH_TOTAL_LENGTH
    + "\ncursor: ".len()
    + 20 // decimal digits of u64::MAX
    + "\ncontent_hash: ".len()
    + 44; // padded base64 of a 32-byte hash
const _: () = assert!(MIN_EVENT_BYTES >= LARGEST_EVENT_DATA_BYTES);
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
    /// Maximum size of one homeserver event's data payload, its `data` lines
    /// joined by newlines (2048..=65536 bytes).
    ///
    /// A limit below the largest homeserver event would end the stream on
    /// that event, so values below 2048 are rejected.
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
            || self.max_event_bytes < MIN_EVENT_BYTES
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
    /// Audit or stream failed, or the Linked Peer's Pubky homeserver could not
    /// be resolved; remote absence cannot be claimed.
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

struct PrivateIntakePeerAudit {
    outcome: Result<PrivatePacketAcquisition>,
    inserted: bool,
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

/// Supervised peers split by whether a Pubky homeserver is known for them.
struct PrivateIntakeTopology {
    /// One reserved stream per group.
    groups: Vec<SubscriptionGroup>,
    /// Supervised peers with no resolvable Pubky homeserver and no previously
    /// resolved one. They reserve no stream and report `Stale`.
    unresolved: Vec<PubkyPublicKey>,
}

/// Build the topology from one homeserver lookup per supervised peer.
///
/// One peer's failed lookup never fails the whole pass: that would stop
/// intake for every other peer at startup and freeze a running supervisor's
/// peer set on refresh.
fn private_intake_topology_from_lookups(
    lookups: Vec<(PubkyPublicKey, Result<Option<paykit_lib::PublicKey>>)>,
    current: &[SubscriptionGroup],
) -> PrivateIntakeTopology {
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for (peer, lookup) in lookups {
        match lookup {
            Ok(Some(homeserver)) => resolved.push((homeserver.to_string(), homeserver, peer)),
            // A Pkarr not-found can be a DHT miss rather than a removed record,
            // so a peer that already has a stream keeps its last resolved
            // homeserver instead of restarting every stream. The lookup error
            // is dropped on purpose: the supervisor has no error channel, and
            // `Stale` health is the caller-visible signal for an uncovered peer.
            Ok(None) | Err(_) => match current.iter().find(|group| group.peers.contains(&peer)) {
                Some(group) => resolved.push((
                    group.homeserver_name.clone(),
                    group.homeserver.clone(),
                    peer,
                )),
                None => unresolved.push(peer),
            },
        }
    }
    PrivateIntakeTopology {
        groups: group_private_intake_peers_by_homeserver(resolved),
        unresolved,
    }
}

/// Align health with the supervised topology: drop peers no longer
/// supervised, keep existing states for streamed peers, and report peers
/// without a resolvable homeserver as `Stale`.
fn apply_private_intake_topology_health(
    health: &mut PrivateIntakeHealth,
    groups: &[SubscriptionGroup],
    unresolved: &[PubkyPublicKey],
) {
    let streamed = || groups.iter().flat_map(|group| group.peers.iter());
    let supervised = streamed()
        .chain(unresolved)
        .collect::<std::collections::HashSet<_>>();
    health.peers.retain(|peer, _| supervised.contains(peer));
    health
        .last_successful_audit_at
        .retain(|peer, _| supervised.contains(peer));
    health.reserved_streams = groups.len();
    for peer in streamed() {
        health
            .peers
            .entry(peer.clone())
            .or_insert(PrivateIntakeTransportState::Connecting);
    }
    for peer in unresolved {
        health
            .peers
            .insert(peer.clone(), PrivateIntakeTransportState::Stale);
    }
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

/// Event stream path filter selecting every private Paykit slot.
///
/// The homeserver event stream treats a path without a trailing `/` as one
/// exact file; only a trailing `/` selects the directory's descendants.
fn private_intake_event_path_filter() -> String {
    format!("{}/", paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX)
}

/// Per-peer Encrypted Link read folders used to drop irrelevant event hints.
///
/// A read folder ties a pseudorandom storage path to one counterparty, so it
/// must never be logged or exposed through health or errors. That is why this
/// type does not derive `Debug`. A peer without an entry is unfiltered: every
/// event under its private tree is a hint. Every acquisition that derives
/// the expected slot replaces its peer's entry with the read folder of the
/// snapshot it used. An acquisition that finds the peer pending an Encrypted
/// Link Handshake removes the entry, and one that fails before deriving the
/// folder keeps it (see `scoped_private_intake_acquire`).
#[derive(Default)]
struct PrivateIntakeEventScopes {
    read_folders: HashMap<PubkyPublicKey, String>,
}

/// Return the Encrypted Link folder that holds the derived slot `resource`,
/// or `None` (unfiltered) when the path does not have the derived shape.
fn private_intake_slot_folder(resource: &str) -> Option<String> {
    let resource: pubky::PubkyResource = resource.parse().ok()?;
    let (folder, _) = resource.path.as_str().rsplit_once('/')?;
    is_private_intake_link_folder(folder).then(|| folder.to_owned())
}

/// Whether `folder` has the shape of a derived Encrypted Link folder:
/// the private prefix followed by 64 lowercase hex characters.
///
/// paykit-lib owns this format: `encrypted_link/paths.rs` derives the folders
/// and `encrypted_link/snapshot.rs` (`validate_pending_packet`) checks the
/// same folder and slot shape. Keep this copy in step with them.
fn is_private_intake_link_folder(folder: &str) -> bool {
    folder
        .strip_prefix(paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Number of messages in a full Encrypted Link Handshake.
///
/// pubky-noise 0.1.0 writes handshake message `n` to slot `n` of the writer's
/// derived folder and starts both transport counters at this count when it
/// enters transport (`DataLinkContext::transition_to_transport`), so slots
/// below it are handshake messages and slots at or above it are transport
/// messages. A unit test pins it to pubky-noise's XX action list.
const ENCRYPTED_LINK_HANDSHAKE_MESSAGE_COUNT: usize = 3;
// `is_encrypted_link_handshake_slot` matches one decimal digit.
const _: () = assert!(ENCRYPTED_LINK_HANDSHAKE_MESSAGE_COUNT <= 10);

/// Whether `path` is an Encrypted Link Handshake slot in any derived folder.
///
/// A relink or recovery derives a new folder, so its handshake reply lands
/// outside the current read folder and must still be a hint. Slot names are
/// canonical decimal counters (see `validate_pending_packet` in paykit-lib
/// `encrypted_link/snapshot.rs`), so `01` or `0/1` is not a handshake slot.
fn is_encrypted_link_handshake_slot(path: &str) -> bool {
    path.rsplit_once('/').is_some_and(|(folder, slot)| {
        is_private_intake_link_folder(folder)
            && matches!(
                slot.as_bytes(),
                [digit @ b'0'..=b'9']
                    if usize::from(digit - b'0') < ENCRYPTED_LINK_HANDSHAKE_MESSAGE_COUNT
            )
    })
}

/// Whether `event` may announce a packet the expected-slot fetch would find.
///
/// `read_folder` is `None` for an unfiltered peer: every event is a hint.
/// Otherwise only a PUT below the read folder, or a PUT at an Encrypted Link
/// Handshake slot, is a hint. Deletes, Receipts, and other links' slots are
/// not.
fn private_intake_event_may_announce_packet(
    read_folder: Option<&str>,
    event: &pubky::Event,
) -> bool {
    let Some(read_folder) = read_folder else {
        return true;
    };
    if !matches!(event.event_type, pubky::EventType::Put { .. }) {
        return false;
    }
    let path = event.resource.path.as_str();
    path.strip_prefix(read_folder)
        .is_some_and(|rest| rest.starts_with('/'))
        || is_encrypted_link_handshake_slot(path)
}

/// Where a subscribed peer's event goes.
enum PrivateIntakeEventRoute {
    /// The event may announce a packet: acquire the expected slot.
    Hint(PrivateIntakeCursorRecord),
    /// The event cannot announce a packet. Its cursor is only committed after
    /// a later successful audit of the same peer.
    Ignored { peer: PubkyPublicKey, event_id: u64 },
}

/// Route an event to its subscribed peer's replay cursor, or `None` when the
/// owner is not subscribed.
///
/// Trust and durability assumptions: the event path is an untrusted hint and
/// never selects what is fetched; acquisition always derives the next expected
/// slot from the stored Encrypted Link. Healthy audits remain the delivery
/// guarantee, so a stale scope can only drop a hint. An ignored event writes
/// no cursor by itself, so a reconnect before the next successful audit
/// replays it and filters it again. A poisoned scope lock leaves every peer
/// unfiltered.
fn route_private_intake_event(
    group: &SubscriptionGroup,
    scopes: &std::sync::Mutex<PrivateIntakeEventScopes>,
    event: pubky::Event,
) -> Option<PrivateIntakeEventRoute> {
    let peer = PubkyPublicKey::from_public_key(&event.resource.owner);
    if !group.peers.contains(&peer) {
        return None;
    }
    let is_hint = scopes.lock().ok().is_none_or(|scopes| {
        private_intake_event_may_announce_packet(
            scopes.read_folders.get(&peer).map(String::as_str),
            &event,
        )
    });
    let event_id = event.cursor.id();
    Some(if is_hint {
        PrivateIntakeEventRoute::Hint(PrivateIntakeCursorRecord {
            subscribed_user: peer,
            homeserver: group.homeserver_name.clone(),
            event_id,
        })
    } else {
        PrivateIntakeEventRoute::Ignored { peer, event_id }
    })
}

/// Highest ignored event ID per peer since that peer's last successful
/// in-stream audit. Held only in memory by one connected stream, whose
/// subscription group names the homeserver these IDs belong to.
type PrivateIntakeIgnoredCursors = HashMap<PubkyPublicKey, u64>;

fn record_ignored_private_intake_event(
    ignored: &mut PrivateIntakeIgnoredCursors,
    peer: PubkyPublicKey,
    event_id: u64,
) {
    ignored
        .entry(peer)
        .and_modify(|highest| *highest = (*highest).max(event_id))
        .or_insert(event_id);
}

/// Run one in-stream audit, then commit, in one batch, the ignored cursors
/// of every peer whose audit found its next expected slot absent.
///
/// The caller holds `ignored` for the whole audit, so no event is recorded
/// while it runs and every entry was seen before the audit started.
///
/// Safety argument: the stream delivers a homeserver's events in cursor
/// order, and for each such peer the audit fetched the expected slot after
/// every event in `ignored` and found it absent. An ignored event either
/// cannot announce a packet the expected-slot fetch would find, or, when the
/// read folder was stale, announced one that this audit fetched. Moving the
/// stored cursor, on `homeserver_name`, past those events only skips
/// replaying them on reconnect.
///
/// `ignored` lives only as long as the connected stream. Any audit outcome
/// other than an absent expected slot (a pending handshake, an already
/// buffered packet, capacity, the acquisition cap or a failure) keeps the
/// peer's ignored cursor for the next in-stream audit while the stream stays
/// connected. An audit that stops at capacity makes the caller exit with
/// backpressure, which drops `ignored` like any other disconnect; the
/// homeserver then replays those events after reconnect and they are filtered
/// again.
async fn audit_private_intake_stream(
    audit_group: &PrivateIntakeAudit,
    homeserver_name: &str,
    ignored: &mut PrivateIntakeIgnoredCursors,
    commit_cursors: &PrivateIntakeCommitCursors,
) -> PrivateIntakeAuditOutcome {
    let outcome = audit_group().await;
    let mut audited = Vec::new();
    ignored.retain(|peer, event_id| {
        if !outcome.absent_slot_peers.contains(peer) {
            return true;
        }
        audited.push(PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: homeserver_name.to_owned(),
            event_id: *event_id,
        });
        false
    });
    if !audited.is_empty() {
        commit_cursors(audited).await;
    }
    outcome
}

/// Commit ignored events' replay cursors in one storage transaction, best
/// effort.
///
/// `StorageTransaction::advance_private_intake_cursor` rejects a cursor that
/// would regress (a later event already moved it) or that names another or an
/// empty homeserver, and its contract is that an error never changes state.
/// Skipping any error, a rejection or a fault, therefore cannot commit half a
/// write, and the other cursors still commit. A failed transaction only leaves
/// irrelevant events to replay.
async fn commit_ignored_private_intake_cursors<S: StorageAdapter>(
    storage: &S,
    cursors: Vec<PrivateIntakeCursorRecord>,
) {
    let _ = storage
        .transaction(move |tx| {
            for cursor in cursors {
                let _ = tx.advance_private_intake_cursor(cursor);
            }
            Ok(())
        })
        .await;
}

async fn audit_private_intake_peer<F, Fut>(
    peer: PubkyPublicKey,
    mut acquire: F,
) -> PrivateIntakePeerAudit
where
    F: FnMut(PubkyPublicKey, Option<PrivateIntakeCursorRecord>) -> Fut,
    Fut: Future<Output = Result<PrivatePacketAcquisition>>,
{
    let mut inserted = false;
    for _ in 0..MAX_AUDIT_ACQUISITIONS_PER_PEER {
        let outcome = match acquire(peer.clone(), None).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return PrivateIntakePeerAudit {
                    outcome: Err(error),
                    inserted,
                };
            }
        };
        match outcome {
            PrivatePacketAcquisition::Inserted => inserted = true,
            PrivatePacketAcquisition::Absent
            | PrivatePacketAcquisition::InsertedAtCapacity
            | PrivatePacketAcquisition::AlreadyBuffered
            | PrivatePacketAcquisition::CapacityReached
            | PrivatePacketAcquisition::PendingHandshake => {
                inserted |= matches!(outcome, PrivatePacketAcquisition::InsertedAtCapacity);
                return PrivateIntakePeerAudit {
                    outcome: Ok(outcome),
                    inserted,
                };
            }
        }
    }
    PrivateIntakePeerAudit {
        outcome: Ok(PrivatePacketAcquisition::Inserted),
        inserted,
    }
}

/// Audit every active peer in `group` through `acquire`, which also refreshes
/// each peer's event scope.
async fn audit_private_intake_group<S: StorageAdapter, C: Clock>(
    storage: &S,
    clock: &C,
    group: &SubscriptionGroup,
    health: &Arc<AsyncRwLock<PrivateIntakeHealth>>,
    work: &Arc<Notify>,
    acquire: &PrivateIntakeAcquire,
) -> PrivateIntakeAuditOutcome {
    let active_peers = match storage
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
        let peer_audit =
            audit_private_intake_peer(peer.clone(), |peer, cursor| acquire(peer, cursor)).await;
        if peer_audit.inserted {
            work.notify_waiters();
        }
        match peer_audit.outcome {
            Ok(outcome) => {
                audit.handshake_pending |=
                    matches!(&outcome, PrivatePacketAcquisition::PendingHandshake);
                audit.backpressured |= matches!(
                    &outcome,
                    PrivatePacketAcquisition::InsertedAtCapacity
                        | PrivatePacketAcquisition::CapacityReached
                );
                if matches!(&outcome, PrivatePacketAcquisition::PendingHandshake) {
                    work.notify_waiters();
                }
                // Only an absent expected slot proves the audit read past
                // every earlier event; other outcomes stopped short of it.
                if matches!(&outcome, PrivatePacketAcquisition::Absent) {
                    audit.absent_slot_peers.insert(peer.clone());
                }
                set_peer_audit_outcome(health, peer.clone(), outcome, clock.now()).await;
            }
            Err(_) => {
                set_peer_state(health, peer.clone(), PrivateIntakeTransportState::Stale).await
            }
        }
    }
    audit
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
/// An acquisition's outcome together with the read folder of the snapshot it
/// used, or `None` when it derived none.
type PrivateIntakeScopedAcquisitionFuture =
    Pin<Box<dyn Future<Output = (Option<String>, Result<PrivatePacketAcquisition>)> + Send>>;
type PrivateIntakeCommitCursorsFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
/// Best-effort monotonic write of ignored events' replay cursors in one
/// storage transaction.
type PrivateIntakeCommitCursors =
    Arc<dyn Fn(Vec<PrivateIntakeCursorRecord>) -> PrivateIntakeCommitCursorsFuture + Send + Sync>;

/// Wrap a scoped acquisition so its outcome refreshes its peer's event
/// scope. A derived read folder replaces the entry. Without a folder, a
/// successful outcome (a pending Encrypted Link Handshake) removes the entry,
/// which leaves the peer unfiltered, and an error keeps it unchanged. Stream
/// events and audits share the returned closure, so the scope follows the
/// latest snapshot from which a slot was derived.
fn scoped_private_intake_acquire<F>(
    scopes: Arc<std::sync::Mutex<PrivateIntakeEventScopes>>,
    acquire: F,
) -> PrivateIntakeAcquire
where
    F: Fn(
            PubkyPublicKey,
            Option<PrivateIntakeCursorRecord>,
        ) -> PrivateIntakeScopedAcquisitionFuture
        + Send
        + Sync
        + 'static,
{
    Arc::new(move |peer, cursor| {
        let scopes = Arc::clone(&scopes);
        let acquisition = acquire(peer.clone(), cursor);
        Box::pin(async move {
            let (read_folder, outcome) = acquisition.await;
            if let Ok(mut scopes) = scopes.lock() {
                match (read_folder, &outcome) {
                    (Some(read_folder), _) => {
                        scopes.read_folders.insert(peer, read_folder);
                    }
                    // A pending Encrypted Link Handshake has no read folder
                    // yet. A slot whose path lacks the derived folder shape
                    // also lands here.
                    (None, Ok(_)) => {
                        scopes.read_folders.remove(&peer);
                    }
                    // The acquisition failed before deriving the slot: a
                    // storage, handshake, authorization, marker or
                    // observation error, or the peer has no private intake
                    // checkpoint or no Encrypted Link snapshot (both are
                    // `RecoveryRequired` errors). Keep the previous entry,
                    // or none, so a flaky homeserver does not turn the
                    // filter off and re-enable a fetch per unrelated event.
                    // A stale folder only delays a packet: handshake slots
                    // in any folder still pass, audits still fetch the
                    // expected slot, and every audit or acquisition that
                    // derives the slot replaces the entry.
                    (None, Err(_)) => {}
                }
            }
            outcome
        })
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PrivateIntakeAuditOutcome {
    handshake_pending: bool,
    backpressured: bool,
    /// Peers whose audit ended in `PrivatePacketAcquisition::Absent`. Only an
    /// absent expected slot proves the audit read past every earlier event of
    /// the peer, which is what lets the caller commit the peer's ignored
    /// cursor; any other outcome stopped short of that slot.
    absent_slot_peers: HashSet<PubkyPublicKey>,
}

type PrivateIntakeAuditFuture = Pin<Box<dyn Future<Output = PrivateIntakeAuditOutcome> + Send>>;
type PrivateIntakeAudit = Arc<dyn Fn() -> PrivateIntakeAuditFuture + Send + Sync>;

async fn process_private_intake_event<F, Fut>(
    group: &SubscriptionGroup,
    scopes: &std::sync::Mutex<PrivateIntakeEventScopes>,
    ignored: &mut PrivateIntakeIgnoredCursors,
    event: pubky::Event,
    acquire: &mut F,
) -> Option<(PubkyPublicKey, Result<PrivatePacketAcquisition>)>
where
    F: FnMut(PubkyPublicKey, Option<PrivateIntakeCursorRecord>) -> Fut,
    Fut: Future<Output = Result<PrivatePacketAcquisition>>,
{
    match route_private_intake_event(group, scopes, event)? {
        PrivateIntakeEventRoute::Hint(cursor) => {
            let peer = cursor.subscribed_user.clone();
            let event_id = cursor.event_id;
            let outcome = acquire(peer.clone(), Some(cursor)).await;
            // A successful acquisition for this event supersedes the peer's
            // ignored events, so a later audit does not write a cursor behind
            // the one this acquisition may have stored, which storage rejects
            // as a regression after a wasted transaction. Dropping an ignored
            // ID can only mean committing less: the worst case is replaying
            // irrelevant events after a reconnect, never a skipped packet. A
            // failed acquisition keeps the entry. The `<=` check is defensive
            // only: the stream delivers events in cursor order, so an ignored
            // ID is never newer than this event.
            if outcome.is_ok()
                && ignored
                    .get(&peer)
                    .is_some_and(|&ignored_id| ignored_id <= event_id)
            {
                ignored.remove(&peer);
            }
            Some((peer, outcome))
        }
        PrivateIntakeEventRoute::Ignored { peer, event_id } => {
            record_ignored_private_intake_event(ignored, peer, event_id);
            None
        }
    }
}

/// Acquire the next expected slot of `snapshot`'s link and return the read
/// folder (the parent of the snapshot's next expected slot) with the outcome.
/// The folder is `None` when that slot cannot be derived, or when the derived
/// slot's path does not have the derived Encrypted Link folder shape
/// (`private_intake_slot_folder` returns `None`). In the second case the
/// acquisition still runs, and a successful outcome leaves the peer
/// unfiltered (see `scoped_private_intake_acquire`). A failure after
/// derivation keeps the folder.
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
) -> (Option<String>, Result<PrivatePacketAcquisition>)
where
    S: StorageAdapter,
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<Option<Vec<u8>>>>,
{
    let (next_resource, snapshot_counter) =
        match snapshot.next_private_application_message_resource(local, secret) {
            Ok(next) => next,
            Err(error) => return (None, Err(error.into())),
        };
    let read_folder = private_intake_slot_folder(&next_resource);
    let outcome = async move {
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
        let (resource, receive_counter) = snapshot
            .private_application_message_resource_at_counter(local, secret, receive_counter)?;
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
    .await;
    (read_folder, outcome)
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
    event_scopes: &std::sync::Mutex<PrivateIntakeEventScopes>,
    commit_cursors: PrivateIntakeCommitCursors,
    mut handshake_pending: bool,
) -> PrivateIntakeEventLoopExit {
    let mut ignored = PrivateIntakeIgnoredCursors::new();
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
                let outcome =
                    audit_private_intake_stream(
                        &audit_group,
                        &group.homeserver_name,
                        &mut ignored,
                        &commit_cursors,
                    )
                    .await;
                if outcome.backpressured {
                    return PrivateIntakeEventLoopExit::Backpressured;
                }
                handshake_pending = outcome.handshake_pending;
            }
            _ = handshake_retry.tick(), if handshake_pending => {
                let outcome =
                    audit_private_intake_stream(
                        &audit_group,
                        &group.homeserver_name,
                        &mut ignored,
                        &commit_cursors,
                    )
                    .await;
                if outcome.backpressured {
                    return PrivateIntakeEventLoopExit::Backpressured;
                }
                handshake_pending = outcome.handshake_pending;
            }
            event = stream.next() => match event {
                Some(Ok(event)) => {
                    let mut acquire_one = |peer, cursor| acquire(peer, cursor);
                    match process_private_intake_event(
                        group,
                        event_scopes,
                        &mut ignored,
                        event,
                        &mut acquire_one,
                    )
                    .await
                    {
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

/// Replace the stored admissions with `admitted_homeservers`, carrying forward
/// the stored admission of every still-supervised peer the map does not place.
async fn replace_private_intake_admissions<S: StorageAdapter, C: Clock>(
    storage: &S,
    clock: &C,
    lease_id: u64,
    mut admitted_homeservers: HashMap<PubkyPublicKey, String>,
) -> Result<bool> {
    let clock = clock.clone();
    storage
        .transaction(move |tx| {
            // `admitted_homeservers` was computed from a peer snapshot taken
            // before the homeserver lookups, so it can miss a peer whose
            // lookup failed or whose Encrypted Link Handshake became pending,
            // through another SDK handle, while the lookups ran. That handshake
            // reserved capacity through `reserve_private_intake_admission`.
            // Dropping it here would let identity-wide capacity be
            // over-admitted until a later refresh. So the carry-forward is
            // decided from current storage, atomically with the replacement:
            // a peer the map does not place keeps its stored admission while
            // it is still supervised (pending handshake or established link).
            // Only an admission already stored is kept; none is invented.
            // Admissions of peers no longer supervised (cancelled handshake,
            // unlinked, blocked) are not carried, so they are released.
            let state = tx.export_storage_state();
            for (peer, homeserver) in &state.private_intake_admitted_homeservers {
                if !admitted_homeservers.contains_key(peer)
                    && private_intake_peer_phase(&state, peer).is_some()
                {
                    admitted_homeservers.insert(peer.clone(), homeserver.clone());
                }
            }
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
    /// Resolve every supervised peer's Pubky homeserver.
    ///
    /// Identity-wide failures (session, Noise key, key authorization) fail the
    /// pass. A per-peer lookup failure only leaves that peer unresolved, or on
    /// its last resolved homeserver when `current` already streams it.
    async fn resolve_private_intake_topology(
        &self,
        current: &[SubscriptionGroup],
    ) -> Result<PrivateIntakeTopology> {
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
        // Release the session read guard before the per-peer network lookups.
        let client = access.outbox_client.clone();
        drop(access);
        let mut lookups = Vec::new();
        for peer in peers.expect("active session reads private intake peers") {
            let lookup = match peer.to_public_key() {
                Ok(remote) => client.get_homeserver_of(&remote).await.map_err(|error| {
                    PaykitSdkError::Transport {
                        context: "resolve private intake homeserver".into(),
                        source: Some(error.into()),
                    }
                }),
                Err(error) => Err(error),
            };
            lookups.push((peer, lookup));
        }
        Ok(private_intake_topology_from_lookups(lookups, current))
    }

    /// Start caller-owned SSE-assisted intake for every established Linked Peer.
    ///
    /// SSE events are hints only. Each hint and healthy audit fetches only next
    /// expected immutable Noise slot, then commits ciphertext with per-user cursor.
    /// No Noise receive checkpoint advances until explicit buffered processing.
    ///
    /// A Linked Peer whose Pubky homeserver cannot be resolved reserves no
    /// stream and is reported `Stale`. It is retried on each topology refresh,
    /// and its failed lookup does not stop intake for other peers. When it
    /// later resolves, it is placed like any other topology change, so an
    /// over-capacity result marks the resolved topology stale and stops
    /// supervision. A topology refresh keeps the stored handshake admission of
    /// every peer that is still supervised but that the refresh did not
    /// place, such as a peer whose homeserver lookup failed or a handshake
    /// that became pending while the refresh was resolving, so reserved
    /// capacity stays reserved.
    pub async fn start_private_intake(
        self: &Arc<Self>,
        config: PrivateIntakeConfig,
    ) -> Result<PrivateIntakeSupervisor> {
        config.validate()?;
        let topology = self.resolve_private_intake_topology(&[]).await?;
        let groups = &topology.groups;
        // Capacity and admissions come from streamed groups only: an
        // unresolved peer has no homeserver to admit and reserves no stream.
        let reserved_streams = reserve_stream_capacity(config.stream_capacity, groups.len())?;
        let lease = claim_private_intake_lease(
            &self.storage,
            self.clock.now(),
            config.stream_capacity,
            admitted_homeservers(groups),
        )
        .await?;
        let lease_id = lease.lease_id;
        let mut initial_health = PrivateIntakeHealth {
            reserved_streams,
            ..PrivateIntakeHealth::default()
        };
        apply_private_intake_topology_health(&mut initial_health, groups, &topology.unresolved);
        let health = Arc::new(AsyncRwLock::new(initial_health));
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
                        topology,
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
        topology: PrivateIntakeTopology,
        config: PrivateIntakeConfig,
        lease_id: u64,
        mut stop: watch::Receiver<bool>,
        health: Arc<AsyncRwLock<PrivateIntakeHealth>>,
        notifications: PrivateIntakeNotifications,
    ) {
        let PrivateIntakeTopology {
            mut groups,
            mut unresolved,
        } = topology;
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

            apply_private_intake_topology_health(&mut *health.write().await, &groups, &unresolved);
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
                        match self.resolve_private_intake_topology(&groups).await {
                            Ok(next) if next.groups == groups => {
                                // Same streams: only the unresolved set may
                                // have changed, so no epoch restart.
                                unresolved = next.unresolved;
                                apply_private_intake_topology_health(
                                    &mut *health.write().await,
                                    &groups,
                                    &unresolved,
                                );
                            }
                            Ok(next) => {
                                unresolved = next.unresolved;
                                let resolved = next.groups;
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
                            // Only identity-wide failures reach this arm; one
                            // peer's lookup failure leaves that peer unresolved.
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
        // Event scopes live for this group task, not for one connection, so
        // read folders recorded on one connection still filter after an SSE
        // reconnect even when the next startup audit fails before deriving a
        // slot. The task starts unfiltered; every acquisition, from the first
        // startup audit on, records the read folder of the snapshot it used.
        // A topology change, including a homeserver move, or a capacity
        // release stops this task through its epoch, and the replacement task
        // starts with an empty map. `group.peers` is fixed for the task; only
        // its subscribable subset changes between connections. A peer that
        // drops out keeps an unused entry, because routing only consults the
        // connection's subscribed peers, and a peer without an entry stays
        // unfiltered. A stale folder only delays a packet: handshake slots in
        // any folder still pass, audits still fetch the expected slot, and
        // every acquisition that derives the slot replaces the entry. The
        // acquisition and cursor-commit callbacks hold no per-connection
        // state, so they are also built once per task.
        let event_scopes = Arc::new(std::sync::Mutex::new(PrivateIntakeEventScopes::default()));
        let acquire = scoped_private_intake_acquire(Arc::clone(&event_scopes), {
            let sdk = Arc::clone(&self);
            move |peer, cursor| {
                let sdk = Arc::clone(&sdk);
                Box::pin(async move { sdk.acquire_expected_private_packet(peer, cursor).await })
            }
        });
        let commit_cursors: PrivateIntakeCommitCursors = {
            let sdk = Arc::clone(&self);
            Arc::new(move |cursors| {
                let sdk = Arc::clone(&sdk);
                Box::pin(async move {
                    commit_ignored_private_intake_cursors(&sdk.storage, cursors).await;
                })
            })
        };
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
                .path(private_intake_event_path_filter())
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
                            || {
                                audit_private_intake_group(
                                    &self.storage,
                                    &self.clock,
                                    &active_group,
                                    &health,
                                    &work,
                                    &acquire,
                                )
                            },
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
            let audit_outcome = audit_private_intake_group(
                &self.storage,
                &self.clock,
                &active_group,
                &health,
                &work,
                &acquire,
            )
            .await;
            if audit_outcome.backpressured {
                topology_changed.notify_one();
                if wait_reconnect_or_stop(&mut stop).await {
                    break;
                }
                continue;
            }
            let audit_group: PrivateIntakeAudit = {
                let sdk = Arc::clone(&self);
                let group = active_group.clone();
                let health = Arc::clone(&health);
                let work = Arc::clone(&work);
                let acquire = Arc::clone(&acquire);
                Arc::new(move || {
                    let sdk = Arc::clone(&sdk);
                    let group = group.clone();
                    let health = Arc::clone(&health);
                    let work = Arc::clone(&work);
                    let acquire = Arc::clone(&acquire);
                    Box::pin(async move {
                        audit_private_intake_group(
                            &sdk.storage,
                            &sdk.clock,
                            &group,
                            &health,
                            &work,
                            &acquire,
                        )
                        .await
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
                Arc::clone(&acquire),
                audit_group,
                &event_scopes,
                Arc::clone(&commit_cursors),
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

    /// Acquire `counterparty`'s next expected slot and return the read folder
    /// of the snapshot it used. The folder is `None` when the peer is pending
    /// an Encrypted Link Handshake, has no snapshot, the acquisition failed
    /// before deriving the slot, or the derived slot's path does not have the
    /// derived Encrypted Link folder shape (a successful acquisition then
    /// leaves the peer unfiltered).
    async fn acquire_expected_private_packet(
        &self,
        counterparty: PubkyPublicKey,
        cursor: Option<PrivateIntakeCursorRecord>,
    ) -> (Option<String>, Result<PrivatePacketAcquisition>) {
        // An error before the expected slot is derived leaves no read folder;
        // `acquire_expected_private_packet_with_fetch` reports it from there.
        let acquisition = async move {
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
                    return Ok((None, Ok(PrivatePacketAcquisition::PendingHandshake)));
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
            let remote = counterparty.to_public_key()?;
            let public_storage = access.outbox_client.public_storage();
            let (authorization, marker) = tokio::join!(
                noise_key_authorization::require_authorization(&public_storage, &remote),
                paykit_lib::fetch_encrypted_link_recovery_marker(
                    &public_storage,
                    &secret,
                    local,
                    &remote,
                    snapshot.link.remote_noise_public_key(),
                ),
            );
            let authorization = authorization.map_err(super::recovery::link_observation_error)?;
            let marker = self
                .preserve_authorization_advance_on_marker_failure(
                    &counterparty,
                    &access,
                    snapshot.state.generation,
                    &snapshot.authorization,
                    &authorization,
                    marker.map_err(|error| super::recovery::link_observation_error(error.into())),
                )
                .await?;
            if !super::private_stream::private_receive_observation_is_current(
                &snapshot,
                &authorization,
                marker.as_ref(),
            ) {
                let lease = self.claim_peer_link_operation(&counterparty).await?;
                let result = self
                    .require_current_private_receive_observation(
                        &counterparty,
                        &lease,
                        &access,
                        super::private_stream::PrivateReceiveObservation::new(
                            snapshot.state.generation,
                            &snapshot.authorization,
                            &authorization,
                            marker.as_ref(),
                        ),
                    )
                    .await;
                self.finish_peer_link_operation(lease, result).await?;
            }
            let generation = snapshot.state.generation;
            let acquisition = acquire_expected_private_packet_with_fetch(
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
            .await;
            Ok(acquisition)
        };
        acquisition.await.unwrap_or_else(|error| (None, Err(error)))
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

    fn no_cursor_commit() -> PrivateIntakeCommitCursors {
        Arc::new(|_| Box::pin(async {}))
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

    fn homeserver_lookup_error() -> Result<Option<paykit_lib::PublicKey>> {
        Err(PaykitSdkError::Transport {
            context: "resolve private intake homeserver".into(),
            source: None,
        })
    }

    fn grouped_peers(groups: &[SubscriptionGroup]) -> Vec<PubkyPublicKey> {
        groups
            .iter()
            .flat_map(|group| group.peers.iter().cloned())
            .collect()
    }

    fn assert_topology_skips_unresolved_peer(lookup: Result<Option<paykit_lib::PublicKey>>) {
        let homeserver_a = pubky::Keypair::random().public_key();
        let homeserver_b = pubky::Keypair::random().public_key();
        let peers = users(3);

        let topology = private_intake_topology_from_lookups(
            vec![
                (peers[0].clone(), Ok(Some(homeserver_a))),
                (peers[1].clone(), lookup),
                (peers[2].clone(), Ok(Some(homeserver_b))),
            ],
            &[],
        );

        assert_eq!(topology.groups.len(), 2);
        assert!(!grouped_peers(&topology.groups).contains(&peers[1]));
        assert_eq!(topology.unresolved, vec![peers[1].clone()]);
    }

    #[test]
    fn test_private_intake_topology_skips_peer_without_homeserver() {
        assert_topology_skips_unresolved_peer(Ok(None));
    }

    #[test]
    fn test_private_intake_topology_skips_peer_on_lookup_error() {
        assert_topology_skips_unresolved_peer(homeserver_lookup_error());
    }

    #[test]
    fn test_private_intake_topology_keeps_covered_peer_homeserver_on_lookup_failure() {
        let first_homeserver = pubky::Keypair::random().public_key();
        let second_homeserver = pubky::Keypair::random().public_key();
        let peers = users(2);
        let (resolving_peer, failing_peer) = (&peers[0], &peers[1]);

        // Groups are ordered by homeserver name, so run both assignments: in
        // one of them the failing peer's group is not the first group.
        for (homeserver_a, homeserver_b) in [
            (&first_homeserver, &second_homeserver),
            (&second_homeserver, &first_homeserver),
        ] {
            let current = group_private_intake_peers_by_homeserver(vec![
                (
                    homeserver_a.to_string(),
                    homeserver_a.clone(),
                    resolving_peer.clone(),
                ),
                (
                    homeserver_b.to_string(),
                    homeserver_b.clone(),
                    failing_peer.clone(),
                ),
            ]);

            for failed_lookup in [Ok(None), homeserver_lookup_error()] {
                let topology = private_intake_topology_from_lookups(
                    vec![
                        (resolving_peer.clone(), Ok(Some(homeserver_a.clone()))),
                        (failing_peer.clone(), failed_lookup),
                    ],
                    &current,
                );

                assert!(topology.groups == current);
                assert!(topology.unresolved.is_empty());
                let failing_group = topology
                    .groups
                    .iter()
                    .find(|group| group.peers.contains(failing_peer))
                    .expect("failing peer keeps a stream");
                assert_eq!(failing_group.homeserver_name, homeserver_b.to_string());
                assert!(!failing_group.peers.contains(resolving_peer));
            }
        }
    }

    #[test]
    fn test_private_intake_topology_groups_peer_once_homeserver_resolves() {
        let homeserver_a = pubky::Keypair::random().public_key();
        let homeserver_b = pubky::Keypair::random().public_key();
        let peers = users(2);

        let first = private_intake_topology_from_lookups(
            vec![
                (peers[0].clone(), Ok(Some(homeserver_a.clone()))),
                (peers[1].clone(), Ok(None)),
            ],
            &[],
        );
        assert_eq!(first.groups.len(), 1);
        assert_eq!(first.unresolved, vec![peers[1].clone()]);

        let second = private_intake_topology_from_lookups(
            vec![
                (peers[0].clone(), Ok(Some(homeserver_a))),
                (peers[1].clone(), Ok(Some(homeserver_b))),
            ],
            &first.groups,
        );

        assert!(second.groups != first.groups);
        assert_eq!(second.groups.len(), 2);
        assert!(grouped_peers(&second.groups).contains(&peers[1]));
        assert!(second.unresolved.is_empty());
    }

    #[tokio::test]
    async fn test_private_intake_health_reports_unresolved_peer_stale() {
        let homeserver = pubky::Keypair::random().public_key();
        let peers = users(4);
        let (new_streamed, caught_up_streamed, unresolved, removed) =
            (&peers[0], &peers[1], &peers[2], &peers[3]);
        let groups = group_private_intake_peers_by_homeserver(
            [new_streamed, caught_up_streamed]
                .into_iter()
                .map(|peer| (homeserver.to_string(), homeserver.clone(), peer.clone()))
                .collect(),
        );
        let audited_at = Utc::now();
        let mut health = PrivateIntakeHealth {
            peers: HashMap::from([
                (
                    caught_up_streamed.clone(),
                    PrivateIntakeTransportState::CaughtUp,
                ),
                (unresolved.clone(), PrivateIntakeTransportState::CaughtUp),
                (removed.clone(), PrivateIntakeTransportState::CaughtUp),
            ]),
            reserved_streams: 3,
            last_successful_audit_at: HashMap::from([
                (caught_up_streamed.clone(), audited_at),
                (removed.clone(), audited_at),
            ]),
        };

        apply_private_intake_topology_health(
            &mut health,
            &groups,
            std::slice::from_ref(unresolved),
        );

        assert_eq!(health.reserved_streams, groups.len());
        assert_eq!(health.peers.len(), 3);
        assert_eq!(
            health.peers[new_streamed],
            PrivateIntakeTransportState::Connecting
        );
        assert_eq!(
            health.peers[caught_up_streamed],
            PrivateIntakeTransportState::CaughtUp
        );
        assert_eq!(health.peers[unresolved], PrivateIntakeTransportState::Stale);
        assert!(!health.peers.contains_key(removed));
        assert_eq!(
            health.last_successful_audit_at,
            HashMap::from([(caught_up_streamed.clone(), audited_at)])
        );

        let (stop, _) = watch::channel(false);
        let supervisor = PrivateIntakeSupervisor {
            lease_id: 1,
            stop,
            tasks: AsyncMutex::new(Vec::new()),
            task_handles: Vec::new(),
            health: Arc::new(AsyncRwLock::new(health)),
            work: Arc::new(Notify::new()),
            capacity_released: Arc::new(Notify::new()),
        };
        assert_eq!(
            supervisor.peer_freshness(unresolved).await,
            Some(PrivateIntakePeerFreshness {
                transport_state: PrivateIntakeTransportState::Stale,
                last_successful_audit_at: None,
            })
        );
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
    fn test_private_intake_event_path_filter_selects_private_directory() {
        let filter = private_intake_event_path_filter();
        assert_eq!(
            filter.strip_suffix('/'),
            Some(paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX),
            "the filter must be the private routing prefix in directory form"
        );
        assert!(
            !filter.ends_with("//"),
            "the filter must end in exactly one slash, got {filter:?}"
        );

        // The homeserver parses each `path` filter with `StoragePath::normalize`
        // and treats only a directory-shaped path as a prefix of event paths.
        use pubky_common::storage_path::StoragePath;
        let slot_path = StoragePath::new(&format!(
            "{}/{}/0",
            paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
            "ab".repeat(32)
        ))
        .unwrap();
        let filter_path = StoragePath::normalize(&filter).unwrap();
        assert!(filter_path.is_directory());
        assert!(filter_path.covers_path(&slot_path));

        // Without the trailing slash the homeserver matches one exact file.
        let file_path = StoragePath::normalize(paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX).unwrap();
        assert!(!file_path.is_directory());
        assert!(!file_path.covers_path(&slot_path));
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
    fn test_sse_observation_rejects_rotated_authorization_or_recovery_context() {
        let identity = pubky::Keypair::random();
        let current =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[7; 32], 1).unwrap();
        let rotated =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[8; 32], 2).unwrap();
        let marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap();
        let other_marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap();
        let recovery =
            paykit_lib::EncryptedLinkRecoveryContext::new(None, Some(marker.attempt_id())).unwrap();

        assert!(
            super::super::private_stream::private_receive_observation_matches(
                &current,
                &recovery,
                &current,
                Some(&marker),
            )
        );
        assert!(
            !super::super::private_stream::private_receive_observation_matches(
                &current,
                &recovery,
                &rotated,
                Some(&marker),
            )
        );
        assert!(
            !super::super::private_stream::private_receive_observation_matches(
                &current,
                &recovery,
                &current,
                Some(&other_marker),
            )
        );
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
    fn test_private_intake_config_rejects_event_limit_below_largest_event() {
        let config = |max_event_bytes| PrivateIntakeConfig {
            stream_capacity: 1,
            max_event_bytes,
            healthy_audit_interval: std::time::Duration::from_secs(60),
        };
        assert!(config(0).validate().is_err());
        assert!(config(MIN_EVENT_BYTES - 1).validate().is_err());
        assert!(config(MIN_EVENT_BYTES).validate().is_ok());
        assert!(config(MAX_EVENT_BYTES).validate().is_ok());
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

    /// Store `peer` as a pending Encrypted Link Handshake, reserving its
    /// admission on `homeserver` in the same transaction, as
    /// `save_link_handshake_state_inner` does.
    async fn seed_pending_handshake(
        storage: &crate::storage::InMemoryStorage,
        peer: PubkyPublicKey,
        homeserver: Option<&str>,
        now: DateTime<Utc>,
    ) {
        let homeserver = homeserver.map(str::to_owned);
        storage
            .transaction(move |tx| {
                if let Some(homeserver) = homeserver {
                    assert!(tx.reserve_private_intake_admission(peer.clone(), homeserver, now)?);
                }
                tx.save_linked_peer(LinkedPeerRecord {
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(now),
                    ..default_linked_peer(peer.clone())
                });
                tx.save_encrypted_link_state(crate::storage::EncryptedLinkStateRecord {
                    counterparty: peer,
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                    generation: 0,
                    checkpointed_at: now,
                });
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_private_intake_admission_replacement_keeps_supervised_peer_admission() {
        let storage = crate::storage::InMemoryStorage::new();
        let peers = users(4);
        let (streamed, pending, never_admitted, unsupervised) =
            (&peers[0], &peers[1], &peers[2], &peers[3]);
        let now = Utc::now();
        let lease = claim_private_intake_lease(
            &storage,
            now,
            2,
            HashMap::from([(streamed.clone(), "homeserver-1".to_owned())]),
        )
        .await
        .unwrap();
        seed_pending_handshake(&storage, pending.clone(), Some("homeserver-2"), now).await;
        seed_pending_handshake(&storage, never_admitted.clone(), None, now).await;
        // An admission whose peer has no pending handshake or link state, as
        // after a cancelled handshake, must be released by the replacement.
        let released_peer = unsupervised.clone();
        assert!(storage
            .transaction(move |tx| {
                tx.reserve_private_intake_admission(released_peer, "homeserver-2".into(), now)
            })
            .await
            .unwrap());
        let (_stop_tx, mut stop_rx) = watch::channel(false);

        assert!(replace_private_intake_admissions_with_retry(
            &storage,
            &MutableClock::new(now),
            lease.lease_id,
            HashMap::from([(streamed.clone(), "homeserver-1".to_owned())]),
            &mut stop_rx,
        )
        .await
        .unwrap());

        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .private_intake_admitted_homeservers,
            HashMap::from([
                (streamed.clone(), "homeserver-1".to_owned()),
                (pending.clone(), "homeserver-2".to_owned()),
            ])
        );
    }

    #[tokio::test]
    async fn test_private_intake_admission_replacement_counts_supervised_peer_admission_toward_capacity(
    ) {
        let storage = crate::storage::InMemoryStorage::new();
        let peers = users(3);
        let (streamed, pending, newcomer) = (&peers[0], &peers[1], &peers[2]);
        let now = Utc::now();
        let lease = claim_private_intake_lease(
            &storage,
            now,
            2,
            HashMap::from([(streamed.clone(), "homeserver-1".to_owned())]),
        )
        .await
        .unwrap();
        seed_pending_handshake(&storage, pending.clone(), Some("homeserver-2"), now).await;
        let before = storage
            .snapshot()
            .unwrap()
            .private_intake_admitted_homeservers;
        let (_stop_tx, mut stop_rx) = watch::channel(false);

        // The resolved map alone needs two streams and fits, but the carried
        // admission on "homeserver-2" makes it three.
        let result = replace_private_intake_admissions_with_retry(
            &storage,
            &MutableClock::new(now),
            lease.lease_id,
            HashMap::from([
                (streamed.clone(), "homeserver-1".to_owned()),
                (newcomer.clone(), "homeserver-3".to_owned()),
            ]),
            &mut stop_rx,
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .private_intake_admitted_homeservers,
            before
        );
    }

    #[tokio::test]
    async fn test_private_intake_admission_replacement_keeps_handshake_reserved_during_resolution()
    {
        let storage = crate::storage::InMemoryStorage::new();
        let peers = users(3);
        let (streamed, newcomer, late_handshake) = (&peers[0], &peers[1], &peers[2]);
        let now = Utc::now();
        let lease = claim_private_intake_lease(
            &storage,
            now,
            3,
            HashMap::from([(streamed.clone(), "homeserver-1".to_owned())]),
        )
        .await
        .unwrap();
        // The map a refresh pass computed from its peer snapshot, taken before
        // the late handshake existed.
        let stale_replacement = HashMap::from([
            (streamed.clone(), "homeserver-1".to_owned()),
            (newcomer.clone(), "homeserver-3".to_owned()),
        ]);
        // While that pass resolves homeservers, another SDK handle over the
        // same identity state makes a handshake pending and reserves it.
        let alternate_handle = storage.clone();
        seed_pending_handshake(
            &alternate_handle,
            late_handshake.clone(),
            Some("homeserver-2"),
            now,
        )
        .await;
        let (_stop_tx, mut stop_rx) = watch::channel(false);

        assert!(replace_private_intake_admissions_with_retry(
            &storage,
            &MutableClock::new(now),
            lease.lease_id,
            stale_replacement,
            &mut stop_rx,
        )
        .await
        .unwrap());

        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .private_intake_admitted_homeservers,
            HashMap::from([
                (streamed.clone(), "homeserver-1".to_owned()),
                (newcomer.clone(), "homeserver-3".to_owned()),
                (late_handshake.clone(), "homeserver-2".to_owned()),
            ])
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
                        backpressured,
                        ..PrivateIntakeAuditOutcome::default()
                    }
                })
            }
        });
        let scopes = StdMutex::default();
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
            &scopes,
            no_cursor_commit(),
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

        let Some(PrivateIntakeEventRoute::Hint(cursor)) =
            route_private_intake_event(&group, &StdMutex::default(), event)
        else {
            panic!("an unscoped subscribed peer's event must be a hint");
        };
        assert_eq!(cursor.subscribed_user, peer);
        assert_eq!(cursor.homeserver, "homeserver-a");
        assert_eq!(cursor.event_id, 42);
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

        assert!(route_private_intake_event(&group, &StdMutex::default(), event).is_none());
    }

    fn intake_event(
        event_type: pubky::EventType,
        resource: pubky::PubkyResource,
        event_id: u64,
    ) -> pubky::Event {
        pubky::Event {
            event_type,
            resource,
            cursor: pubky::EventCursor::new(event_id),
        }
    }

    fn put_event(resource: pubky::PubkyResource, event_id: u64) -> pubky::Event {
        intake_event(
            pubky::EventType::Put {
                content_hash: blake3::Hash::from_bytes([7; 32]),
            },
            resource,
            event_id,
        )
    }

    fn peer_resource(owner: &pubky::PublicKey, path: &str) -> pubky::PubkyResource {
        pubky::PubkyResource::new(owner.clone(), path).unwrap()
    }

    /// An established link's peer, its read folder, and its expected slot.
    struct ScopedIntakeFixture {
        remote: pubky::PublicKey,
        read_folder: String,
        expected_slot: pubky::PubkyResource,
    }

    fn scoped_intake_fixture() -> ScopedIntakeFixture {
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let (snapshot, secret) = intake_snapshot(&local, &remote, 3);
        let (expected_slot, _) = snapshot
            .next_private_application_message_resource(&local, &secret)
            .unwrap();
        let read_folder = private_intake_slot_folder(&expected_slot).unwrap();
        let expected_slot: pubky::PubkyResource = expected_slot.parse().unwrap();
        // The read folder is the parent of the next expected slot.
        assert!(is_private_intake_link_folder(&read_folder));
        assert_eq!(expected_slot.owner, remote);
        assert_eq!(expected_slot.path.as_str(), format!("{read_folder}/3"));
        ScopedIntakeFixture {
            remote,
            read_folder,
            expected_slot,
        }
    }

    fn other_link_folder(read_folder: &str) -> String {
        let other = format!(
            "{}/{}",
            paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
            "ab".repeat(32)
        );
        assert_ne!(other, read_folder);
        other
    }

    /// Events under a scoped peer's private tree that cannot announce its
    /// next expected slot.
    fn irrelevant_intake_events(fixture: &ScopedIntakeFixture) -> Vec<pubky::Event> {
        let owner = &fixture.remote;
        let folder = &fixture.read_folder;
        let other_folder = other_link_folder(folder);
        let receipt = paykit_lib::ReceiptAccess::location_for(&paykit_lib::ReceiptId::new_v4());
        vec![
            put_event(peer_resource(owner, &format!("{other_folder}/3")), 10),
            put_event(peer_resource(owner, &receipt), 11),
            intake_event(pubky::EventType::Delete, fixture.expected_slot.clone(), 12),
            put_event(peer_resource(owner, &format!("{folder}0")), 13),
            put_event(peer_resource(owner, &format!("{folder}0/3")), 14),
        ]
    }

    #[test]
    fn test_private_intake_event_filter_accepts_put_in_read_folder() {
        let fixture = scoped_intake_fixture();
        let scope = Some(fixture.read_folder.as_str());

        assert!(private_intake_event_may_announce_packet(
            scope,
            &put_event(fixture.expected_slot.clone(), 1),
        ));
        let later_slot = peer_resource(&fixture.remote, &format!("{}/4", fixture.read_folder));
        assert!(private_intake_event_may_announce_packet(
            scope,
            &put_event(later_slot, 2),
        ));
    }

    #[test]
    fn test_private_intake_event_filter_ignores_irrelevant_events() {
        let fixture = scoped_intake_fixture();

        for event in irrelevant_intake_events(&fixture) {
            assert!(
                !private_intake_event_may_announce_packet(Some(&fixture.read_folder), &event),
                "event {} must not be a hint",
                event.cursor.id()
            );
        }
    }

    #[test]
    fn test_private_intake_event_filter_accepts_handshake_slot_in_any_folder() {
        // pubky-noise 0.1.0 XX handshakes use slots 0, 1, and 2; transport
        // counters start at 3, which the `intake_snapshot` fixture mirrors.
        assert_eq!(
            ENCRYPTED_LINK_HANDSHAKE_MESSAGE_COUNT,
            pubky_noise::snow_crypto::full_handshake_actions(
                pubky_noise::snow_crypto::HandshakePattern::PatternXX,
                true,
            )
            .unwrap()
            .len()
        );
        let fixture = scoped_intake_fixture();
        let owner = &fixture.remote;
        let scope = Some(fixture.read_folder.as_str());
        let other_folder = other_link_folder(&fixture.read_folder);

        for folder in [&fixture.read_folder, &other_folder] {
            for slot in 0..3 {
                let resource = peer_resource(owner, &format!("{folder}/{slot}"));
                assert!(private_intake_event_may_announce_packet(
                    scope,
                    &put_event(resource, 1),
                ));
            }
        }
        let not_handshake_slots = [
            format!("{other_folder}/3"),
            format!("{other_folder}/01"),
            format!("{other_folder}/0/1"),
            format!("{}/0", other_folder.to_uppercase()),
            format!("{}/0", &other_folder[..other_folder.len() - 1]),
        ];
        for path in not_handshake_slots {
            assert!(
                !private_intake_event_may_announce_packet(
                    scope,
                    &put_event(peer_resource(owner, &path), 1),
                ),
                "{path} must not be a handshake slot"
            );
        }
    }

    #[test]
    fn test_private_intake_event_route_leaves_unscoped_peer_unfiltered() {
        let fixture = scoped_intake_fixture();
        let scoped_peer = PubkyPublicKey::from_public_key(&fixture.remote);
        let unscoped_owner = pubky::Keypair::random().public_key();
        let unscoped_peer = PubkyPublicKey::from_public_key(&unscoped_owner);
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![scoped_peer.clone(), unscoped_peer.clone()],
        };
        let scopes = StdMutex::new(PrivateIntakeEventScopes {
            read_folders: HashMap::from([(scoped_peer.clone(), fixture.read_folder.clone())]),
        });

        for event in irrelevant_intake_events(&fixture) {
            let event_id = event.cursor.id();
            assert!(matches!(
                route_private_intake_event(&group, &scopes, event),
                Some(PrivateIntakeEventRoute::Ignored { peer, event_id: ignored_id })
                    if peer == scoped_peer && ignored_id == event_id
            ));
        }
        for event in irrelevant_intake_events(&fixture) {
            let event = intake_event(
                event.event_type,
                peer_resource(&unscoped_owner, event.resource.path.as_str()),
                event.cursor.id(),
            );
            let event_id = event.cursor.id();
            assert!(matches!(
                route_private_intake_event(&group, &scopes, event),
                Some(PrivateIntakeEventRoute::Hint(cursor))
                    if cursor.subscribed_user == unscoped_peer && cursor.event_id == event_id
            ));
        }

        // A poisoned scope lock leaves even the scoped peer unfiltered.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = scopes.lock().unwrap();
            panic!("poison private intake event scopes");
        }));
        assert!(scopes.is_poisoned());
        for event in irrelevant_intake_events(&fixture) {
            assert!(matches!(
                route_private_intake_event(&group, &scopes, event),
                Some(PrivateIntakeEventRoute::Hint(_))
            ));
        }
    }

    #[tokio::test]
    async fn test_expected_slot_fetch_without_derivation_reports_no_read_folder() {
        let storage = crate::storage::InMemoryStorage::new();
        let local = pubky::Keypair::random().public_key();
        let remote = pubky::Keypair::random().public_key();
        let peer = PubkyPublicKey::from_public_key(&remote);
        let (snapshot, _) = intake_snapshot(&local, &remote, 3);
        seed_intake_link(&storage, peer.clone(), &snapshot, 7).await;
        let fetched = Arc::new(StdMutex::new(false));

        let (read_folder, outcome) = acquire_expected_private_packet_with_fetch(
            &storage,
            peer,
            &snapshot,
            &local,
            &[9; 32],
            7,
            None,
            Utc::now(),
            {
                let fetched = Arc::clone(&fetched);
                move |_| {
                    *fetched.lock().unwrap() = true;
                    async { Ok(None) }
                }
            },
        )
        .await;

        assert!(outcome.is_err());
        assert_eq!(read_folder, None);
        assert!(!*fetched.lock().unwrap());
    }

    #[test]
    fn test_private_intake_slot_folder_requires_derived_folder_shape() {
        let owner = pubky::Keypair::random().public_key().z32();
        let folder = format!(
            "{}/{}",
            paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
            "ab".repeat(32)
        );

        assert_eq!(
            private_intake_slot_folder(&format!("pubky://{owner}{folder}/3")),
            Some(folder.clone())
        );
        for path in [
            paykit_lib::ReceiptAccess::location_for(&paykit_lib::ReceiptId::new_v4()),
            format!("{}/3", folder.to_uppercase()),
            format!("{folder}/0/3"),
        ] {
            assert_eq!(
                private_intake_slot_folder(&format!("pubky://{owner}{path}")),
                None,
                "{path} must not have a derived folder"
            );
        }
    }

    #[tokio::test]
    async fn test_production_stream_loop_ignores_irrelevant_events_without_acquisition() {
        let fixture = scoped_intake_fixture();
        let peer = PubkyPublicKey::from_public_key(&fixture.remote);
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![peer.clone()],
        };
        let scopes = StdMutex::new(PrivateIntakeEventScopes {
            read_folders: HashMap::from([(peer.clone(), fixture.read_folder.clone())]),
        });
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let acquire: PrivateIntakeAcquire = Arc::new({
            let observed = Arc::clone(&observed);
            move |requested, cursor| {
                observed.lock().unwrap().push((requested, cursor));
                Box::pin(async { Ok(PrivatePacketAcquisition::Absent) })
            }
        });
        let audit: PrivateIntakeAudit =
            Arc::new(|| Box::pin(async { PrivateIntakeAuditOutcome::default() }));
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let (_stop, stop_rx) = watch::channel(false);
        let run = |events: Vec<pubky::Event>| {
            run_connected_private_intake_stream(
                &group,
                Box::pin(stream::iter(events.into_iter().map(Ok))),
                Duration::from_secs(60),
                Duration::from_secs(60),
                stop_rx.clone(),
                &health,
                &work,
                Arc::clone(&acquire),
                Arc::clone(&audit),
                &scopes,
                no_cursor_commit(),
                false,
            )
        };

        let ignored_exit = run(irrelevant_intake_events(&fixture)).await;
        assert!(matches!(ignored_exit, PrivateIntakeEventLoopExit::Ended));
        assert!(observed.lock().unwrap().is_empty());
        assert!(health.read().await.peers.is_empty());

        let mut events = irrelevant_intake_events(&fixture);
        events.push(put_event(fixture.expected_slot.clone(), 99));
        let exit = run(events).await;

        assert!(matches!(exit, PrivateIntakeEventLoopExit::Ended));
        let observed = std::mem::take(&mut *observed.lock().unwrap());
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].0, peer);
        assert_eq!(observed[0].1.as_ref().unwrap().event_id, 99);
        assert_eq!(
            health.read().await.peers[&peer],
            PrivateIntakeTransportState::CaughtUp
        );
    }

    fn recovery_required() -> PaykitSdkError {
        PaykitSdkError::RecoveryRequired {
            context: "test acquisition failure".into(),
            source: None,
        }
    }

    #[tokio::test]
    async fn test_private_intake_acquisition_refreshes_read_folder_before_next_event() {
        let fixture = scoped_intake_fixture();
        let peer = PubkyPublicKey::from_public_key(&fixture.remote);
        let new_folder = other_link_folder(&fixture.read_folder);
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![peer.clone()],
        };
        let scopes = Arc::new(StdMutex::new(PrivateIntakeEventScopes {
            read_folders: HashMap::from([(peer.clone(), fixture.read_folder.clone())]),
        }));
        let observed = Arc::new(StdMutex::new(Vec::new()));
        // A recovery moved the link to `new_folder`: the peer's handshake
        // reply completes it and the acquisition reports the new folder.
        let acquire = scoped_private_intake_acquire(Arc::clone(&scopes), {
            let observed = Arc::clone(&observed);
            let new_folder = new_folder.clone();
            move |_, cursor: Option<PrivateIntakeCursorRecord>| {
                observed.lock().unwrap().push(cursor.unwrap().event_id);
                let new_folder = new_folder.clone();
                Box::pin(async move { (Some(new_folder), Ok(PrivatePacketAcquisition::Absent)) })
            }
        });
        let events = vec![
            put_event(
                peer_resource(&fixture.remote, &format!("{new_folder}/1")),
                20,
            ),
            put_event(
                peer_resource(&fixture.remote, &format!("{new_folder}/3")),
                21,
            ),
            put_event(fixture.expected_slot.clone(), 22),
        ];
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let (_stop, stop_rx) = watch::channel(false);

        let exit = run_connected_private_intake_stream(
            &group,
            Box::pin(stream::iter(events.into_iter().map(Ok))),
            Duration::from_secs(60),
            Duration::from_secs(60),
            stop_rx,
            &health,
            &work,
            acquire,
            Arc::new(|| Box::pin(async { PrivateIntakeAuditOutcome::default() })),
            &scopes,
            no_cursor_commit(),
            false,
        )
        .await;

        assert!(matches!(exit, PrivateIntakeEventLoopExit::Ended));
        // The transport slot in the new folder is a hint without an audit in
        // between, and the old folder's slot no longer is.
        assert_eq!(*observed.lock().unwrap(), vec![20, 21]);
        assert_eq!(
            scopes.lock().unwrap().read_folders.get(&peer),
            Some(&new_folder)
        );
    }

    #[tokio::test]
    async fn test_private_intake_pending_acquisition_leaves_peer_unfiltered() {
        let fixture = scoped_intake_fixture();
        let peer = PubkyPublicKey::from_public_key(&fixture.remote);
        let scopes = Arc::new(StdMutex::new(PrivateIntakeEventScopes {
            read_folders: HashMap::from([(peer.clone(), fixture.read_folder.clone())]),
        }));
        let acquire = scoped_private_intake_acquire(Arc::clone(&scopes), |_, _| {
            Box::pin(async { (None, Ok(PrivatePacketAcquisition::PendingHandshake)) })
        });

        let outcome = acquire(peer.clone(), None).await;

        assert!(matches!(
            outcome,
            Ok(PrivatePacketAcquisition::PendingHandshake)
        ));
        assert!(!scopes.lock().unwrap().read_folders.contains_key(&peer));
    }

    #[tokio::test]
    async fn test_private_intake_failed_acquisition_keeps_read_folder() {
        let fixture = scoped_intake_fixture();
        let peer = PubkyPublicKey::from_public_key(&fixture.remote);
        let scopes = Arc::new(StdMutex::new(PrivateIntakeEventScopes::default()));
        let acquire = scoped_private_intake_acquire(Arc::clone(&scopes), |_, _| {
            Box::pin(async { (None, Err(recovery_required())) })
        });

        // Without a previous folder the peer stays unfiltered.
        assert!(acquire(peer.clone(), None).await.is_err());
        assert!(!scopes.lock().unwrap().read_folders.contains_key(&peer));

        // A previous folder survives the error.
        scopes
            .lock()
            .unwrap()
            .read_folders
            .insert(peer.clone(), fixture.read_folder.clone());
        assert!(acquire(peer.clone(), None).await.is_err());
        assert_eq!(
            scopes.lock().unwrap().read_folders.get(&peer),
            Some(&fixture.read_folder)
        );
    }

    #[tokio::test]
    async fn test_private_intake_audit_refreshes_read_folders() {
        let fixture = scoped_intake_fixture();
        let established = PubkyPublicKey::from_public_key(&fixture.remote);
        let failing = users(1).remove(0);
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![established.clone(), failing.clone()],
        };
        let scopes = Arc::new(StdMutex::new(PrivateIntakeEventScopes {
            read_folders: HashMap::from([(
                failing.clone(),
                other_link_folder(&fixture.read_folder),
            )]),
        }));
        let acquire = scoped_private_intake_acquire(Arc::clone(&scopes), {
            let established = established.clone();
            let read_folder = fixture.read_folder.clone();
            move |requested, _| {
                let outcome = if requested == established {
                    (
                        Some(read_folder.clone()),
                        Ok(PrivatePacketAcquisition::Absent),
                    )
                } else {
                    (None, Err(recovery_required()))
                };
                Box::pin(async move { outcome })
            }
        });
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));

        let outcome = audit_private_intake_group(
            &crate::storage::InMemoryStorage::new(),
            &MutableClock::new(Utc::now()),
            &group,
            &health,
            &Arc::new(Notify::new()),
            &acquire,
        )
        .await;

        assert_eq!(
            outcome.absent_slot_peers,
            HashSet::from([established.clone()])
        );
        let scopes = scopes.lock().unwrap();
        assert_eq!(
            scopes.read_folders.get(&established),
            Some(&fixture.read_folder)
        );
        // A failure before slot derivation keeps the previous folder.
        assert_eq!(
            scopes.read_folders.get(&failing),
            Some(&other_link_folder(&fixture.read_folder))
        );
    }

    #[tokio::test]
    async fn test_private_intake_audit_commits_ignored_cursors_only_after_absent_slot() {
        let peers = users(4);
        let (absent, pending, capacity, inserting) = (
            peers[0].clone(),
            peers[1].clone(),
            peers[2].clone(),
            peers[3].clone(),
        );
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: peers.clone(),
        };
        let acquire: PrivateIntakeAcquire = Arc::new({
            let (absent, pending, capacity) = (absent.clone(), pending.clone(), capacity.clone());
            move |requested, _| {
                let outcome = if requested == absent {
                    PrivatePacketAcquisition::Absent
                } else if requested == pending {
                    PrivatePacketAcquisition::PendingHandshake
                } else if requested == capacity {
                    PrivatePacketAcquisition::CapacityReached
                } else {
                    // Every fetch finds another packet, so the audit stops at
                    // the per-peer acquisition cap.
                    PrivatePacketAcquisition::Inserted
                };
                Box::pin(async move { Ok(outcome) })
            }
        });
        let audit: PrivateIntakeAudit = Arc::new(move || {
            let group = group.clone();
            let acquire = Arc::clone(&acquire);
            Box::pin(async move {
                audit_private_intake_group(
                    &crate::storage::InMemoryStorage::new(),
                    &MutableClock::new(Utc::now()),
                    &group,
                    &Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default())),
                    &Arc::new(Notify::new()),
                    &acquire,
                )
                .await
            })
        });
        let cursor = |peer: &PubkyPublicKey, event_id| PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-a".into(),
            event_id,
        };
        let mut ignored = PrivateIntakeIgnoredCursors::new();
        for (peer, event_id) in peers.iter().zip([10, 20, 30, 40]) {
            record_ignored_private_intake_event(&mut ignored, peer.clone(), event_id);
        }
        let batches = Arc::new(StdMutex::new(Vec::new()));
        let commit_cursors: PrivateIntakeCommitCursors = Arc::new({
            let batches = Arc::clone(&batches);
            move |cursors| {
                batches.lock().unwrap().push(cursors);
                Box::pin(async {})
            }
        });

        let outcome =
            audit_private_intake_stream(&audit, "homeserver-a", &mut ignored, &commit_cursors)
                .await;

        // Only the peer whose expected slot was absent counts as audited;
        // a pending handshake, full capacity or the acquisition cap leave
        // the ignored cursor in the stream's map (a capacity stop then ends
        // the stream, which drops it).
        assert_eq!(outcome.absent_slot_peers, HashSet::from([absent.clone()]));
        assert_eq!(
            std::mem::take(&mut *batches.lock().unwrap()),
            vec![vec![cursor(&absent, 10)]]
        );
        assert_eq!(
            ignored,
            HashMap::from([
                (pending.clone(), 20),
                (capacity.clone(), 30),
                (inserting, 40)
            ])
        );
    }

    /// Drives a stream loop whose audits block until released.
    struct IgnoredCursorScript {
        events_tx: tokio::sync::mpsc::UnboundedSender<pubky::Event>,
        acquired_rx: tokio::sync::mpsc::UnboundedReceiver<Option<u64>>,
        started_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
        release_tx: tokio::sync::mpsc::UnboundedSender<PrivateIntakeAuditOutcome>,
    }

    impl IgnoredCursorScript {
        /// Wait for the next audit and release it with `outcome`.
        async fn audit(&mut self, outcome: PrivateIntakeAuditOutcome) {
            self.started_rx.recv().await.unwrap();
            self.release_tx.send(outcome).unwrap();
        }

        /// Send `events`, then `hint`, and wait until the hint is acquired,
        /// so the loop has handled every earlier event and committed what
        /// earlier audits allowed. Audits that start meanwhile fail, which
        /// keeps every ignored cursor.
        async fn send(&mut self, events: Vec<pubky::Event>, hint: pubky::Event) {
            for event in events {
                self.events_tx.send(event).unwrap();
            }
            let event_id = hint.cursor.id();
            self.events_tx.send(hint).unwrap();
            loop {
                tokio::select! {
                    acquired = self.acquired_rx.recv() => {
                        assert_eq!(acquired.unwrap(), Some(event_id));
                        return;
                    }
                    _ = self.started_rx.recv() => {
                        self.release_tx.send(PrivateIntakeAuditOutcome::default()).unwrap();
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn test_private_intake_audit_commits_highest_ignored_cursor_seen_before_it() {
        let fixture = scoped_intake_fixture();
        let peer = PubkyPublicKey::from_public_key(&fixture.remote);
        let other_folder = other_link_folder(&fixture.read_folder);
        let ignored = |event_id| {
            put_event(
                peer_resource(&fixture.remote, &format!("{other_folder}/{event_id}")),
                event_id,
            )
        };
        // Sync points come from a second, unfiltered peer, so their
        // successful acquisitions never supersede `peer`'s ignored cursors.
        let sync_owner = pubky::Keypair::random().public_key();
        let hint = |event_id| {
            put_event(
                peer_resource(&sync_owner, &format!("{}/{event_id}", fixture.read_folder)),
                event_id,
            )
        };
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![peer.clone(), PubkyPublicKey::from_public_key(&sync_owner)],
        };
        let scopes = StdMutex::new(PrivateIntakeEventScopes {
            read_folders: HashMap::from([(peer.clone(), fixture.read_folder.clone())]),
        });
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let events = stream::unfold(events_rx, |mut events_rx| async move {
            events_rx.recv().await.map(|event| (Ok(event), events_rx))
        });
        let (acquired_tx, acquired_rx) = tokio::sync::mpsc::unbounded_channel();
        let acquire: PrivateIntakeAcquire = Arc::new(move |_, cursor| {
            acquired_tx
                .send(cursor.map(|cursor: PrivateIntakeCursorRecord| cursor.event_id))
                .unwrap();
            Box::pin(async { Ok(PrivatePacketAcquisition::Absent) })
        });
        // Each audit reports its start and blocks until the test releases it
        // with an outcome.
        let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) =
            tokio::sync::mpsc::unbounded_channel::<PrivateIntakeAuditOutcome>();
        let release_rx = Arc::new(AsyncMutex::new(release_rx));
        let audit: PrivateIntakeAudit = Arc::new(move || {
            let started_tx = started_tx.clone();
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                started_tx.send(()).unwrap();
                release_rx.lock().await.recv().await.unwrap_or_default()
            })
        });
        let commits = Arc::new(StdMutex::new(Vec::new()));
        let commit_cursors: PrivateIntakeCommitCursors = Arc::new({
            let commits = Arc::clone(&commits);
            move |cursors| {
                commits.lock().unwrap().push(cursors);
                Box::pin(async {})
            }
        });
        let audited = PrivateIntakeAuditOutcome {
            absent_slot_peers: HashSet::from([peer.clone()]),
            ..PrivateIntakeAuditOutcome::default()
        };
        let health = Arc::new(AsyncRwLock::new(PrivateIntakeHealth::default()));
        let work = Arc::new(Notify::new());
        let (stop, stop_rx) = watch::channel(false);
        let running = run_connected_private_intake_stream(
            &group,
            Box::pin(events),
            Duration::from_millis(20),
            Duration::ZERO,
            stop_rx,
            &health,
            &work,
            acquire,
            audit,
            &scopes,
            commit_cursors,
            false,
        );
        // Event ids per commit batch, one batch per storage transaction.
        let committed_ids = || {
            commits
                .lock()
                .unwrap()
                .iter()
                .map(|batch| batch.iter().map(|cursor| cursor.event_id).collect())
                .collect::<Vec<Vec<_>>>()
        };

        let mut script = IgnoredCursorScript {
            events_tx,
            acquired_rx,
            started_rx,
            release_tx,
        };
        let script_run = async {
            // An audit with no ignored events commits nothing.
            script.audit(audited.clone()).await;
            script.send(Vec::new(), hint(9)).await;
            assert!(committed_ids().is_empty());

            // A completed audit commits the highest ignored cursor seen
            // before it, once.
            script.send(vec![ignored(10), ignored(11)], hint(12)).await;
            assert!(committed_ids().is_empty());
            script.audit(audited.clone()).await;
            script.send(vec![ignored(13)], hint(14)).await;
            assert_eq!(committed_ids(), vec![vec![11]]);

            // Ignored events after an audit wait for the next one, and a
            // failed audit keeps them.
            script.audit(PrivateIntakeAuditOutcome::default()).await;
            script.send(Vec::new(), hint(15)).await;
            assert_eq!(committed_ids(), vec![vec![11]]);
            script.audit(audited.clone()).await;
            script.send(Vec::new(), hint(16)).await;
            assert_eq!(committed_ids(), vec![vec![11], vec![13]]);

            script.audit(audited.clone()).await;
            script.send(Vec::new(), hint(17)).await;
            assert_eq!(committed_ids(), vec![vec![11], vec![13]]);
        };

        tokio::pin!(running);
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                exit = &mut running => panic!("stream loop exited early: {exit:?}"),
                () = script_run => {}
            }
        })
        .await
        .expect("ignored cursor script should finish");
        let commits = std::mem::take(&mut *commits.lock().unwrap());
        assert!(commits
            .iter()
            .flatten()
            .all(|cursor| cursor.subscribed_user == peer && cursor.homeserver == "homeserver-a"));
        stop.send(true).unwrap();
        // An audit may be waiting for release; dropping its sender ends it.
        // The event sender stays alive so the stream does not end first.
        drop(script.release_tx);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), running)
                .await
                .unwrap(),
            PrivateIntakeEventLoopExit::Stopped
        ));
    }

    #[tokio::test]
    async fn test_private_intake_successful_hint_supersedes_earlier_ignored_cursor() {
        let fixture = scoped_intake_fixture();
        let peer = PubkyPublicKey::from_public_key(&fixture.remote);
        let other_folder = other_link_folder(&fixture.read_folder);
        let ignored_event = |event_id| {
            put_event(
                peer_resource(&fixture.remote, &format!("{other_folder}/{event_id}")),
                event_id,
            )
        };
        let hint_event = |event_id| {
            put_event(
                peer_resource(
                    &fixture.remote,
                    &format!("{}/{event_id}", fixture.read_folder),
                ),
                event_id,
            )
        };
        let group = SubscriptionGroup {
            homeserver: pubky::Keypair::random().public_key(),
            homeserver_name: "homeserver-a".into(),
            peers: vec![peer.clone()],
        };
        let scopes = StdMutex::new(PrivateIntakeEventScopes {
            read_folders: HashMap::from([(peer.clone(), fixture.read_folder.clone())]),
        });
        let audit: PrivateIntakeAudit = Arc::new({
            let peer = peer.clone();
            move || {
                let audited = PrivateIntakeAuditOutcome {
                    absent_slot_peers: HashSet::from([peer.clone()]),
                    ..PrivateIntakeAuditOutcome::default()
                };
                Box::pin(async move { audited })
            }
        });
        let commits = Arc::new(StdMutex::new(Vec::<Vec<u64>>::new()));
        let commit_cursors: PrivateIntakeCommitCursors = Arc::new({
            let commits = Arc::clone(&commits);
            move |cursors: Vec<PrivateIntakeCursorRecord>| {
                commits
                    .lock()
                    .unwrap()
                    .push(cursors.iter().map(|cursor| cursor.event_id).collect());
                Box::pin(async {})
            }
        });
        let mut succeed = |_: PubkyPublicKey, _: Option<PrivateIntakeCursorRecord>| async {
            Ok(PrivatePacketAcquisition::Absent)
        };
        let mut fail = |_: PubkyPublicKey, _: Option<PrivateIntakeCursorRecord>| async {
            Err(PaykitSdkError::Transport {
                context: "fetch expected private intake slot".into(),
                source: None,
            })
        };
        let mut ignored = PrivateIntakeIgnoredCursors::new();
        let committed_ids = || std::mem::take(&mut *commits.lock().unwrap());

        // A successful hint supersedes the ignored event before it, so the
        // next audit commits nothing for the peer.
        process_private_intake_event(
            &group,
            &scopes,
            &mut ignored,
            ignored_event(10),
            &mut succeed,
        )
        .await;
        assert_eq!(ignored, HashMap::from([(peer.clone(), 10)]));
        process_private_intake_event(&group, &scopes, &mut ignored, hint_event(11), &mut succeed)
            .await;
        assert!(ignored.is_empty());
        audit_private_intake_stream(&audit, "homeserver-a", &mut ignored, &commit_cursors).await;
        assert!(committed_ids().is_empty());

        // An ignored event after the hint still commits, and so does one
        // with a higher ID than a successful hint.
        process_private_intake_event(
            &group,
            &scopes,
            &mut ignored,
            ignored_event(12),
            &mut succeed,
        )
        .await;
        audit_private_intake_stream(&audit, "homeserver-a", &mut ignored, &commit_cursors).await;
        assert_eq!(committed_ids(), vec![vec![12]]);
        process_private_intake_event(
            &group,
            &scopes,
            &mut ignored,
            ignored_event(14),
            &mut succeed,
        )
        .await;
        process_private_intake_event(&group, &scopes, &mut ignored, hint_event(13), &mut succeed)
            .await;
        assert_eq!(ignored, HashMap::from([(peer.clone(), 14)]));
        audit_private_intake_stream(&audit, "homeserver-a", &mut ignored, &commit_cursors).await;
        assert_eq!(committed_ids(), vec![vec![14]]);

        // A hint whose acquisition fails keeps the ignored event.
        process_private_intake_event(
            &group,
            &scopes,
            &mut ignored,
            ignored_event(20),
            &mut succeed,
        )
        .await;
        let failed =
            process_private_intake_event(&group, &scopes, &mut ignored, hint_event(21), &mut fail)
                .await;
        assert!(matches!(failed, Some((_, Err(_)))));
        assert_eq!(ignored, HashMap::from([(peer.clone(), 20)]));
        audit_private_intake_stream(&audit, "homeserver-a", &mut ignored, &commit_cursors).await;
        assert_eq!(committed_ids(), vec![vec![20]]);
    }

    #[tokio::test]
    async fn test_private_intake_audit_commits_ignored_cursors_in_one_batch() {
        let peers = users(3);
        let cursor = |peer: &PubkyPublicKey, event_id| PrivateIntakeCursorRecord {
            subscribed_user: peer.clone(),
            homeserver: "homeserver-a".into(),
            event_id,
        };
        let mut ignored = PrivateIntakeIgnoredCursors::new();
        for (peer, event_id) in peers.iter().zip([10, 20, 30]) {
            record_ignored_private_intake_event(&mut ignored, peer.clone(), event_id);
        }
        let audit: PrivateIntakeAudit = Arc::new({
            let absent_slot_peers = HashSet::from([peers[0].clone(), peers[1].clone()]);
            move || {
                let absent_slot_peers = absent_slot_peers.clone();
                Box::pin(async move {
                    PrivateIntakeAuditOutcome {
                        absent_slot_peers,
                        ..PrivateIntakeAuditOutcome::default()
                    }
                })
            }
        });
        let batches = Arc::new(StdMutex::new(Vec::new()));
        let commit_cursors: PrivateIntakeCommitCursors = Arc::new({
            let batches = Arc::clone(&batches);
            move |cursors| {
                batches.lock().unwrap().push(cursors);
                Box::pin(async {})
            }
        });

        audit_private_intake_stream(&audit, "homeserver-a", &mut ignored, &commit_cursors).await;

        // Both audited peers' cursors go to storage in one batch; the peer
        // the audit did not complete keeps its ignored cursor.
        let mut batches = std::mem::take(&mut *batches.lock().unwrap());
        assert_eq!(batches.len(), 1);
        batches[0].sort_by_key(|cursor| cursor.event_id);
        assert_eq!(
            batches[0],
            vec![cursor(&peers[0], 10), cursor(&peers[1], 20)]
        );
        assert_eq!(ignored, HashMap::from([(peers[2].clone(), 30)]));
    }

    #[tokio::test]
    async fn test_private_intake_ignored_cursor_commit_skips_rejected_cursors() {
        let storage = crate::storage::InMemoryStorage::new();
        let peers = users(4);
        let cursor =
            |peer: &PubkyPublicKey, homeserver: &str, event_id| PrivateIntakeCursorRecord {
                subscribed_user: peer.clone(),
                homeserver: homeserver.into(),
                event_id,
            };
        let stored = [
            cursor(&peers[0], "homeserver-a", 50),
            cursor(&peers[1], "homeserver-b", 5),
        ];
        storage
            .transaction({
                let stored = stored.clone();
                move |tx| {
                    for cursor in stored {
                        tx.advance_private_intake_cursor(cursor)?;
                    }
                    Ok(())
                }
            })
            .await
            .unwrap();

        commit_ignored_private_intake_cursors(
            &storage,
            vec![
                // Regresses below the stored cursor.
                cursor(&peers[0], "homeserver-a", 40),
                // Names another homeserver than the stored cursor.
                cursor(&peers[1], "homeserver-a", 60),
                cursor(&peers[2], "homeserver-a", 10),
                cursor(&peers[3], "homeserver-a", 20),
            ],
        )
        .await;

        let cursors = storage.snapshot().unwrap().private_intake_cursors;
        assert_eq!(cursors[&peers[0]], stored[0]);
        assert_eq!(cursors[&peers[1]], stored[1]);
        assert_eq!(cursors[&peers[2]], cursor(&peers[2], "homeserver-a", 10));
        assert_eq!(cursors[&peers[3]], cursor(&peers[3], "homeserver-a", 20));
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
    async fn test_startup_audit_reports_inserted_work_before_absence_or_failure() {
        let peer = users(1).remove(0);
        let outcomes = Arc::new(StdMutex::new(vec![
            Ok(PrivatePacketAcquisition::Absent),
            Ok(PrivatePacketAcquisition::Inserted),
            Ok(PrivatePacketAcquisition::Inserted),
        ]));
        let observed = Arc::new(StdMutex::new(Vec::new()));

        let result = audit_private_intake_peer(peer.clone(), {
            let outcomes = Arc::clone(&outcomes);
            let observed = Arc::clone(&observed);
            move |requested, cursor| {
                let outcome = outcomes.lock().unwrap().pop().unwrap();
                observed.lock().unwrap().push((requested, cursor));
                async move { outcome }
            }
        })
        .await;

        assert!(result.inserted);
        assert!(matches!(
            result.outcome,
            Ok(PrivatePacketAcquisition::Absent)
        ));
        assert_eq!(observed.lock().unwrap().len(), 3);
        assert!(observed
            .lock()
            .unwrap()
            .iter()
            .all(|(requested, cursor)| requested == &peer && cursor.is_none()));

        let outcomes = Arc::new(StdMutex::new(vec![
            Err(PaykitSdkError::Transport {
                context: "later fetch failed".into(),
                source: None,
            }),
            Ok(PrivatePacketAcquisition::Inserted),
        ]));
        let result = audit_private_intake_peer(peer, {
            let outcomes = Arc::clone(&outcomes);
            move |_, _| {
                let outcome = outcomes.lock().unwrap().pop().unwrap();
                async move { outcome }
            }
        })
        .await;

        assert!(result.inserted);
        assert!(result.outcome.is_err());
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
            &StdMutex::default(),
            no_cursor_commit(),
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
        let scopes = StdMutex::default();
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
            &scopes,
            no_cursor_commit(),
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

        let (read_folder, outcome) = acquire_expected_private_packet_with_fetch(
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
        .await;

        assert!(matches!(outcome, Ok(PrivatePacketAcquisition::Inserted)));
        // The read folder is the parent of the snapshot's next expected slot.
        assert_eq!(read_folder, private_intake_slot_folder(&expected));
        assert!(read_folder.is_some());
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
        .1
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
            .1
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

        let (read_folder, result) = acquire_expected_private_packet_with_fetch(
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
        // A failure after the slot is derived keeps the peer's read folder.
        assert!(read_folder.is_some());
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
        .await
        .1;

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
        .await
        .1;

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
        .1
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
                    .1
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
            &StdMutex::default(),
            no_cursor_commit(),
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
                    .1
                }
            }
        })
        .await;

        assert!(outcome.inserted);
        assert!(matches!(
            outcome.outcome,
            Ok(PrivatePacketAcquisition::Absent)
        ));
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
                    .1
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
            &StdMutex::default(),
            no_cursor_commit(),
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
            &StdMutex::default(),
            no_cursor_commit(),
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
            &StdMutex::default(),
            no_cursor_commit(),
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
