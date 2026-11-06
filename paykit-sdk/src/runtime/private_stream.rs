use super::recovery::link_observation_error;
use super::*;
use crate::{
    domain::private_stream::persist_private_stream_batch_in_transaction,
    storage::retry_storage_transaction,
};
use futures_util::{stream, StreamExt};
use paykit_lib::PaykitNoiseKeyAuthorization as NoiseAuthorization;

const PRIVATE_INBOX_PROBE_CONCURRENCY: usize = 16;

pub(super) struct PrivateReceiveSnapshot {
    pub(super) link: paykit_lib::EncryptedLinkSnapshot,
    pub(super) state: EncryptedLinkStateRecord,
    pub(super) authorization: paykit_lib::PaykitNoiseKeyAuthorization,
    has_peer_lease: bool,
}

pub(super) struct PrivateReceiveContext {
    pub(super) session_access: GuardedSessionAccess,
    pub(super) snapshot: PrivateReceiveSnapshot,
}

enum PrivateInboxProbe {
    Empty(Box<PrivateReceiveSnapshot>),
    Ready(Box<PrivateReceiveSnapshot>),
    Reload,
}

pub(super) fn private_receive_observation_matches(
    expected_authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
    recovery_context: &paykit_lib::EncryptedLinkRecoveryContext,
    authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
    marker: Option<&paykit_lib::EncryptedLinkRecoveryMarker>,
) -> bool {
    authorization == expected_authorization
        && marker
            .is_none_or(|marker| Some(marker.attempt_id()) == recovery_context.remote_attempt_id())
}

pub(super) fn private_receive_observation_is_current(
    snapshot: &PrivateReceiveSnapshot,
    authorization: &NoiseAuthorization,
    marker: Option<&paykit_lib::EncryptedLinkRecoveryMarker>,
) -> bool {
    private_receive_observation_matches(
        &snapshot.authorization,
        snapshot.link.recovery_context(),
        authorization,
        marker,
    )
}

pub(super) struct PrivateReceiveObservation<'a> {
    expected_link_generation: u64,
    expected_authorization: &'a NoiseAuthorization,
    authorization: &'a NoiseAuthorization,
    marker: Result<Option<&'a paykit_lib::EncryptedLinkRecoveryMarker>>,
}

impl<'a> PrivateReceiveObservation<'a> {
    pub(super) fn new(
        expected_link_generation: u64,
        expected_authorization: &'a NoiseAuthorization,
        authorization: &'a NoiseAuthorization,
        marker: Option<&'a paykit_lib::EncryptedLinkRecoveryMarker>,
    ) -> Self {
        Self {
            expected_link_generation,
            expected_authorization,
            authorization,
            marker: Ok(marker),
        }
    }

    pub(super) fn with_marker_result(
        expected_link_generation: u64,
        expected_authorization: &'a NoiseAuthorization,
        authorization: &'a NoiseAuthorization,
        marker: Result<Option<&'a paykit_lib::EncryptedLinkRecoveryMarker>>,
    ) -> Self {
        Self {
            expected_link_generation,
            expected_authorization,
            authorization,
            marker,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PrivateReceiveObservationStatus {
    Current,
    RecoveryRequired,
    ReloadMarker { link_generation: u64 },
}

pub(super) async fn require_current_private_receive_observation_with_lease<S, C>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    lease: &PeerLinkOperationLease,
    observation: PrivateReceiveObservation<'_>,
    clock: &C,
) -> Result<PrivateReceiveObservationStatus>
where
    S: StorageAdapter,
    C: Clock,
{
    let PrivateReceiveObservation {
        expected_link_generation,
        expected_authorization,
        authorization,
        marker,
    } = observation;
    let marker_was_read = marker.is_ok();
    let marker_attempt_id = marker
        .as_ref()
        .ok()
        .and_then(|marker| marker.as_ref())
        .map(|marker| marker.attempt_id().to_owned());
    // Commit the monotonic authorization pin and recovery transition together
    // under the peer lease before callers classify this observation as stale.
    let changed = retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let lease = lease.clone();
        let authorization = authorization.clone();
        let marker_attempt_id = marker_attempt_id.clone();
        move |tx| {
            let now = clock.now();
            crate::storage::require_peer_link_operation_lease(tx, &lease)?;
            let current_lease = tx
                .peer_link_operation_lease(&counterparty)
                .expect("required peer lease must remain present");
            if current_lease.expires_at <= now {
                return Err(PaykitSdkError::Policy {
                    context: "peer link operation lease expired".into(),
                    source: None,
                });
            }
            let peer = tx.linked_peer(&counterparty);
            let current_link_generation = tx
                .encrypted_link_state(&counterparty)
                .map(|state| state.generation);
            if let Some(link_generation) =
                current_link_generation.filter(|generation| *generation != expected_link_generation)
            {
                // A marker loaded through a superseded snapshot key says nothing
                // about the replacement link. Only defer to a marker reload when
                // that replacement checkpoint already uses the observed key.
                if private_receive_checkpoint_uses_authorization(tx, &counterparty, &authorization)
                {
                    return Ok(if marker_was_read {
                        PrivateReceiveObservationStatus::ReloadMarker { link_generation }
                    } else {
                        PrivateReceiveObservationStatus::Current
                    });
                }
                // Otherwise commit the monotonic pin and fail-closed recovery
                // transition atomically; never leave an old-key checkpoint Linked.
                noise_key_authorization::pin_authorization_in_transaction(
                    tx,
                    &counterparty,
                    &authorization,
                )?;
                mark_recovery_required_for_marker_in_transaction(
                    tx,
                    &counterparty,
                    now,
                    marker_attempt_id.as_deref(),
                )?;
                return Ok(PrivateReceiveObservationStatus::RecoveryRequired);
            }
            // A completed link may have superseded the pre-read snapshot while
            // public observations were in flight. Current matching authorization
            // and marker own that link.
            if peer.as_ref().is_some_and(|peer| {
                (peer.state == LinkedPeerState::Linked || peer.state == LinkedPeerState::Linking)
                    && peer.noise_key_authorization.as_ref() == Some(&authorization)
                    && (!marker_was_read
                        || peer.remote_recovery_attempt_id.as_deref()
                            == marker_attempt_id.as_deref())
            }) {
                return Ok(PrivateReceiveObservationStatus::Current);
            }
            noise_key_authorization::pin_authorization_in_transaction(
                tx,
                &counterparty,
                &authorization,
            )?;
            let peer = tx.linked_peer(&counterparty);
            let marker_changed = marker_was_read
                && marker_attempt_id.as_deref().is_some_and(|attempt_id| {
                    peer.as_ref()
                        .and_then(|peer| peer.remote_recovery_attempt_id.as_deref())
                        != Some(attempt_id)
                });
            if expected_authorization != &authorization
                || marker_changed
                || peer.is_none_or(|peer| peer.state != LinkedPeerState::RecoveryRequired)
            {
                mark_recovery_required_for_marker_in_transaction(
                    tx,
                    &counterparty,
                    now,
                    marker_attempt_id.as_deref(),
                )?;
            }
            Ok(PrivateReceiveObservationStatus::RecoveryRequired)
        }
    })
    .await?;

    match changed {
        PrivateReceiveObservationStatus::RecoveryRequired => {
            Err(PaykitSdkError::RecoveryRequired {
                context: "private intake peer authorization or recovery context changed".into(),
                source: None,
            })
        }
        PrivateReceiveObservationStatus::Current => marker.map(|_| changed),
        PrivateReceiveObservationStatus::ReloadMarker { .. } => Ok(changed),
    }
}

pub(super) struct PrivateInboxCheckpoint {
    identity: PubkyPublicKey,
    local_noise_public_key: PubkyPublicKey,
    state: EncryptedLinkStateRecord,
    authorization: paykit_lib::PaykitNoiseKeyAuthorization,
}

impl PrivateInboxCheckpoint {
    fn from_probe(
        session_access: &PubkySessionAccess,
        probe: &PrivateInboxProbe,
    ) -> Result<Option<Self>> {
        let PrivateInboxProbe::Empty(snapshot) = probe else {
            return Ok(None);
        };
        if snapshot.has_peer_lease {
            return Ok(None);
        }
        Ok(Some(Self {
            identity: session_access.public_key()?,
            local_noise_public_key: crate::storage::paykit_noise_public_key(
                &session_access.paykit_identity_secret_key().ok_or_else(|| {
                    PaykitSdkError::Identity {
                        context: "private receive requires the Paykit identity key".into(),
                        source: None,
                    }
                })?,
            ),
            state: snapshot.state.clone(),
            authorization: snapshot.authorization.clone(),
        }))
    }

    pub(super) fn is_current(&self, tx: &dyn StorageTransaction) -> bool {
        tx.load_identity_state()
            .and_then(|state| state.public_key)
            .as_ref()
            == Some(&self.identity)
            && require_private_receive_checkpoint(
                tx,
                &self.state,
                &self.authorization,
                &self.local_noise_public_key,
            )
            .is_ok()
    }
}

struct PrivateReceiveLink {
    link: paykit_lib::EncryptedLink,
    state: EncryptedLinkStateRecord,
    authorized_apps: Option<HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>>,
    authorization: paykit_lib::PaykitNoiseKeyAuthorization,
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    pub(super) async fn require_current_private_receive_observation(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
        observation: PrivateReceiveObservation<'_>,
    ) -> Result<()> {
        let authorization_changed = observation.expected_authorization != observation.authorization;
        let authorization = observation.authorization.clone();
        let remote_noise_public_key = authorization.noise_public_key().clone();
        let result = match require_current_private_receive_observation_with_lease(
            &self.storage,
            counterparty,
            lease,
            observation,
            &self.clock,
        )
        .await
        {
            Ok(PrivateReceiveObservationStatus::Current) => Ok(()),
            Ok(PrivateReceiveObservationStatus::ReloadMarker { link_generation }) => {
                let public_storage = session_access.outbox_client.public_storage();
                let secret = session_access.paykit_noise_secret_key()?;
                let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
                    &public_storage,
                    &secret,
                    session_access.session.info().public_key(),
                    &counterparty.to_public_key()?,
                    authorization.noise_public_key(),
                )
                .await
                .map_err(|error| link_observation_error(error.into()))?;
                match require_current_private_receive_observation_with_lease(
                    &self.storage,
                    counterparty,
                    lease,
                    PrivateReceiveObservation::new(
                        link_generation,
                        &authorization,
                        &authorization,
                        marker.as_ref(),
                    ),
                    &self.clock,
                )
                .await
                {
                    Ok(PrivateReceiveObservationStatus::Current) => Ok(()),
                    Ok(PrivateReceiveObservationStatus::ReloadMarker { .. }) => {
                        Err(PaykitSdkError::ConcurrentUpdate {
                            context: "Encrypted Link generation changed while peer lease was held"
                                .into(),
                            source: None,
                        })
                    }
                    Ok(PrivateReceiveObservationStatus::RecoveryRequired) => {
                        unreachable!("recovery-required observations return an error")
                    }
                    Err(error) => Err(error),
                }
            }
            Ok(PrivateReceiveObservationStatus::RecoveryRequired) => {
                unreachable!("recovery-required observations return an error")
            }
            Err(error) => Err(error),
        };
        if authorization_changed && matches!(&result, Err(PaykitSdkError::RecoveryRequired { .. }))
        {
            // Recovery is already durable. Marker publication is best-effort
            // liveness and must not replace the fail-closed observation result.
            let _ = self
                .publish_local_recovery_marker_with_key(
                    counterparty,
                    session_access,
                    lease,
                    &remote_noise_public_key,
                )
                .await;
        }
        result
    }

    pub(super) async fn preserve_authorization_advance_on_marker_failure(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        expected_link_generation: u64,
        expected_authorization: &NoiseAuthorization,
        authorization: &NoiseAuthorization,
        marker: Result<Option<paykit_lib::EncryptedLinkRecoveryMarker>>,
    ) -> Result<Option<paykit_lib::EncryptedLinkRecoveryMarker>> {
        let Err(error) = marker else {
            return marker;
        };
        if expected_authorization == authorization {
            return Err(error);
        }

        let lease = self.claim_peer_link_operation(counterparty).await?;
        let result = self
            .require_current_private_receive_observation(
                counterparty,
                &lease,
                session_access,
                PrivateReceiveObservation::with_marker_result(
                    expected_link_generation,
                    expected_authorization,
                    authorization,
                    Err(error),
                ),
            )
            .await;
        self.finish_peer_link_operation(lease, result).await?;
        Err(PaykitSdkError::LinkObservation {
            context: "recovery marker lookup failed during authorization advance".into(),
        })
    }

    /// Receive and durably persist available private messages.
    ///
    /// This requires a stored Encrypted Link snapshot for the counterparty.
    /// Handshake establishment and recovery are separate workflows.
    /// Empty inboxes are probed without acquiring a peer lease or writing state.
    /// Available messages are prepared read-only and committed only while the
    /// original link checkpoint remains current and no peer operation holds a lease.
    ///
    /// One call handles at most
    /// [`PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT`](paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT)
    /// messages, each committed with its own checkpoint; call again to drain more.
    /// A report with a `receive_batch_id` and no stream items means identical
    /// re-sends were consumed without new state, so more messages may be pending.
    /// Returns [`PaykitSdkError::Policy`] when the counterparty reached its
    /// retained private stream item limit and the local identity has neither
    /// transacted with it nor saved it as a contact. That message stays
    /// unacknowledged and messages committed earlier in the call stay committed.
    /// [`PaykitSdkError::is_retention_limit_reached`] identifies this refusal.
    /// Saving the counterparty as a contact resumes intake.
    /// [`Self::forget_peer`] removes what a blocked counterparty retained, and
    /// is refused when there is payment history with it.
    pub async fn receive_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivateStreamIntakeReport> {
        self.receive_private_messages_with_checkpoint(counterparty, None)
            .await
            .map(|(report, _)| report)
    }

    /// Process a bounded batch of ciphertext already stored by private intake.
    ///
    /// Application state, Event Message dedupe, Noise checkpoint, queue removal,
    /// and remaining packet generations commit atomically. The returned freshness
    /// comes from `supervisor`; an empty local queue does not imply remote absence.
    pub async fn receive_buffered_private_messages(
        &self,
        counterparty: PubkyPublicKey,
        supervisor: &PrivateIntakeSupervisor,
    ) -> Result<BufferedPrivateStreamIntakeReport> {
        let freshness = supervisor
            .peer_freshness(&counterparty)
            .await
            .ok_or_else(|| PaykitSdkError::PrivateIntakeModeConflict {
                counterparty: counterparty.clone(),
            })?;
        self.require_private_intake_supervisor_lease(supervisor.lease_id)
            .await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .receive_buffered_private_messages_with_claim(
                counterparty.clone(),
                supervisor.lease_id,
                lease.clone(),
                freshness,
                supervisor,
            )
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn require_private_intake_supervisor_lease(&self, lease_id: u64) -> Result<()> {
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| require_private_intake_supervisor_lease(tx, lease_id, now))
            .await
    }

    async fn receive_buffered_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        supervisor_lease_id: u64,
        lease: PeerLinkOperationLease,
        freshness: PrivateIntakePeerFreshness,
        supervisor: &PrivateIntakeSupervisor,
    ) -> Result<BufferedPrivateStreamIntakeReport> {
        let session_access = self
            .load_session_access_for_initialized_identity("receive buffered private messages")
            .await?;
        let counterparty_for_load = counterparty.clone();
        let lease_for_load = lease.clone();
        let now = self.clock.now();
        let snapshot = self
            .storage
            .transaction(move |tx| {
                require_private_intake_supervisor_lease(tx, supervisor_lease_id, now)?;
                crate::storage::require_peer_link_operation_lease(tx, &lease_for_load)?;
                private_receive_snapshot(tx, &counterparty_for_load).ok_or_else(|| {
                    PaykitSdkError::RecoveryRequired {
                        context: "buffered private intake has no current Encrypted Link".into(),
                        source: None,
                    }
                })
            })
            .await?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let local_public_key = session_access.session.info().public_key().clone();
        let remote_public_key = counterparty.to_public_key()?;
        let mut link = paykit_lib::restore_encrypted_link(
            session_access.session.clone(),
            secret_key,
            &remote_public_key,
            session_access.outbox_client.clone(),
            snapshot.link,
        )
        .await?;
        noise_key_authorization::validate_remote_static_key(
            &snapshot.authorization,
            link.remote_static_public_key(),
            true,
        )?;
        let mut stored_link_state = snapshot.state;
        // Refresh current App Registry authorization when reachable. Lookup
        // failure intentionally preserves cached authorization so already
        // durable ciphertext remains drainable during transport outages.
        let mut authorized_apps = self.private_receive_authorized_apps(&counterparty).await;
        let mut aggregate = PrivateStreamIntakeReport {
            receive_batch_id: None,
            stream_item_ids: Vec::new(),
            event_conflicts: Vec::new(),
        };

        for _ in 0..paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT {
            let counterparty_for_head = counterparty.clone();
            let lease_for_head = lease.clone();
            let expected_state = stored_link_state.clone();
            let now = self.clock.now();
            let Some(packet) = self
                .storage
                .transaction(move |tx| {
                    require_private_intake_supervisor_lease(tx, supervisor_lease_id, now)?;
                    crate::storage::require_peer_link_operation_lease(tx, &lease_for_head)?;
                    if tx.encrypted_link_state(&counterparty_for_head).as_ref()
                        != Some(&expected_state)
                    {
                        return Err(PaykitSdkError::ConcurrentUpdate {
                            context: "Encrypted Link changed during buffered private receive"
                                .into(),
                            source: None,
                        });
                    }
                    Ok(tx
                        .export_storage_state()
                        .private_intake_peers
                        .get(&counterparty_for_head)
                        .and_then(|peer| peer.buffered_packets.first())
                        .cloned())
                })
                .await?
            else {
                break;
            };
            let expected_counter = link
                .snapshot()?
                .next_private_application_message_resource(&local_public_key, &secret_key)?
                .1;
            if packet.link_generation != stored_link_state.generation
                || packet.receive_counter != expected_counter
            {
                return Err(PaykitSdkError::RecoveryRequired {
                    context: "buffered private packet does not match Noise checkpoint".into(),
                    source: None,
                });
            }
            let prepared =
                match link.prepare_private_application_message_ciphertext(&packet.ciphertext) {
                    Ok(prepared) => prepared,
                    Err(err)
                        if err.is_non_retryable_private_receive_error()
                            || matches!(err, paykit_lib::PaykitError::InvalidData { .. }) =>
                    {
                        mark_recovery_required_with_lease(
                            &self.storage,
                            counterparty.clone(),
                            lease.clone(),
                            &self.clock,
                        )
                        .await?;
                        let _ = self
                            .publish_local_recovery_marker_with_key(
                                &counterparty,
                                &session_access,
                                &lease,
                                snapshot.authorization.noise_public_key(),
                            )
                            .await;
                        return Err(err.into());
                    }
                    Err(err) => return Err(err.into()),
                };
            let next_link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: Some(prepared.resulting_snapshot().serialize()),
                handshake_snapshot: None,
                handshake_role: None,
                generation: stored_link_state.generation.saturating_add(1),
                checkpointed_at: self.clock.now(),
            };
            let write = PrivateStreamBatchWrite {
                counterparty: counterparty.clone(),
                confirmation_app_id: self.config.app_id.clone(),
                messages: vec![prepared.message().clone()],
                link_state: Some(next_link_state.clone()),
                authorized_receipt_apps: None,
                link_lease: Some(lease.clone()),
                receive_batch_id: aggregate.receive_batch_id,
                received_at: self.clock.now(),
            };
            let previous_generation = stored_link_state.generation;
            let resulting_generation = next_link_state.generation;
            let receive_counter = packet.receive_counter;
            let report = self
                .retry_storage_transaction(|| {
                    let mut write = write.clone();
                    let counterparty = counterparty.clone();
                    let authorized_apps = &authorized_apps;
                    move |tx| {
                        require_private_intake_supervisor_lease(
                            tx,
                            supervisor_lease_id,
                            self.clock.now(),
                        )?;
                        write.authorized_receipt_apps = cache_private_receive_authorization(
                            tx,
                            &counterparty,
                            authorized_apps.as_ref(),
                        );
                        let report = persist_private_stream_batch_in_transaction(tx, write)?;
                        tx.remove_buffered_private_intake_packet(
                            &counterparty,
                            previous_generation,
                            receive_counter,
                        )?;
                        tx.rebase_buffered_private_intake_generation(
                            &counterparty,
                            previous_generation,
                            resulting_generation,
                        )?;
                        Ok(report)
                    }
                })
                .await?;
            supervisor.notify_capacity_released();
            authorized_apps = None;
            link.acknowledge_persisted_private_receive(prepared)?;
            stored_link_state = next_link_state;
            aggregate.receive_batch_id = report.receive_batch_id;
            aggregate.stream_item_ids.extend(report.stream_item_ids);
            aggregate.event_conflicts.extend(report.event_conflicts);
        }

        let counterparty_for_remaining = counterparty.clone();
        let remaining_buffered_packets = self
            .storage
            .transaction(move |tx| {
                Ok(tx
                    .export_storage_state()
                    .private_intake_peers
                    .get(&counterparty_for_remaining)
                    .map_or(0, |peer| peer.buffered_packets.len()))
            })
            .await?;
        Ok(BufferedPrivateStreamIntakeReport {
            report: aggregate,
            remaining_buffered_packets,
            freshness,
        })
    }

    pub(super) async fn receive_private_messages_with_checkpoint(
        &self,
        counterparty: PubkyPublicKey,
        context: Option<PrivateReceiveContext>,
    ) -> Result<(PrivateStreamIntakeReport, Option<PrivateInboxCheckpoint>)> {
        let (session_access, snapshot) = if let Some(context) = context {
            self.require_legacy_private_receive_mode(&counterparty)
                .await?;
            let access = context.session_access;
            access.capability_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
            access.paykit_noise_secret_key()?;
            self.validate_local_noise_key_authorization(&access).await?;
            (access, Some(context.snapshot))
        } else {
            let (access, snapshots) = match self.private_receive_context(Some(&counterparty)).await
            {
                Ok(context) => context,
                Err(error) => {
                    if let Err(conflict @ PaykitSdkError::PrivateIntakeModeConflict { .. }) = self
                        .require_legacy_private_receive_mode(&counterparty)
                        .await
                    {
                        return Err(conflict);
                    }
                    return Err(error);
                }
            };
            let (_, snapshot, conflicts) = snapshots
                .into_iter()
                .next()
                .expect("counterparty context contains requested peer");
            if conflicts {
                return Err(PaykitSdkError::PrivateIntakeModeConflict {
                    counterparty: counterparty.clone(),
                });
            }
            (access, snapshot)
        };
        let probe = self
            .probe_private_inbox(&counterparty, &session_access, snapshot)
            .await?;
        let checkpoint = PrivateInboxCheckpoint::from_probe(&session_access, &probe)?;
        let report = self
            .receive_probed_private_messages(counterparty, &session_access, probe)
            .await?;
        Ok((report, checkpoint))
    }

    pub(super) async fn probe_empty_private_inbox(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &GuardedSessionAccess,
        snapshot: PrivateReceiveSnapshot,
    ) -> Result<Option<PrivateInboxCheckpoint>> {
        self.validate_local_noise_key_authorization(session_access)
            .await?;
        let probe = self
            .probe_private_inbox(counterparty, session_access, Some(snapshot))
            .await?;
        PrivateInboxCheckpoint::from_probe(session_access, &probe)
    }

    async fn receive_probed_private_messages(
        &self,
        counterparty: PubkyPublicKey,
        session_access: &GuardedSessionAccess,
        probe: PrivateInboxProbe,
    ) -> Result<PrivateStreamIntakeReport> {
        match probe {
            PrivateInboxProbe::Empty(_) => {
                return Ok(PrivateStreamIntakeReport {
                    receive_batch_id: None,
                    stream_item_ids: Vec::new(),
                    event_conflicts: Vec::new(),
                })
            }
            PrivateInboxProbe::Ready(snapshot) => {
                if let Some(report) = Box::pin(self.receive_private_messages_from_snapshot(
                    counterparty.clone(),
                    session_access,
                    *snapshot,
                ))
                .await?
                {
                    return Ok(report);
                }
            }
            PrivateInboxProbe::Reload => {}
        }
        // Recovery decisions require a fresh read under the peer lease.
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .receive_private_messages_with_claim(counterparty, lease.clone(), session_access)
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Receive private messages from every locally linked counterparty.
    pub async fn receive_private_messages_from_linked_peers(
        &self,
    ) -> Result<Vec<PrivateStreamCounterpartyIntakeReport>> {
        let (session_access, counterparties) = match self.private_receive_context(None).await {
            Ok(context) => context,
            Err(error) => {
                let counterparties = self
                    .storage
                    .transaction(|tx| Ok(linked_counterparties(tx)))
                    .await?;
                let ownership = self
                    .storage
                    .transaction(|tx| Ok(tx.export_storage_state()))
                    .await?;
                let now = self.clock.now();
                return Ok(counterparties
                    .into_iter()
                    .map(|counterparty| {
                        let error =
                            if legacy_private_receive_conflicts(&ownership, &counterparty, now) {
                                PaykitSdkError::PrivateIntakeModeConflict {
                                    counterparty: counterparty.clone(),
                                }
                                .to_string()
                            } else {
                                error.to_string()
                            };
                        PrivateStreamCounterpartyIntakeReport {
                            counterparty,
                            report: None,
                            error: Some(error),
                        }
                    })
                    .collect());
            }
        };
        let (conflicts, counterparties): (Vec<_>, Vec<_>) = counterparties
            .into_iter()
            .partition(|(_, _, conflicts)| *conflicts);
        let mut reports = Vec::with_capacity(conflicts.len() + counterparties.len());
        reports.extend(conflicts.into_iter().map(|(counterparty, _, _)| {
            PrivateStreamCounterpartyIntakeReport {
                error: Some(
                    PaykitSdkError::PrivateIntakeModeConflict {
                        counterparty: counterparty.clone(),
                    }
                    .to_string(),
                ),
                counterparty,
                report: None,
            }
        }));
        let probes = stream::iter(counterparties)
            .map(|(counterparty, snapshot, _)| {
                let session_access = &session_access;
                async move {
                    let probe = self
                        .probe_private_inbox(&counterparty, session_access, snapshot)
                        .await;
                    (counterparty, probe)
                }
            })
            .buffer_unordered(PRIVATE_INBOX_PROBE_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        // Finish polling every probe before a serialized receive can block it.
        for (counterparty, probe) in probes {
            let result = match probe {
                Ok(probe) => {
                    self.receive_probed_private_messages(
                        counterparty.clone(),
                        &session_access,
                        probe,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(report) => reports.push(PrivateStreamCounterpartyIntakeReport {
                    counterparty,
                    report: Some(report),
                    error: None,
                }),
                Err(err) => reports.push(PrivateStreamCounterpartyIntakeReport {
                    counterparty,
                    report: None,
                    error: Some(err.to_string()),
                }),
            }
        }
        reports.sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));
        Ok(reports)
    }

    async fn require_legacy_private_receive_mode(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let counterparty_for_check = counterparty.clone();
        let now = self.clock.now();
        let conflicts = self
            .storage
            .transaction(move |tx| {
                Ok(legacy_private_receive_conflicts(
                    &tx.export_storage_state(),
                    &counterparty_for_check,
                    now,
                ))
            })
            .await?;
        if conflicts {
            return Err(PaykitSdkError::PrivateIntakeModeConflict {
                counterparty: counterparty.clone(),
            });
        }
        Ok(())
    }

    pub(super) async fn private_receive_context(
        &self,
        counterparty: Option<&PubkyPublicKey>,
    ) -> Result<(
        GuardedSessionAccess,
        Vec<(PubkyPublicKey, Option<PrivateReceiveSnapshot>, bool)>,
    )> {
        let now = self.clock.now();
        let (access, _, peers) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                let peers = counterparty
                    .map_or_else(|| linked_counterparties(tx), |peer| vec![peer.clone()]);
                let state = tx.export_storage_state();
                Ok(peers
                    .into_iter()
                    .map(|peer| {
                        let snapshot = private_receive_snapshot(tx, &peer);
                        let conflicts = legacy_private_receive_conflicts(&state, &peer, now);
                        (peer, snapshot, conflicts)
                    })
                    .collect())
            })
            .await?;
        let access = access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        access.paykit_noise_secret_key()?;
        self.validate_local_noise_key_authorization(&access).await?;
        Ok((access, peers.expect("active session reads peer state")))
    }

    async fn probe_private_inbox(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        snapshot: Option<PrivateReceiveSnapshot>,
    ) -> Result<PrivateInboxProbe> {
        let Some(snapshot) = snapshot else {
            return Ok(PrivateInboxProbe::Reload);
        };
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for private message lookup".into(),
                    source: None,
                })?;
        let remote_public_key = counterparty.to_public_key()?;
        let link = &snapshot.link;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let session_info = session_access.session.info();
        let (authorization, marker, pending) = tokio::join!(
            noise_key_authorization::require_authorization(&public_storage, &remote_public_key),
            paykit_lib::fetch_encrypted_link_recovery_marker(
                &public_storage,
                &secret_key,
                session_info.public_key(),
                &remote_public_key,
                link.remote_noise_public_key(),
            ),
            link.has_pending_private_application_message(
                &public_storage,
                session_info.public_key(),
                &secret_key,
            ),
        );
        let authorization = authorization.map_err(link_observation_error)?;
        let marker = self
            .preserve_authorization_advance_on_marker_failure(
                counterparty,
                session_access,
                snapshot.state.generation,
                &snapshot.authorization,
                &authorization,
                marker.map_err(|error| link_observation_error(error.into())),
            )
            .await?;
        if !private_receive_observation_is_current(&snapshot, &authorization, marker.as_ref()) {
            return Ok(PrivateInboxProbe::Reload);
        }
        match pending {
            // Read-only empty probes can coexist with a lease. Actual receives
            // must claim it, including reclaiming an expired record.
            Ok(true) if snapshot.has_peer_lease => Ok(PrivateInboxProbe::Reload),
            Ok(true) => Ok(PrivateInboxProbe::Ready(Box::new(snapshot))),
            Ok(false) => Ok(PrivateInboxProbe::Empty(Box::new(snapshot))),
            Err(paykit_lib::PaykitError::Validation(_)) => Ok(PrivateInboxProbe::Reload),
            Err(err) => Err(err.into()),
        }
    }

    async fn receive_private_messages_from_snapshot(
        &self,
        counterparty: PubkyPublicKey,
        session_access: &GuardedSessionAccess,
        snapshot: PrivateReceiveSnapshot,
    ) -> Result<Option<PrivateStreamIntakeReport>> {
        let authorized_apps = self.private_receive_authorized_apps(&counterparty).await;
        let link = match paykit_lib::restore_encrypted_link(
            session_access.session.clone(),
            session_access.paykit_noise_secret_key()?,
            &counterparty.to_public_key()?,
            session_access.outbox_client.clone(),
            snapshot.link,
        )
        .await
        {
            Ok(link) => link,
            Err(err @ paykit_lib::PaykitError::Transport { .. }) => return Err(err.into()),
            Err(_) => return Ok(None),
        };
        if noise_key_authorization::validate_remote_static_key(
            &snapshot.authorization,
            link.remote_static_public_key(),
            true,
        )
        .is_err()
        {
            return Ok(None);
        }
        self.receive_private_messages_with_link(
            counterparty,
            None,
            session_access,
            PrivateReceiveLink {
                link,
                state: snapshot.state,
                authorized_apps,
                authorization: snapshot.authorization,
            },
        )
        .await
    }

    async fn private_receive_authorized_apps(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>> {
        // A failed lookup keeps cached authorization; a missing registry clears it.
        self.paykit_app_registry(counterparty.clone())
            .await
            .ok()
            .map(|registry| {
                registry
                    .map(|registry| {
                        registry
                            .apps()
                            .iter()
                            .map(|(id, app)| (id.clone(), app.capabilities()))
                            .collect()
                    })
                    .unwrap_or_default()
            })
    }

    async fn restore_private_receive_link(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<PrivateReceiveLink> {
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
        let (peer, stored_link_state) = self
            .storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let peer = tx.linked_peer(&counterparty);
                let state = tx.encrypted_link_state(&counterparty);
                require_private_automation_ready(
                    peer.as_ref().map(|peer| peer.state.clone()),
                    state
                        .as_ref()
                        .is_some_and(|state| state.link_snapshot.is_some()),
                    &counterparty,
                )?;
                Ok((
                    peer.expect("private automation requires a linked peer"),
                    state,
                ))
            })
            .await?;
        let expected_authorization = peer.noise_key_authorization.as_ref().ok_or_else(|| {
            PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no pinned Noise key authorization for counterparty {counterparty}"
                ),
                source: None,
            }
        })?;
        let expected_link_generation = stored_link_state
            .as_ref()
            .expect("private automation requires stored Encrypted Link state")
            .generation;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let session_info = session_access.session.info();
        let local_public_key = session_info.public_key();
        let remote_public_key = counterparty.to_public_key()?;
        let public_storage = session_access.outbox_client.public_storage();
        let (authorization, marker) = tokio::join!(
            noise_key_authorization::require_authorization(&public_storage, &remote_public_key),
            paykit_lib::fetch_encrypted_link_recovery_marker(
                &public_storage,
                &secret_key,
                local_public_key,
                &remote_public_key,
                expected_authorization.noise_public_key(),
            ),
        );
        let authorization = authorization.map_err(link_observation_error)?;
        let marker = marker.map_err(|error| link_observation_error(error.into()));
        match marker {
            Ok(marker) => {
                self.require_current_private_receive_observation(
                    &counterparty,
                    &lease,
                    session_access,
                    PrivateReceiveObservation::new(
                        expected_link_generation,
                        expected_authorization,
                        &authorization,
                        marker.as_ref(),
                    ),
                )
                .await?;
            }
            Err(error) => {
                self.require_current_private_receive_observation(
                    &counterparty,
                    &lease,
                    session_access,
                    PrivateReceiveObservation::with_marker_result(
                        expected_link_generation,
                        expected_authorization,
                        &authorization,
                        Err(error),
                    ),
                )
                .await?;
            }
        }
        let remote_noise_public_key = authorization.noise_public_key().clone();
        let authorized_apps = self.private_receive_authorized_apps(&counterparty).await;

        let stored_link_state =
            stored_link_state.ok_or_else(|| PaykitSdkError::RecoveryRequired {
                context: format!("no Encrypted Link state for counterparty {counterparty}"),
                source: None,
            })?;
        let Some(snapshot_bytes) = stored_link_state.link_snapshot.as_ref() else {
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                &self.clock,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_key(
                    &counterparty,
                    session_access,
                    &lease,
                    &remote_noise_public_key,
                )
                .await;
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no active Encrypted Link snapshot for counterparty {counterparty}"
                ),
                source: None,
            });
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    &self.clock,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_key(
                        &counterparty,
                        session_access,
                        &lease,
                        &remote_noise_public_key,
                    )
                    .await;
                return Err(err.into());
            }
        };
        crate::domain::linked_peers::require_recovery_context(&peer, snapshot.recovery_context())?;
        if snapshot.remote_noise_public_key() != &remote_noise_public_key {
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                &self.clock,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_key(
                    &counterparty,
                    session_access,
                    &lease,
                    &remote_noise_public_key,
                )
                .await;
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!("counterparty {counterparty} rotated its Paykit identity key"),
                source: None,
            });
        }

        let link = match paykit_lib::restore_encrypted_link(
            session_access.session.clone(),
            secret_key,
            &remote_public_key,
            session_access.outbox_client.clone(),
            snapshot,
        )
        .await
        .map_err(PaykitSdkError::from)
        .and_then(|link| {
            noise_key_authorization::validate_remote_static_key(
                &authorization,
                link.remote_static_public_key(),
                true,
            )?;
            Ok(link)
        }) {
            Ok(link) => link,
            Err(err @ PaykitSdkError::Transport { .. }) => return Err(err),
            Err(err) => {
                mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    &self.clock,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_key(
                        &counterparty,
                        session_access,
                        &lease,
                        &remote_noise_public_key,
                    )
                    .await;
                return Err(err);
            }
        };
        Ok(PrivateReceiveLink {
            link,
            state: stored_link_state,
            authorized_apps,
            authorization,
        })
    }

    async fn receive_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: &GuardedSessionAccess,
    ) -> Result<PrivateStreamIntakeReport> {
        self.require_legacy_private_receive_mode(&counterparty)
            .await?;
        let link = self
            .with_guarded_storage_operation(
                Arc::clone(&session_access._guard),
                Box::pin(self.restore_private_receive_link(
                    counterparty.clone(),
                    lease.clone(),
                    session_access,
                )),
            )
            .await?;
        Ok(self
            .receive_private_messages_with_link(counterparty, Some(lease), session_access, link)
            .await?
            .expect("leased receive handles recovery directly"))
    }

    async fn receive_private_messages_with_link(
        &self,
        counterparty: PubkyPublicKey,
        lease: Option<PeerLinkOperationLease>,
        session_access: &GuardedSessionAccess,
        received_link: PrivateReceiveLink,
    ) -> Result<Option<PrivateStreamIntakeReport>> {
        let PrivateReceiveLink {
            mut link,
            state: mut stored_link_state,
            mut authorized_apps,
            authorization,
        } = received_link;
        let local_noise_public_key = PubkyPublicKey::from_public_key(
            &pubky::Keypair::from_secret(&session_access.paykit_noise_secret_key()?).public_key(),
        );
        let mut aggregate: Option<PrivateStreamIntakeReport> = None;
        for _ in 0..paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT {
            let prepared = match link.prepare_next_private_application_message().await {
                Ok(Some(prepared)) => prepared,
                Ok(None) => break,
                Err(err)
                    if err.is_non_retryable_private_receive_error()
                        || matches!(err, paykit_lib::PaykitError::InvalidData { .. }) =>
                {
                    let Some(lease) = lease.as_ref() else {
                        // The speculative read cannot authorize recovery of possibly newer state.
                        return Ok(None);
                    };
                    mark_recovery_required_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        lease.clone(),
                        &self.clock,
                    )
                    .await?;
                    let _ = self
                        .publish_local_recovery_marker_with_key(
                            &counterparty,
                            session_access,
                            lease,
                            authorization.noise_public_key(),
                        )
                        .await;
                    return Err(err.into());
                }
                Err(err) => return Err(err.into()),
            };
            let now = self.clock.now();
            let next_link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: Some(prepared.resulting_snapshot().serialize()),
                handshake_snapshot: None,
                handshake_role: None,
                generation: stored_link_state.generation.saturating_add(1),
                checkpointed_at: now,
            };
            let write = PrivateStreamBatchWrite {
                counterparty: counterparty.clone(),
                confirmation_app_id: self.config.app_id.clone(),
                messages: vec![prepared.message().clone()],
                link_state: Some(next_link_state.clone()),
                authorized_receipt_apps: None,
                link_lease: lease.clone(),
                receive_batch_id: aggregate
                    .as_ref()
                    .and_then(|report| report.receive_batch_id),
                received_at: now,
            };
            let previous_generation = stored_link_state.generation;
            let resulting_generation = next_link_state.generation;
            let report = self
                .retry_storage_transaction(|| {
                    let mut write = write.clone();
                    let authorized_apps = &authorized_apps;
                    let stored_link_state = &stored_link_state;
                    let authorization = &authorization;
                    let local_noise_public_key = &local_noise_public_key;
                    move |tx| {
                        if legacy_private_receive_conflicts(
                            &tx.export_storage_state(),
                            &write.counterparty,
                            self.clock.now(),
                        ) {
                            return Ok(Err(PaykitSdkError::PrivateIntakeModeConflict {
                                counterparty: write.counterparty.clone(),
                            }));
                        }
                        if write.link_lease.is_none() {
                            if let Err(error) = require_private_receive_checkpoint(
                                tx,
                                stored_link_state,
                                authorization,
                                local_noise_public_key,
                            ) {
                                // A stale preparation needs a new receive, not a storage retry.
                                return Ok(Err(error));
                            }
                        }
                        write.authorized_receipt_apps = cache_private_receive_authorization(
                            tx,
                            &write.counterparty,
                            authorized_apps.as_ref(),
                        );
                        let counterparty = write.counterparty.clone();
                        let report = persist_private_stream_batch_in_transaction(tx, write)?;
                        tx.rebase_buffered_private_intake_generation(
                            &counterparty,
                            previous_generation,
                            resulting_generation,
                        )?;
                        Ok(Ok(report))
                    }
                })
                .await??;
            authorized_apps = None;
            link.acknowledge_persisted_private_receive(prepared)?;
            stored_link_state = next_link_state;
            match aggregate.as_mut() {
                Some(aggregate) => {
                    aggregate.stream_item_ids.extend(report.stream_item_ids);
                    aggregate.event_conflicts.extend(report.event_conflicts);
                }
                None => aggregate = Some(report),
            }
        }

        match aggregate {
            Some(report) => Ok(Some(report)),
            None if lease.is_none() => Ok(Some(PrivateStreamIntakeReport {
                receive_batch_id: None,
                stream_item_ids: Vec::new(),
                event_conflicts: Vec::new(),
            })),
            None => self
                .retry_storage_transaction(|| {
                    let counterparty = counterparty.clone();
                    let lease = lease.clone();
                    let authorized_apps = &authorized_apps;
                    move |tx| {
                        if legacy_private_receive_conflicts(
                            &tx.export_storage_state(),
                            &counterparty,
                            self.clock.now(),
                        ) {
                            return Err(PaykitSdkError::PrivateIntakeModeConflict {
                                counterparty: counterparty.clone(),
                            });
                        }
                        let authorized_receipt_apps = cache_private_receive_authorization(
                            tx,
                            &counterparty,
                            authorized_apps.as_ref(),
                        );
                        persist_private_stream_batch_in_transaction(
                            tx,
                            PrivateStreamBatchWrite {
                                counterparty,
                                confirmation_app_id: self.config.app_id.clone(),
                                messages: Vec::new(),
                                link_state: None,
                                authorized_receipt_apps,
                                link_lease: lease,
                                receive_batch_id: None,
                                received_at: self.clock.now(),
                            },
                        )
                    }
                })
                .await
                .map(Some),
        }
    }
}

fn cache_private_receive_authorization(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    fetched_apps: Option<&HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>>,
) -> Option<Vec<paykit_lib::PaykitAppId>> {
    if let Some(apps) = fetched_apps {
        tx.save_authorized_paykit_apps(counterparty.clone(), apps.clone());
    }
    tx.authorized_paykit_apps(counterparty).map(|apps| {
        apps.into_iter()
            .filter(|(_, capabilities)| capabilities.receipts)
            .map(|(id, _)| id)
            .collect()
    })
}

pub(super) fn private_receive_snapshot(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
) -> Option<PrivateReceiveSnapshot> {
    let (state, link, authorization) = associated_link_snapshot(tx, counterparty)?;
    Some(PrivateReceiveSnapshot {
        link,
        state,
        authorization,
        has_peer_lease: tx.peer_link_operation_lease(counterparty).is_some(),
    })
}

/// Return the current link snapshot when stored state still associates it
/// with `counterparty`.
///
/// The peer must be linked, and the snapshot's recipient, recovery context,
/// and remote Noise key must match the stored peer record. This checks stored
/// state only; it does not fetch the counterparty's current authorization.
pub(super) fn associated_link_snapshot(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
) -> Option<(
    EncryptedLinkStateRecord,
    paykit_lib::EncryptedLinkSnapshot,
    paykit_lib::PaykitNoiseKeyAuthorization,
)> {
    let peer = tx.linked_peer(counterparty)?;
    if peer.state != LinkedPeerState::Linked {
        return None;
    }
    let state = tx.encrypted_link_state(counterparty)?;
    let snapshot =
        paykit_lib::EncryptedLinkSnapshot::deserialize(state.link_snapshot.as_ref()?).ok()?;
    if PubkyPublicKey::from_public_key(snapshot.recipient()) != *counterparty {
        return None;
    }
    crate::domain::linked_peers::require_recovery_context(&peer, snapshot.recovery_context())
        .ok()?;
    let authorization = peer.noise_key_authorization?;
    if snapshot.remote_noise_public_key() != authorization.noise_public_key() {
        return None;
    }
    Some((state, snapshot, authorization))
}

fn private_receive_checkpoint_uses_authorization(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
) -> bool {
    let Some(peer) = tx.linked_peer(counterparty) else {
        return false;
    };
    match peer.state {
        LinkedPeerState::Linked => private_receive_snapshot(tx, counterparty)
            .is_some_and(|snapshot| snapshot.authorization == *authorization),
        LinkedPeerState::Linking => {
            peer.noise_key_authorization.as_ref() == Some(authorization)
                && tx
                    .encrypted_link_state(counterparty)
                    .is_some_and(|state| state.handshake_snapshot.is_some())
        }
        _ => false,
    }
}

fn require_private_receive_checkpoint(
    tx: &dyn StorageTransaction,
    state: &EncryptedLinkStateRecord,
    authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
    local_noise_public_key: &PubkyPublicKey,
) -> Result<()> {
    if tx.paykit_noise_public_key().as_ref() != Some(local_noise_public_key) {
        return Err(PaykitSdkError::Identity {
            context: "Paykit identity key changed during private receive".into(),
            source: None,
        });
    }
    let current = private_receive_snapshot(tx, &state.counterparty);
    if current.is_none_or(|current| {
        current.has_peer_lease || current.state != *state || current.authorization != *authorization
    }) {
        return Err(PaykitSdkError::ConcurrentUpdate {
            context: "Encrypted Link changed during private receive; retry from current state"
                .into(),
            source: None,
        });
    }
    Ok(())
}

fn linked_counterparties(tx: &dyn StorageTransaction) -> Vec<PubkyPublicKey> {
    let mut peers = tx
        .export_storage_state()
        .linked_peers
        .into_values()
        .filter(|peer| peer.state == LinkedPeerState::Linked)
        .map(|peer| peer.counterparty)
        .collect::<Vec<_>>();
    peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    peers
}

fn legacy_private_receive_conflicts(
    state: &crate::storage::StorageState,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> bool {
    let has_buffered_packets = state
        .private_intake_peers
        .get(counterparty)
        .is_some_and(|peer| !peer.buffered_packets.is_empty());
    let supervisor_owns_established_link = state
        .private_intake_supervisor_lease
        .as_ref()
        .is_some_and(|lease| lease.expires_at > now)
        && state
            .linked_peers
            .get(counterparty)
            .is_some_and(|peer| peer.state == LinkedPeerState::Linked)
        && state
            .encrypted_link_states
            .get(counterparty)
            .is_some_and(|link| link.link_snapshot.is_some());
    has_buffered_packets || supervisor_owns_established_link
}

fn require_private_intake_supervisor_lease(
    tx: &dyn StorageTransaction,
    lease_id: u64,
    now: DateTime<Utc>,
) -> Result<()> {
    let current = tx.export_storage_state().private_intake_supervisor_lease;
    if current
        .as_ref()
        .is_none_or(|lease| lease.lease_id != lease_id || lease.expires_at <= now)
    {
        return Err(PaykitSdkError::ConcurrentUpdate {
            context: "private intake supervisor ownership changed".into(),
            source: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        decode_storage_state_blob, encode_storage_state_blob, NewOutboundPrivateMessage,
        PrivateIntakeCursorRecord, StorageTransactionCallback,
    };
    use crate::InMemoryStorage;
    use std::sync::{Arc, Mutex as StdMutex};

    #[derive(Clone)]
    struct TransactionObservedStorage {
        inner: InMemoryStorage,
        transaction_started: Arc<StdMutex<Option<std::sync::mpsc::Sender<()>>>>,
    }

    #[async_trait::async_trait]
    impl StorageAdapter for TransactionObservedStorage {
        async fn load_local_identity_state(&self) -> Result<Option<IdentityState>> {
            self.inner.load_local_identity_state().await
        }

        async fn transaction_erased<'a>(
            &self,
            callback: StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn std::any::Any + Send>> {
            if let Some(started) = self.transaction_started.lock().unwrap().take() {
                started.send(()).unwrap();
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

    fn private_receive_test_snapshot(
        counterparty: &PubkyPublicKey,
        remote_noise_public_key: &pubky::PublicKey,
        recovery_attempt_id: Option<&str>,
    ) -> paykit_lib::EncryptedLinkSnapshot {
        use pubky_noise::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};

        let state = pubky_noise::serializer::PubkyNoiseSessionState {
            version: pubky_noise::serializer::SESSION_STATE_VERSION,
            phase: NoisePhase::Transport,
            pattern: HandshakePattern::PatternXX,
            initiator: true,
            ephemeral_secret: [1; 32],
            static_secret: Some([2; 32]),
            counter: 3,
            noise_step: NoiseStep::Final,
            sub_step_index: 0,
            handshake_hash: Some([3; 32]),
            link_id: Some([4; 32]),
            sending_nonce: 0,
            receiving_nonce: 0,
            write_counter: 3,
            read_counter: 3,
            endpoint_pubkey: counterparty.to_public_key().unwrap().to_bytes(),
            handshake_messages: vec![vec![5; 96]],
        };
        let mut bytes = state.serialize();
        bytes.extend_from_slice(&remote_noise_public_key.to_bytes());
        for attempt_id in [recovery_attempt_id, recovery_attempt_id] {
            bytes.extend_from_slice(attempt_id.map(str::as_bytes).unwrap_or(&[0; 36]));
        }
        paykit_lib::EncryptedLinkSnapshot::deserialize(&bytes).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_observation_expiry_after_storage_fence_is_atomic_and_reclaimable() {
        let inner = InMemoryStorage::new();
        let identity = pubky::Keypair::random();
        let counterparty = PubkyPublicKey::from_public_key(&identity.public_key());
        let generation_1 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[1; 32], 1).unwrap();
        let generation_2 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[2; 32], 2).unwrap();
        let claimed_at = Utc::now();
        let expires_at = claimed_at + chrono::Duration::seconds(1);
        let lease = inner
            .transaction(|tx| {
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.last_sync_at = Some(claimed_at);
                peer.noise_key_authorization = Some(generation_1.clone());
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: claimed_at,
                });
                tx.advance_private_intake_cursor(PrivateIntakeCursorRecord {
                    subscribed_user: counterparty.clone(),
                    homeserver: "https://homeserver.example".into(),
                    event_id: 9,
                })?;
                let app_id = paykit_lib::PaykitAppId::new("bitkit").unwrap();
                tx.save_paykit_app_capabilities(
                    &app_id,
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: true,
                        payment_requests: true,
                        receipts: true,
                        outgoing_payments: true,
                    },
                );
                tx.activate_paykit_app(&app_id);
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id,
                    paykit_lib::PrivateMessageKind::PaymentRequest
                        .as_str()
                        .into(),
                    "{}".into(),
                    claimed_at,
                ))?;
                tx.claim_peer_link_operation(&counterparty, claimed_at, expires_at)?
                    .ok_or_else(|| PaykitSdkError::ConcurrentUpdate {
                        context: "test peer lease unavailable".into(),
                        source: None,
                    })
            })
            .await
            .unwrap();
        let before = inner.snapshot().unwrap();
        let clock = MutableClock::new(claimed_at);
        let (holder_entered_tx, holder_entered_rx) = tokio::sync::oneshot::channel();
        let (release_holder_tx, release_holder_rx) = std::sync::mpsc::channel();
        let holder_storage = inner.clone();
        let holder = tokio::spawn(async move {
            holder_storage
                .transaction(move |_tx| {
                    holder_entered_tx.send(()).unwrap();
                    release_holder_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        holder_entered_rx.await.unwrap();

        let (observation_started_tx, observation_started_rx) = std::sync::mpsc::channel();
        let storage = TransactionObservedStorage {
            inner: inner.clone(),
            transaction_started: Arc::new(StdMutex::new(Some(observation_started_tx))),
        };
        let observation_counterparty = counterparty.clone();
        let observation_lease = lease.clone();
        let observation_generation_1 = generation_1.clone();
        let observation_generation_2 = generation_2.clone();
        let observation_clock = clock.clone();
        let observation = tokio::spawn(async move {
            require_current_private_receive_observation_with_lease(
                &storage,
                &observation_counterparty,
                &observation_lease,
                PrivateReceiveObservation::new(
                    7,
                    &observation_generation_1,
                    &observation_generation_2,
                    None,
                ),
                &observation_clock,
            )
            .await
        });
        observation_started_rx.recv().unwrap();
        clock.set(expires_at);
        release_holder_tx.send(()).unwrap();
        holder.await.unwrap().unwrap();

        assert!(matches!(
            observation.await.unwrap(),
            Err(PaykitSdkError::Policy { .. })
        ));
        assert_eq!(inner.snapshot().unwrap(), before);

        let reclaimed_lease = inner
            .transaction(|tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    expires_at,
                    expires_at + chrono::Duration::minutes(1),
                )?
                .ok_or_else(|| PaykitSdkError::ConcurrentUpdate {
                    context: "test peer lease was not reclaimable at expiry boundary".into(),
                    source: None,
                })
            })
            .await
            .unwrap();
        assert_ne!(reclaimed_lease.lease_id, lease.lease_id);
        assert!(matches!(
            require_current_private_receive_observation_with_lease(
                &inner,
                &counterparty,
                &reclaimed_lease,
                PrivateReceiveObservation::new(7, &generation_1, &generation_2, None),
                &clock,
            )
            .await,
            Err(PaykitSdkError::RecoveryRequired { .. })
        ));
        let recovered = inner.snapshot().unwrap();
        let peer = &recovered.linked_peers[&counterparty];
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(peer.noise_key_authorization.as_ref(), Some(&generation_2));
        assert_eq!(peer.last_sync_at, Some(expires_at));
        assert!(recovered.encrypted_link_states[&counterparty]
            .link_snapshot
            .is_none());
        assert_eq!(
            recovered.private_intake_cursors,
            before.private_intake_cursors
        );
        assert_eq!(
            recovered.outbound_private_messages[0].status,
            OutboundPrivateMessageStatus::RecoveryRequired
        );
        assert_eq!(
            recovered.outbound_private_messages[0].updated_at,
            expires_at
        );
    }

    #[tokio::test]
    async fn test_marker_failure_persists_authorization_and_rejects_rollback_replay() {
        let storage = InMemoryStorage::new();
        let identity = pubky::Keypair::random();
        let counterparty = PubkyPublicKey::from_public_key(&identity.public_key());
        let generation_1 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[1; 32], 1).unwrap();
        let generation_2 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[2; 32], 2).unwrap();
        let local_identity =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let now = Utc::now();
        let lease = storage
            .transaction(|tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_identity),
                    initialized_at: now,
                });
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.noise_key_authorization = Some(generation_1.clone());
                tx.save_linked_peer(peer);
                tx.claim_peer_link_operation(&counterparty, now, now + chrono::Duration::minutes(1))
            })
            .await
            .unwrap()
            .unwrap();
        let clock = MutableClock::new(now);
        let initial = storage.snapshot().unwrap();
        let initial_blob = encode_storage_state_blob(&initial).unwrap();
        let storage =
            InMemoryStorage::from_state(decode_storage_state_blob(&initial_blob).unwrap());
        require_current_private_receive_observation_with_lease(
            &storage,
            &counterparty,
            &lease,
            PrivateReceiveObservation::new(0, &generation_1, &generation_1, None),
            &clock,
        )
        .await
        .unwrap();
        assert_eq!(storage.snapshot().unwrap(), initial);

        assert!(matches!(
            require_current_private_receive_observation_with_lease(
                &storage,
                &counterparty,
                &lease,
                PrivateReceiveObservation::with_marker_result(
                    0,
                    &generation_1,
                    &generation_2,
                    Err(PaykitSdkError::LinkObservation {
                        context: "forced marker read failure".into(),
                    }),
                ),
                &clock,
            )
            .await,
            Err(PaykitSdkError::RecoveryRequired { .. })
        ));
        let advanced = storage.snapshot().unwrap();
        assert_eq!(
            advanced.linked_peers[&counterparty]
                .noise_key_authorization
                .as_ref(),
            Some(&generation_2)
        );
        assert_eq!(
            advanced.linked_peers[&counterparty].state,
            LinkedPeerState::RecoveryRequired
        );
        crate::validate_storage_state(&advanced).unwrap();
        let state_blob = encode_storage_state_blob(&advanced).unwrap();
        let reloaded = InMemoryStorage::from_state(decode_storage_state_blob(&state_blob).unwrap());
        assert_eq!(reloaded.snapshot().unwrap(), advanced);

        assert!(matches!(
            require_current_private_receive_observation_with_lease(
                &reloaded,
                &counterparty,
                &lease,
                PrivateReceiveObservation::new(0, &generation_1, &generation_1, None),
                &clock,
            )
            .await,
            Err(PaykitSdkError::LinkObservation { .. })
        ));
        assert_eq!(reloaded.snapshot().unwrap(), advanced);
    }

    #[tokio::test]
    async fn test_buffered_receive_generation_change_with_empty_rotated_key_marker_recovers() {
        let storage = InMemoryStorage::new();
        let identity = pubky::Keypair::random();
        let counterparty = PubkyPublicKey::from_public_key(&identity.public_key());
        let generation_1 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[1; 32], 1).unwrap();
        let generation_2 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[2; 32], 2).unwrap();
        let old_snapshot =
            private_receive_test_snapshot(&counterparty, generation_1.noise_public_key(), None);
        let local_identity =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let now = Utc::now();
        let lease = storage
            .transaction(|tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_identity),
                    initialized_at: now,
                });
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.noise_key_authorization = Some(generation_1.clone());
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(old_snapshot.serialize()),
                    handshake_snapshot: None,
                    handshake_role: None,
                    // A normal buffered receive advanced this after SSE captured 1.
                    generation: 2,
                    checkpointed_at: now,
                });
                tx.claim_peer_link_operation(&counterparty, now, now + chrono::Duration::minutes(1))
            })
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(
            require_current_private_receive_observation_with_lease(
                &storage,
                &counterparty,
                &lease,
                PrivateReceiveObservation::new(1, &generation_1, &generation_2, None),
                &MutableClock::new(now),
            )
            .await,
            Err(PaykitSdkError::RecoveryRequired { .. })
        ));

        let persisted = storage.snapshot().unwrap();
        let peer = &persisted.linked_peers[&counterparty];
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(peer.noise_key_authorization.as_ref(), Some(&generation_2));
        assert!(persisted.encrypted_link_states[&counterparty]
            .link_snapshot
            .is_none());
        crate::validate_storage_state(&persisted).unwrap();
    }

    #[tokio::test]
    async fn test_buffered_receive_generation_change_with_failed_rotated_key_marker_recovers() {
        let storage = InMemoryStorage::new();
        let identity = pubky::Keypair::random();
        let counterparty = PubkyPublicKey::from_public_key(&identity.public_key());
        let generation_1 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[1; 32], 1).unwrap();
        let generation_2 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[2; 32], 2).unwrap();
        let old_snapshot =
            private_receive_test_snapshot(&counterparty, generation_1.noise_public_key(), None);
        let local_identity =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let now = Utc::now();
        let lease = storage
            .transaction(|tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_identity),
                    initialized_at: now,
                });
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.noise_key_authorization = Some(generation_1.clone());
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(old_snapshot.serialize()),
                    handshake_snapshot: None,
                    handshake_role: None,
                    // A normal buffered receive advanced this after SSE captured 1.
                    generation: 2,
                    checkpointed_at: now,
                });
                tx.claim_peer_link_operation(&counterparty, now, now + chrono::Duration::minutes(1))
            })
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(
            require_current_private_receive_observation_with_lease(
                &storage,
                &counterparty,
                &lease,
                PrivateReceiveObservation::with_marker_result(
                    1,
                    &generation_1,
                    &generation_2,
                    Err(PaykitSdkError::LinkObservation {
                        context: "forced marker read failure".into(),
                    }),
                ),
                &MutableClock::new(now),
            )
            .await,
            Err(PaykitSdkError::RecoveryRequired { .. })
        ));

        let persisted = storage.snapshot().unwrap();
        let peer = &persisted.linked_peers[&counterparty];
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(peer.noise_key_authorization.as_ref(), Some(&generation_2));
        assert!(persisted.encrypted_link_states[&counterparty]
            .link_snapshot
            .is_none());
        crate::validate_storage_state(&persisted).unwrap();
    }

    #[tokio::test]
    async fn test_sse_stale_observation_preserves_concurrently_relinked_peer() {
        let storage = InMemoryStorage::new();
        let identity = pubky::Keypair::random();
        let counterparty = PubkyPublicKey::from_public_key(&identity.public_key());
        let generation_1 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[1; 32], 1).unwrap();
        let generation_2 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[2; 32], 2).unwrap();
        let current_marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap();
        let current_snapshot = private_receive_test_snapshot(
            &counterparty,
            generation_2.noise_public_key(),
            Some(current_marker.attempt_id()),
        );
        let now = Utc::now();
        let lease = storage
            .transaction(|tx| {
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.noise_key_authorization = Some(generation_2.clone());
                peer.local_recovery_attempt_id = Some(current_marker.attempt_id().to_owned());
                peer.remote_recovery_attempt_id = Some(current_marker.attempt_id().to_owned());
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(current_snapshot.serialize()),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 2,
                    checkpointed_at: now,
                });
                tx.claim_peer_link_operation(&counterparty, now, now + chrono::Duration::minutes(1))
            })
            .await
            .unwrap()
            .unwrap();
        let before = storage.snapshot().unwrap();
        require_current_private_receive_observation_with_lease(
            &storage,
            &counterparty,
            &lease,
            PrivateReceiveObservation::new(1, &generation_1, &generation_2, Some(&current_marker)),
            &MutableClock::new(now),
        )
        .await
        .unwrap();

        assert_eq!(storage.snapshot().unwrap(), before);
    }

    #[tokio::test]
    async fn test_sse_stale_observation_preserves_concurrently_started_recovery_handshake() {
        let storage = InMemoryStorage::new();
        let identity = pubky::Keypair::random();
        let counterparty = PubkyPublicKey::from_public_key(&identity.public_key());
        let generation_1 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[1; 32], 1).unwrap();
        let generation_2 =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[2; 32], 2).unwrap();
        let stale_marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap();
        let current_marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap();
        assert_ne!(stale_marker.attempt_id(), current_marker.attempt_id());
        let now = Utc::now();
        storage
            .transaction(|tx| {
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.noise_key_authorization = Some(generation_1.clone());
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 1,
                    checkpointed_at: now,
                });
                Ok(())
            })
            .await
            .unwrap();
        // SSE captured generation 1, then its old snapshot key yielded the stale
        // marker while another operation started recovery under generation 2.
        let expected_link_generation =
            storage.snapshot().unwrap().encrypted_link_states[&counterparty].generation;
        let lease = storage
            .transaction(|tx| {
                let mut peer = tx.linked_peer(&counterparty).unwrap();
                peer.state = LinkedPeerState::Linking;
                peer.noise_key_authorization = Some(generation_2.clone());
                peer.local_recovery_attempt_id = Some(current_marker.attempt_id().to_owned());
                peer.remote_recovery_attempt_id = Some(current_marker.attempt_id().to_owned());
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![4, 5, 6]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                    generation: 2,
                    checkpointed_at: now,
                });
                tx.claim_peer_link_operation(&counterparty, now, now + chrono::Duration::minutes(1))
            })
            .await
            .unwrap()
            .unwrap();
        let before = storage.snapshot().unwrap();

        let status = require_current_private_receive_observation_with_lease(
            &storage,
            &counterparty,
            &lease,
            PrivateReceiveObservation::new(
                expected_link_generation,
                &generation_1,
                &generation_2,
                Some(&stale_marker),
            ),
            &MutableClock::new(now),
        )
        .await
        .unwrap();
        assert_eq!(
            status,
            PrivateReceiveObservationStatus::ReloadMarker { link_generation: 2 }
        );
        assert_eq!(storage.snapshot().unwrap(), before);

        let reloaded_status = require_current_private_receive_observation_with_lease(
            &storage,
            &counterparty,
            &lease,
            PrivateReceiveObservation::new(2, &generation_2, &generation_2, Some(&current_marker)),
            &MutableClock::new(now),
        )
        .await
        .unwrap();
        assert_eq!(reloaded_status, PrivateReceiveObservationStatus::Current);

        assert_eq!(storage.snapshot().unwrap(), before);
    }

    #[tokio::test]
    async fn test_changed_recovery_marker_persists_recovery_transition() {
        let storage = InMemoryStorage::new();
        let identity = pubky::Keypair::random();
        let counterparty = PubkyPublicKey::from_public_key(&identity.public_key());
        let authorization =
            paykit_lib::PaykitNoiseKeyAuthorization::sign(&identity, &[1; 32], 1).unwrap();
        let local_identity =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let old_marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap();
        let new_marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
        .unwrap();
        let now = Utc::now();
        let lease = storage
            .transaction(|tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_identity),
                    initialized_at: now,
                });
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                peer.noise_key_authorization = Some(authorization.clone());
                peer.remote_recovery_attempt_id = Some(old_marker.attempt_id().to_owned());
                peer.remote_recovery_marker_observed_at = Some(now);
                tx.save_linked_peer(peer);
                tx.claim_peer_link_operation(&counterparty, now, now + chrono::Duration::minutes(1))
            })
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            require_current_private_receive_observation_with_lease(
                &storage,
                &counterparty,
                &lease,
                PrivateReceiveObservation::new(
                    0,
                    &authorization,
                    &authorization,
                    Some(&new_marker),
                ),
                &MutableClock::new(now),
            )
            .await,
            Err(PaykitSdkError::RecoveryRequired { .. })
        ));
        let persisted = storage.snapshot().unwrap();
        let state_blob = encode_storage_state_blob(&persisted).unwrap();
        let reloaded = InMemoryStorage::from_state(decode_storage_state_blob(&state_blob).unwrap());
        let reloaded_state = reloaded.snapshot().unwrap();
        assert_eq!(reloaded_state, persisted);
        let peer = &reloaded_state.linked_peers[&counterparty];
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(peer.noise_key_authorization.as_ref(), Some(&authorization));
        assert_eq!(
            peer.remote_recovery_attempt_id.as_deref(),
            Some(new_marker.attempt_id())
        );
        assert_eq!(peer.remote_recovery_marker_observed_at, Some(now));
    }
}
