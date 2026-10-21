use {
    crate::{
        cluster_slots_service::cluster_slots::ClusterSlots,
        repair::{
            duplicate_repair_status::{
                AncestorRequestDecision, AncestorRequestStatus, AncestorRequestType,
            },
            outstanding_requests::OutstandingRequests,
            packet_threshold::DynamicPacketToProcessThreshold,
            repair_service::{AncestorDuplicateSlotsSender, RepairInfo, RepairStatsGroup},
            serve_repair::{
                AncestorHashesRepairType, AncestorHashesResponse, RepairProtocol, ServeRepair,
            },
            serve_repair_service::RESPONSE_CHANNEL_SIZE,
            standard_repair_handler::StandardRepairHandler,
        },
        replay_stage::DUPLICATE_THRESHOLD,
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded},
    dashmap::{DashMap, mapref::entry::Entry::Occupied},
    solana_clock::Slot,
    solana_gossip::{contact_info::Protocol, ping_pong::Pong},
    solana_keypair::{Keypair, Signer, signable::Signable},
    solana_ledger::blockstore::Blockstore,
    solana_perf::packet::{BytesPacket, PacketBatch, packet_config},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::streamer::{self, PacketBatchReceiver, StreamerReceiveStats},
    solana_time_utils::timestamp,
    std::{
        collections::HashSet,
        io::{Cursor, Read},
        net::UdpSocket,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle, sleep},
        time::{Duration, Instant},
    },
};

#[derive(Debug, PartialEq, Eq)]
pub enum AncestorHashesReplayUpdate {
    Dead(Slot),
    DeadDuplicateConfirmed(Slot),
    // `Slot` belongs to a fork we have pruned. We have observed that this fork is "popular" aka
    // reached 52+% stake through votes in turbine/gossip including votes for descendants. These
    // votes are hash agnostic since we have not replayed `Slot` so we can never say for certainty
    // that this fork has reached duplicate confirmation, but it is suspected to have. This
    // indicates that there is most likely a block with invalid ancestry present and thus we
    // collect an ancestor sample to resolve this issue. `Slot` is the deepest slot in this fork
    // that is popular, so any duplicate problems will be for `Slot` or one of it's ancestors.
    PopularPrunedFork(Slot),
}

impl AncestorHashesReplayUpdate {
    fn slot(&self) -> Slot {
        match self {
            AncestorHashesReplayUpdate::Dead(slot) => *slot,
            AncestorHashesReplayUpdate::DeadDuplicateConfirmed(slot) => *slot,
            AncestorHashesReplayUpdate::PopularPrunedFork(slot) => *slot,
        }
    }
}

pub const MAX_ANCESTOR_HASHES_SLOT_REQUESTS_PER_SECOND: usize = 2;

pub type AncestorHashesReplayUpdateSender = Sender<AncestorHashesReplayUpdate>;
pub type AncestorHashesReplayUpdateReceiver = Receiver<AncestorHashesReplayUpdate>;

type RetryableSlotsSender = Sender<(Slot, AncestorRequestType)>;
type RetryableSlotsReceiver = Receiver<(Slot, AncestorRequestType)>;
type OutstandingAncestorHashesRepairs = OutstandingRequests<AncestorHashesRepairType>;

#[derive(Default)]
struct AncestorHashesResponsesStats {
    total_packets: usize,
    processed: usize,
    dropped_packets: usize,
    invalid_packets: usize,
    ping_count: usize,
    ping_err_verify_count: usize,
}

impl AncestorHashesResponsesStats {
    const REPORT_INTERVAL: Duration = Duration::from_secs(2);

    fn report(&mut self) {
        datapoint_info!(
            "ancestor_hashes_responses",
            ("total_packets", self.total_packets, i64),
            ("processed", self.processed, i64),
            ("dropped_packets", self.dropped_packets, i64),
            ("invalid_packets", self.invalid_packets, i64),
            ("ping_count", self.ping_count, i64),
            ("ping_err_verify_count", self.ping_err_verify_count, i64),
        );
        *self = AncestorHashesResponsesStats::default();
    }
}

pub struct AncestorRepairRequestsStats {
    pub ancestor_requests: RepairStatsGroup,
    last_report: Instant,
}

impl Default for AncestorRepairRequestsStats {
    fn default() -> Self {
        AncestorRepairRequestsStats {
            ancestor_requests: RepairStatsGroup::default(),
            last_report: Instant::now(),
        }
    }
}

impl AncestorRepairRequestsStats {
    const REPORT_INTERVAL: Duration = Duration::from_secs(2);

    fn report(&mut self) {
        let slot_to_count: Vec<_> = self
            .ancestor_requests
            .slot_pubkeys
            .iter()
            .map(|(slot, slot_repairs)| (slot, slot_repairs.pubkey_repairs().values().sum::<u64>()))
            .collect();

        let repair_total = self.ancestor_requests.count;
        if self.last_report.elapsed() > Self::REPORT_INTERVAL && repair_total > 0 {
            info!("ancestor_repair_requests_stats: {slot_to_count:?}");
            datapoint_info!(
                "ancestor-repair",
                ("ancestor-repair-count", self.ancestor_requests.count, i64)
            );

            *self = AncestorRepairRequestsStats::default();
        }
    }
}

pub struct AncestorHashesChannels {
    pub ancestor_hashes_replay_update_receiver: AncestorHashesReplayUpdateReceiver,
}

pub struct AncestorHashesService {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl AncestorHashesService {
    pub fn new(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        ancestor_hashes_request_socket: Arc<UdpSocket>,
        ancestor_hashes_channels: AncestorHashesChannels,
        repair_info: RepairInfo,
    ) -> Option<Self> {
        if repair_info
            .bank_forks
            .read()
            .unwrap()
            .migration_status()
            .is_alpenglow_enabled()
        {
            info!("Alpenglow enabled, not starting AncestorHashesService");
            return None;
        }
        let outstanding_requests = Arc::<RwLock<OutstandingAncestorHashesRepairs>>::default();
        let (response_sender, response_receiver) = bounded(RESPONSE_CHANNEL_SIZE);
        let t_receiver = streamer::receiver(
            "solRcvrAncHash".to_string(),
            ancestor_hashes_request_socket.clone(),
            exit.clone(),
            response_sender.clone(),
            Arc::new(StreamerReceiveStats::new(
                "ancestor_hashes_response_receiver",
            )),
            Some(Duration::from_millis(1)), // coalesce
            false,                          // is_staked_service
        );

        let AncestorHashesChannels {
            ancestor_hashes_replay_update_receiver,
        } = ancestor_hashes_channels;

        let ancestor_hashes_request_statuses: Arc<DashMap<Slot, AncestorRequestStatus>> =
            Arc::new(DashMap::new());
        // MAX_ANCESTOR_HASHES_SLOT_REQUESTS_PER_SECOND = 2, so we can buffer for > minute here.
        let (retryable_slots_sender, retryable_slots_receiver) = bounded(128);

        // Listen for responses to our ancestor requests
        let t_ancestor_hashes_responses = Self::run_responses_listener(
            ancestor_hashes_request_statuses.clone(),
            response_receiver,
            blockstore.clone(),
            outstanding_requests.clone(),
            exit.clone(),
            &repair_info,
            retryable_slots_sender,
            ancestor_hashes_request_socket.clone(),
        );

        // Generate ancestor requests for dead slots that are repairable
        let t_ancestor_requests = Self::run_manage_ancestor_requests(
            blockstore,
            ancestor_hashes_request_statuses,
            ancestor_hashes_request_socket,
            repair_info,
            outstanding_requests,
            exit,
            ancestor_hashes_replay_update_receiver,
            retryable_slots_receiver,
        );
        Some(Self {
            thread_hdls: vec![t_receiver, t_ancestor_hashes_responses, t_ancestor_requests],
        })
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdls.into_iter().try_for_each(JoinHandle::join)
    }

    /// Listen for responses to our ancestors hashes repair requests
    fn run_responses_listener(
        ancestor_hashes_request_statuses: Arc<DashMap<Slot, AncestorRequestStatus>>,
        response_receiver: PacketBatchReceiver,
        blockstore: Arc<Blockstore>,
        outstanding_requests: Arc<RwLock<OutstandingAncestorHashesRepairs>>,
        exit: Arc<AtomicBool>,
        repair_info: &RepairInfo,
        retryable_slots_sender: RetryableSlotsSender,
        ancestor_socket: Arc<UdpSocket>,
    ) -> JoinHandle<()> {
        let ancestor_duplicate_slots_sender = repair_info.ancestor_duplicate_slots_sender.clone();
        let cluster_info = repair_info.cluster_info.clone();
        let migration_status = repair_info.bank_forks.read().unwrap().migration_status();
        Builder::new()
            .name("solAncHashesSvc".to_string())
            .spawn(move || {
                let mut last_stats_report = Instant::now();
                let mut stats = AncestorHashesResponsesStats::default();
                let mut packet_threshold = DynamicPacketToProcessThreshold::default();
                while !exit.load(Ordering::Relaxed) && !migration_status.is_alpenglow_enabled() {
                    let keypair = cluster_info.keypair();
                    let result = Self::process_new_packets_from_channel(
                        &ancestor_hashes_request_statuses,
                        &response_receiver,
                        &blockstore,
                        &outstanding_requests,
                        &mut stats,
                        &mut packet_threshold,
                        &ancestor_duplicate_slots_sender,
                        &retryable_slots_sender,
                        &keypair,
                        &ancestor_socket,
                    );
                    match result {
                        Ok(_) | Err(RecvTimeoutError::Timeout) => (),
                        Err(RecvTimeoutError::Disconnected) => {
                            info!("ancestors hashes responses listener disconnected");
                            return;
                        }
                    };
                    if last_stats_report.elapsed() > AncestorHashesResponsesStats::REPORT_INTERVAL {
                        stats.report();
                        last_stats_report = Instant::now();
                    }
                }
            })
            .unwrap()
    }

    /// Process messages from the network
    #[allow(clippy::too_many_arguments)]
    fn process_new_packets_from_channel(
        ancestor_hashes_request_statuses: &DashMap<Slot, AncestorRequestStatus>,
        response_receiver: &PacketBatchReceiver,
        blockstore: &Blockstore,
        outstanding_requests: &RwLock<OutstandingAncestorHashesRepairs>,
        stats: &mut AncestorHashesResponsesStats,
        packet_threshold: &mut DynamicPacketToProcessThreshold,
        ancestor_duplicate_slots_sender: &AncestorDuplicateSlotsSender,
        retryable_slots_sender: &RetryableSlotsSender,
        keypair: &Keypair,
        ancestor_socket: &UdpSocket,
    ) -> Result<(), RecvTimeoutError> {
        let timeout = Duration::new(1, 0);
        let mut packet_batches = vec![response_receiver.recv_timeout(timeout)?];
        let mut total_packets = packet_batches[0].len();

        let mut dropped_packets = 0;
        while let Ok(batch) = response_receiver.try_recv() {
            total_packets += batch.len();
            if packet_threshold.should_drop(total_packets) {
                dropped_packets += batch.len();
            } else {
                packet_batches.push(batch);
            }
        }

        stats.dropped_packets += dropped_packets;
        stats.total_packets += total_packets;

        let timer = Instant::now();
        for packet_batch in packet_batches {
            Self::process_packet_batch(
                ancestor_hashes_request_statuses,
                packet_batch,
                stats,
                outstanding_requests,
                blockstore,
                ancestor_duplicate_slots_sender,
                retryable_slots_sender,
                keypair,
                ancestor_socket,
            );
        }
        packet_threshold.update(total_packets, timer.elapsed());
        Ok(())
    }

    fn process_packet_batch(
        ancestor_hashes_request_statuses: &DashMap<Slot, AncestorRequestStatus>,
        packet_batch: PacketBatch,
        stats: &mut AncestorHashesResponsesStats,
        outstanding_requests: &RwLock<OutstandingAncestorHashesRepairs>,
        blockstore: &Blockstore,
        ancestor_duplicate_slots_sender: &AncestorDuplicateSlotsSender,
        retryable_slots_sender: &RetryableSlotsSender,
        keypair: &Keypair,
        ancestor_socket: &UdpSocket,
    ) {
        packet_batch.iter().for_each(|packet| {
            let ancestor_request_decision = Self::verify_and_process_ancestor_response(
                packet,
                ancestor_hashes_request_statuses,
                stats,
                outstanding_requests,
                blockstore,
                keypair,
                ancestor_socket,
            );
            if let Some(ancestor_request_decision) = ancestor_request_decision {
                Self::handle_ancestor_request_decision(
                    ancestor_request_decision,
                    ancestor_duplicate_slots_sender,
                    retryable_slots_sender,
                );
            }
        });
    }

    /// Returns `Some((request_slot, decision))`, where `decision` is an actionable
    /// result after processing sufficient responses for the subject of the query,
    /// `request_slot`
    fn verify_and_process_ancestor_response(
        packet: &BytesPacket,
        ancestor_hashes_request_statuses: &DashMap<Slot, AncestorRequestStatus>,
        stats: &mut AncestorHashesResponsesStats,
        outstanding_requests: &RwLock<OutstandingAncestorHashesRepairs>,
        blockstore: &Blockstore,
        keypair: &Keypair,
        ancestor_socket: &UdpSocket,
    ) -> Option<AncestorRequestDecision> {
        let from_addr = packet.meta().socket_addr();
        let Some(packet_data) = packet.data(..) else {
            stats.invalid_packets += 1;
            return None;
        };
        let mut cursor = Cursor::new(packet_data);
        let Ok(response) = wincode::config::deserialize_from(&mut cursor, packet_config()) else {
            stats.invalid_packets += 1;
            return None;
        };

        match response {
            AncestorHashesResponse::Hashes(ref hashes) => {
                // deserialize trailing nonce
                let Ok(nonce) = wincode::config::deserialize_from(&mut cursor, packet_config())
                else {
                    stats.invalid_packets += 1;
                    return None;
                };

                // verify that packet does not contain extraneous data
                if cursor.bytes().next().is_some() {
                    stats.invalid_packets += 1;
                    return None;
                }

                let request_slot = outstanding_requests.write().unwrap().register_response(
                    nonce,
                    &response,
                    timestamp(),
                    // If the response is valid, return the slot the request
                    // was for
                    |ancestor_hashes_request| ancestor_hashes_request.0,
                );

                if request_slot.is_none() {
                    stats.invalid_packets += 1;
                    return None;
                }

                // If was a valid response, there must be a valid `request_slot`
                let request_slot = request_slot.unwrap();
                stats.processed += 1;

                if let Occupied(mut ancestor_hashes_status_ref) =
                    ancestor_hashes_request_statuses.entry(request_slot)
                {
                    let decision = ancestor_hashes_status_ref.get_mut().add_response(
                        &from_addr,
                        hashes.clone(),
                        blockstore,
                    );
                    let request_type = ancestor_hashes_status_ref.get().request_type();
                    if decision.is_some() {
                        // Once a request is completed, remove it from the map so that new
                        // requests for the same slot can be made again if necessary. It's
                        // important to hold the `write` lock here via
                        // `ancestor_hashes_status_ref` so that we don't race with deletion +
                        // insertion from the `t_ancestor_requests` thread, which may
                        // 1) Remove expired statuses from `ancestor_hashes_request_statuses`
                        // 2) Insert another new one via `manage_ancestor_requests()`.
                        // In which case we wouldn't want to delete the newly inserted entry here.
                        ancestor_hashes_status_ref.remove();
                    }
                    decision.map(|decision| AncestorRequestDecision {
                        slot: request_slot,
                        decision,
                        request_type,
                    })
                } else {
                    None
                }
            }
            AncestorHashesResponse::Ping(ping) => {
                // verify that packet does not contain extraneous data
                if cursor.bytes().next().is_some() {
                    stats.invalid_packets += 1;
                    return None;
                }
                if !ping.verify() {
                    stats.ping_err_verify_count += 1;
                    return None;
                }
                stats.ping_count += 1;
                let pong = RepairProtocol::Pong(Pong::new(&ping, keypair));
                if let Ok(pong) = wincode::serialize(&pong) {
                    let _ = ancestor_socket.send_to(&pong, from_addr);
                }
                None
            }
        }
    }

    fn handle_ancestor_request_decision(
        ancestor_request_decision: AncestorRequestDecision,
        ancestor_duplicate_slots_sender: &AncestorDuplicateSlotsSender,
        retryable_slots_sender: &RetryableSlotsSender,
    ) {
        if ancestor_request_decision.is_retryable()
            && let Err(TrySendError::Full(_)) = retryable_slots_sender.try_send((
                ancestor_request_decision.slot,
                ancestor_request_decision.request_type,
            ))
        {
            warn!("Dropping ancestor request decision - retryable_slots channel is full");
        }

        // TODO: In the case of DuplicateAncestorDecision::ContinueSearch
        // This means all the ancestors were mismatched, which
        // means the earliest mismatched ancestor has yet to be found.
        //
        // In the best case scenario, this means after ReplayStage dumps
        // the earliest known ancestor `A` here, and then repairs `A`,
        // because we may still have the incorrect version of some ancestor
        // of `A`, we will mark `A` as dead and then continue the search
        // protocol through another round of ancestor repairs.
        //
        // However this process is a bit slow, so in an ideal world, the
        // protocol could be extended to keep searching by making
        // another ancestor repair request from the earliest returned
        // ancestor from this search.

        let potential_slot_to_repair = ancestor_request_decision.slot_to_repair();

        // Now signal ReplayStage about the new updated slot. It's important to do this
        // AFTER we've removed the ancestor_hashes_status_ref in case replay
        // then sends us another dead slot signal based on the updates we are
        // about to send.
        if let Some(slot_to_repair) = potential_slot_to_repair {
            // Signal ReplayStage to dump the fork that is descended from
            // `earliest_mismatched_slot_to_dump`.
            let _ = ancestor_duplicate_slots_sender.send(slot_to_repair);
        }
    }

    fn process_replay_updates(
        ancestor_hashes_replay_update_receiver: &AncestorHashesReplayUpdateReceiver,
        ancestor_hashes_request_statuses: &DashMap<Slot, AncestorRequestStatus>,
        dead_slot_pool: &mut HashSet<Slot>,
        repairable_dead_slot_pool: &mut HashSet<Slot>,
        popular_pruned_slot_pool: &mut HashSet<Slot>,
        root_slot: Slot,
    ) {
        for update in ancestor_hashes_replay_update_receiver.try_iter() {
            let slot = update.slot();
            if slot <= root_slot || ancestor_hashes_request_statuses.contains_key(&slot) {
                continue;
            }
            match update {
                AncestorHashesReplayUpdate::Dead(dead_slot) => {
                    if repairable_dead_slot_pool.contains(&dead_slot) {
                        continue;
                    } else if popular_pruned_slot_pool.contains(&dead_slot) {
                        // If `dead_slot` is also part of a popular pruned fork, this implies that the slot has
                        // become `EpochSlotsFrozen` as 52% had to have frozen some version of this slot in order
                        // to vote on it / it's descendants as observed in `repair_weight`.
                        // This fits the alternate criteria we use in `find_epoch_slots_frozen_dead_slots`
                        // so we can upgrade it to `repairable_dead_slot_pool`.
                        popular_pruned_slot_pool.remove(&dead_slot);
                        repairable_dead_slot_pool.insert(dead_slot);
                    } else {
                        dead_slot_pool.insert(dead_slot);
                    }
                }
                AncestorHashesReplayUpdate::DeadDuplicateConfirmed(dead_slot) => {
                    // If this slot was previously queued as a popular pruned slot, prefer to
                    // instead process it as dead duplicate confirmed.
                    // In general we prefer to use the dead duplicate confirmed pathway
                    // whenever possible as it allows us to compare frozen hashes of ancestors with
                    // the cluster rather than just comparing ancestry links.
                    popular_pruned_slot_pool.remove(&dead_slot);

                    dead_slot_pool.remove(&dead_slot);
                    repairable_dead_slot_pool.insert(dead_slot);
                }
                AncestorHashesReplayUpdate::PopularPrunedFork(pruned_slot) => {
                    // The `dead_slot_pool` or `repairable_dead_slot_pool` can already contain this slot already
                    // if the below order of events happens:
                    //
                    // 1. Slot is marked dead/duplicate confirmed
                    // 2. Slot is pruned
                    if dead_slot_pool.contains(&pruned_slot) {
                        // Similar to the above case where `pruned_slot` was first pruned and then marked
                        // dead, since `pruned_slot` is part of a popular pruned fork it has become
                        // `EpochSlotsFrozen` as 52% must have frozen a version of this slot in
                        // order to vote.
                        // This fits the alternate criteria we use in `find_epoch_slots_frozen_dead_slots`
                        // so we can upgrade it to `repairable_dead_slot_pool`.
                        info!(
                            "{pruned_slot} is part of a popular pruned fork however we previously \
                             marked it as dead. Upgrading as dead duplicate confirmed"
                        );
                        dead_slot_pool.remove(&pruned_slot);
                        repairable_dead_slot_pool.insert(pruned_slot);
                    } else if repairable_dead_slot_pool.contains(&pruned_slot) {
                        // If we already observed `pruned_slot` as dead duplicate confirmed, we
                        // ignore the additional information that `pruned_slot` is popular pruned.
                        // This is similar to the above case where `pruned_slot` was first pruned
                        // and then marked dead duplicate confirmed.
                        info!(
                            "Received pruned duplicate confirmed status for {pruned_slot} that \
                             was previously marked dead duplicate confirmed. Ignoring and \
                             processing it as dead duplicate confirmed."
                        );
                    } else {
                        popular_pruned_slot_pool.insert(pruned_slot);
                    }
                }
            }
        }
    }

    fn run_manage_ancestor_requests(
        blockstore: Arc<Blockstore>,
        ancestor_hashes_request_statuses: Arc<DashMap<Slot, AncestorRequestStatus>>,
        ancestor_hashes_request_socket: Arc<UdpSocket>,
        repair_info: RepairInfo,
        outstanding_requests: Arc<RwLock<OutstandingAncestorHashesRepairs>>,
        exit: Arc<AtomicBool>,
        ancestor_hashes_replay_update_receiver: AncestorHashesReplayUpdateReceiver,
        retryable_slots_receiver: RetryableSlotsReceiver,
    ) -> JoinHandle<()> {
        let migration_status = repair_info.bank_forks.read().unwrap().migration_status();
        let serve_repair = {
            let bank_forks_r = repair_info.bank_forks.read().unwrap();
            ServeRepair::new(
                repair_info.cluster_info.clone(),
                bank_forks_r.sharable_banks(),
                repair_info.repair_whitelist.clone(),
                Box::new(StandardRepairHandler::new(blockstore)),
                bank_forks_r.migration_status(),
            )
        };
        let mut repair_stats = AncestorRepairRequestsStats::default();

        let mut dead_slot_pool = HashSet::new();
        let mut repairable_dead_slot_pool = HashSet::new();
        // We keep a separate pool for slots that are part of popular pruned forks, (reached 52+%
        // in repair weight). Since these slots are pruned, we most likely do not have frozen
        // hashes for the ancestors. Because of this we process responses differently, using only
        // slot number to find the missing/invalid ancestor to dump & repair.
        //
        // However if slots are pruned and also dead/dead duplicate confirmed we give extra priority
        // to the dead pathway, preferring to add the slot to `repairable_dead_slot_pool` instead. This is
        // because if the slot is dead we must have been able to replay the ancestor. If the ancestors
        // have frozen hashes we should compare hashes instead of raw ancestry as hashes give more information
        // in finding missing/invalid ancestors.
        let mut popular_pruned_slot_pool = HashSet::new();

        // Sliding window that limits the number of slots repaired via AncestorRepair
        // to MAX_ANCESTOR_HASHES_SLOT_REQUESTS_PER_SECOND/second
        let mut request_throttle = vec![];
        Builder::new()
            .name("solManAncReqs".to_string())
            .spawn(move || {
                loop {
                    if exit.load(Ordering::Relaxed) || migration_status.is_alpenglow_enabled() {
                        return;
                    }
                    Self::manage_ancestor_requests(
                        &ancestor_hashes_request_statuses,
                        &ancestor_hashes_request_socket,
                        &repair_info,
                        &outstanding_requests,
                        &ancestor_hashes_replay_update_receiver,
                        &retryable_slots_receiver,
                        &serve_repair,
                        &mut repair_stats,
                        &mut dead_slot_pool,
                        &mut repairable_dead_slot_pool,
                        &mut popular_pruned_slot_pool,
                        &mut request_throttle,
                    );

                    let sleep_duration = Duration::from_nanos_u128(
                        repair_info
                            .bank_forks
                            .read()
                            .unwrap()
                            .root_bank()
                            .ns_per_slot,
                    );
                    sleep(sleep_duration);
                }
            })
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn manage_ancestor_requests(
        ancestor_hashes_request_statuses: &DashMap<Slot, AncestorRequestStatus>,
        ancestor_hashes_request_socket: &UdpSocket,
        repair_info: &RepairInfo,
        outstanding_requests: &RwLock<OutstandingAncestorHashesRepairs>,
        ancestor_hashes_replay_update_receiver: &AncestorHashesReplayUpdateReceiver,
        retryable_slots_receiver: &RetryableSlotsReceiver,
        serve_repair: &ServeRepair,
        repair_stats: &mut AncestorRepairRequestsStats,
        dead_slot_pool: &mut HashSet<Slot>,
        repairable_dead_slot_pool: &mut HashSet<Slot>,
        popular_pruned_slot_pool: &mut HashSet<Slot>,
        request_throttle: &mut Vec<u64>,
    ) {
        let root_bank = repair_info.bank_forks.read().unwrap().root_bank();
        for (slot, request_type) in retryable_slots_receiver.try_iter() {
            datapoint_info!("ancestor-repair-retry", ("slot", slot, i64));
            if request_type.is_pruned() {
                popular_pruned_slot_pool.insert(slot);
            } else {
                repairable_dead_slot_pool.insert(slot);
            }
        }

        Self::process_replay_updates(
            ancestor_hashes_replay_update_receiver,
            ancestor_hashes_request_statuses,
            dead_slot_pool,
            repairable_dead_slot_pool,
            popular_pruned_slot_pool,
            root_bank.slot(),
        );

        Self::find_epoch_slots_frozen_dead_slots(
            &repair_info.cluster_slots,
            dead_slot_pool,
            repairable_dead_slot_pool,
            &root_bank,
        );

        dead_slot_pool.retain(|slot| *slot > root_bank.slot());
        repairable_dead_slot_pool.retain(|slot| *slot > root_bank.slot());
        popular_pruned_slot_pool.retain(|slot| *slot > root_bank.slot());

        ancestor_hashes_request_statuses.retain(|slot, status| {
            if *slot <= root_bank.slot() {
                false
            } else if status.is_expired() {
                // Add the slot back to the correct pool to retry
                if status.request_type().is_pruned() {
                    popular_pruned_slot_pool.insert(*slot);
                } else {
                    repairable_dead_slot_pool.insert(*slot);
                }
                false
            } else {
                true
            }
        });

        // Keep around the last second of requests in the throttler.
        request_throttle.retain(|request_time| *request_time > (timestamp() - 1000));

        let identity_keypair: &Keypair = &repair_info.cluster_info.keypair();

        let number_of_allowed_requests =
            MAX_ANCESTOR_HASHES_SLOT_REQUESTS_PER_SECOND.saturating_sub(request_throttle.len());

        // Find dead and pruned slots for which it's worthwhile to ask the network for their
        // ancestors, prioritizing dead slots first.
        let potential_slot_requests = repairable_dead_slot_pool
            .iter()
            .copied()
            .zip(std::iter::repeat(
                AncestorRequestType::DeadDuplicateConfirmed,
            ))
            .chain(
                popular_pruned_slot_pool
                    .iter()
                    .copied()
                    .zip(std::iter::repeat(AncestorRequestType::PopularPruned)),
            )
            .collect::<Vec<_>>()
            .into_iter();

        for (slot, request_type) in potential_slot_requests.take(number_of_allowed_requests) {
            warn!(
                "Cluster froze slot: {slot}, but we marked it as {}. Initiating protocol to \
                 sample cluster for dead slot ancestors.",
                if request_type.is_pruned() {
                    "pruned"
                } else {
                    "dead"
                },
            );

            if Self::initiate_ancestor_hashes_requests_for_duplicate_slot(
                ancestor_hashes_request_statuses,
                ancestor_hashes_request_socket,
                &repair_info.cluster_slots,
                serve_repair,
                &repair_info.repair_validators,
                slot,
                repair_stats,
                outstanding_requests,
                identity_keypair,
                request_type,
            ) {
                request_throttle.push(timestamp());
                if request_type.is_pruned() {
                    popular_pruned_slot_pool.take(&slot).unwrap();
                } else {
                    repairable_dead_slot_pool.take(&slot).unwrap();
                }
            }
        }
        repair_stats.report();
    }

    /// Find if any dead slots in `dead_slot_pool` have been frozen by sufficient
    /// number of nodes in the cluster to justify adding to the `repairable_dead_slot_pool`.
    fn find_epoch_slots_frozen_dead_slots(
        cluster_slots: &ClusterSlots,
        dead_slot_pool: &mut HashSet<Slot>,
        repairable_dead_slot_pool: &mut HashSet<Slot>,
        root_bank: &Bank,
    ) {
        dead_slot_pool.retain(|dead_slot| {
            let epoch = root_bank.get_epoch_and_slot_index(*dead_slot).0;
            //TODO: figure out if we even need to make this check
            if let Some(_epoch_stakes) = root_bank.epoch_stakes(epoch) {
                let status = cluster_slots.lookup(*dead_slot);
                if let Some(completed_dead_slot_supporters) = status {
                    let total_stake = completed_dead_slot_supporters.total_stake();
                    // If sufficient number of validators froze this slot, then there's a chance
                    // this dead slot was duplicate confirmed and will make it into in the main fork.
                    // This means it's worth asking the cluster to get the correct version.
                    if completed_dead_slot_supporters.total_support() as f64 / total_stake as f64
                        > DUPLICATE_THRESHOLD
                    {
                        repairable_dead_slot_pool.insert(*dead_slot);
                        false
                    } else {
                        true
                    }
                } else {
                    true
                }
            } else {
                warn!(
                    "Dead slot {} is too far ahead of root bank {}",
                    dead_slot,
                    root_bank.slot()
                );
                false
            }
        })
    }

    /// Returns true if a request was successfully made and the status
    /// added to `ancestor_hashes_request_statuses`
    #[allow(clippy::too_many_arguments)]
    fn initiate_ancestor_hashes_requests_for_duplicate_slot(
        ancestor_hashes_request_statuses: &DashMap<Slot, AncestorRequestStatus>,
        ancestor_hashes_request_socket: &UdpSocket,
        cluster_slots: &ClusterSlots,
        serve_repair: &ServeRepair,
        repair_validators: &Option<HashSet<Pubkey>>,
        duplicate_slot: Slot,
        repair_stats: &mut AncestorRepairRequestsStats,
        outstanding_requests: &RwLock<OutstandingAncestorHashesRepairs>,
        identity_keypair: &Keypair,
        request_type: AncestorRequestType,
    ) -> bool {
        let Ok(sampled_validators) = serve_repair.repair_request_ancestor_hashes_sample_peers(
            duplicate_slot,
            cluster_slots,
            repair_validators,
            Protocol::UDP,
            &identity_keypair.pubkey(),
        ) else {
            return false;
        };

        for (pubkey, socket_addr) in &sampled_validators {
            repair_stats
                .ancestor_requests
                .update(pubkey, duplicate_slot, 0);
            let ancestor_hashes_repair_type = AncestorHashesRepairType(duplicate_slot);
            let nonce = outstanding_requests
                .write()
                .unwrap()
                .add_request(ancestor_hashes_repair_type, timestamp());
            let Ok(request_bytes) = serve_repair.ancestor_repair_request_bytes(
                identity_keypair,
                pubkey,
                duplicate_slot,
                nonce,
            ) else {
                continue;
            };
            let _ = ancestor_hashes_request_socket.send_to(&request_bytes, socket_addr);
        }

        let ancestor_request_status = AncestorRequestStatus::new(
            sampled_validators
                .into_iter()
                .map(|(_pk, socket_addr)| socket_addr),
            duplicate_slot,
            request_type,
        );
        assert!(
            ancestor_hashes_request_statuses
                .insert(duplicate_slot, ancestor_request_status)
                .is_none()
        );
        true
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_gossip::{cluster_info::ClusterInfo, node::Node},
        solana_keypair::Keypair,
        solana_ledger::get_tmp_ledger_path_auto_delete,
        solana_net_utils::{SocketAddrSpace, sockets::bind_to_localhost_unique},
        solana_perf::packet::BytesPacket,
        solana_runtime::bank_forks::BankForks,
        solana_signer::Signer,
    };

    #[test]
    pub fn test_ancestor_hashes_service_process_replay_updates() {
        let (ancestor_hashes_replay_update_sender, ancestor_hashes_replay_update_receiver) =
            bounded(1024);
        let ancestor_hashes_request_statuses = DashMap::new();
        let mut dead_slot_pool = HashSet::new();
        let mut repairable_dead_slot_pool = HashSet::new();
        let mut popular_pruned_slot_pool = HashSet::new();
        let slot = 10;
        let mut root_slot = 0;

        // 1) Getting a dead signal should only add the slot to the dead pool
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::Dead(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.contains(&slot));
        assert!(repairable_dead_slot_pool.is_empty());
        assert!(popular_pruned_slot_pool.is_empty());

        // 2) Getting a duplicate confirmed dead slot should move the slot
        // from the dead pool to the repairable pool
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.contains(&slot));
        assert!(popular_pruned_slot_pool.is_empty());

        // 3) Getting another dead signal should not add it back to the dead pool
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::Dead(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.contains(&slot));
        assert!(popular_pruned_slot_pool.is_empty());

        // 4) If an outstanding request (pruned or regular) for a slot already exists, should
        // ignore any signals from replay stage
        ancestor_hashes_request_statuses.insert(slot, AncestorRequestStatus::default());
        dead_slot_pool.clear();
        repairable_dead_slot_pool.clear();
        popular_pruned_slot_pool.clear();
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::Dead(slot))
            .unwrap();
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(slot))
            .unwrap();
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::PopularPrunedFork(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.is_empty());
        assert!(popular_pruned_slot_pool.is_empty());

        ancestor_hashes_request_statuses.insert(
            slot,
            AncestorRequestStatus::new(
                std::iter::empty(),
                slot,
                AncestorRequestType::PopularPruned,
            ),
        );
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::Dead(slot))
            .unwrap();
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(slot))
            .unwrap();
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::PopularPrunedFork(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.is_empty());
        assert!(popular_pruned_slot_pool.is_empty());

        // 5) If we get any signals for slots <= root_slot, they should be ignored
        root_slot = 15;
        ancestor_hashes_request_statuses.clear();
        dead_slot_pool.clear();
        repairable_dead_slot_pool.clear();
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::Dead(root_slot - 1))
            .unwrap();
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(
                root_slot - 2,
            ))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.is_empty());
    }

    #[test]
    pub fn test_ancestor_hashes_service_process_pruned_replay_updates() {
        let (ancestor_hashes_replay_update_sender, ancestor_hashes_replay_update_receiver) =
            bounded(1024);
        let ancestor_hashes_request_statuses = DashMap::new();
        let mut dead_slot_pool = HashSet::new();
        let mut repairable_dead_slot_pool = HashSet::new();
        let mut popular_pruned_slot_pool = HashSet::new();
        let slot = 10;
        let root_slot = 0;

        // 1) Getting a popular pruned signal should add it to the popular pruned pool
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::PopularPrunedFork(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.is_empty());
        assert!(popular_pruned_slot_pool.contains(&slot));

        // 2) Receiving a dead signal afterwards should upgrade it to dead duplicate confirmed
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::Dead(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.contains(&slot));
        assert!(popular_pruned_slot_pool.is_empty());

        // 3) Instead if we receive a dead duplicate confirmed afterwards it should also be
        //    upgraded to dead duplicate confirmed
        repairable_dead_slot_pool.clear();
        popular_pruned_slot_pool.insert(slot);
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.contains(&slot));
        assert!(popular_pruned_slot_pool.is_empty());

        // 4) Receiving a popular pruned after it has been dead duplicate confirmed should do nothing
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::PopularPrunedFork(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.contains(&slot));
        assert!(popular_pruned_slot_pool.is_empty());

        // 5) Instead, receiving a popular pruned after it has only been marked dead should upgrade it to dead
        //    duplicate confirmed
        repairable_dead_slot_pool.clear();
        dead_slot_pool.insert(slot);
        ancestor_hashes_replay_update_sender
            .send(AncestorHashesReplayUpdate::PopularPrunedFork(slot))
            .unwrap();
        AncestorHashesService::process_replay_updates(
            &ancestor_hashes_replay_update_receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );
        assert!(dead_slot_pool.is_empty());
        assert!(repairable_dead_slot_pool.contains(&slot));
        assert!(popular_pruned_slot_pool.is_empty());
    }

    struct ManageAncestorHashesState {
        ancestor_hashes_request_statuses: Arc<DashMap<Slot, AncestorRequestStatus>>,
        ancestor_hashes_request_socket: Arc<UdpSocket>,
        repair_info: RepairInfo,
        outstanding_requests: Arc<RwLock<OutstandingAncestorHashesRepairs>>,
    }

    impl ManageAncestorHashesState {
        fn new(bank_forks: Arc<RwLock<BankForks>>) -> Self {
            let ancestor_hashes_request_statuses = Arc::new(DashMap::new());
            let ancestor_hashes_request_socket =
                Arc::new(bind_to_localhost_unique().expect("should bind"));
            let epoch_schedule = bank_forks
                .read()
                .unwrap()
                .root_bank()
                .epoch_schedule()
                .clone();

            let keypair = Keypair::new();
            let requester_cluster_info = Arc::new(ClusterInfo::new(
                Node::new_localhost_with_pubkey(&keypair.pubkey()).info,
                Arc::new(keypair),
                SocketAddrSpace::Unspecified,
            ));
            let repair_whitelist = Arc::new(RwLock::new(HashSet::default()));
            let (ancestor_duplicate_slots_sender, _ancestor_duplicate_slots_receiver) =
                bounded(1024);
            let repair_info = RepairInfo {
                bank_forks,
                cluster_info: requester_cluster_info,
                cluster_slots: Arc::new(ClusterSlots::default_for_tests()),
                epoch_schedule,
                ancestor_duplicate_slots_sender,
                repair_validators: None,
                repair_whitelist,
            };

            Self {
                ancestor_hashes_request_statuses,
                ancestor_hashes_request_socket,
                repair_info,
                outstanding_requests: Arc::new(RwLock::new(
                    OutstandingAncestorHashesRepairs::default(),
                )),
            }
        }
    }

    #[test]
    fn test_verify_and_process_ancestor_responses_invalid_packet() {
        let bank0 = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank0);

        let ManageAncestorHashesState {
            ancestor_hashes_request_statuses,
            ancestor_hashes_request_socket,
            outstanding_requests,
            repair_info,
            ..
        } = ManageAncestorHashesState::new(bank_forks);

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        // Create invalid packet with fewer bytes than the size of the nonce
        let packet = BytesPacket::empty();

        assert!(
            AncestorHashesService::verify_and_process_ancestor_response(
                &packet,
                &ancestor_hashes_request_statuses,
                &mut AncestorHashesResponsesStats::default(),
                &outstanding_requests,
                &blockstore,
                &repair_info.cluster_info.keypair(),
                &ancestor_hashes_request_socket,
            )
            .is_none()
        );
    }

    #[test]
    fn test_process_replay_updates_continue_after_skipped_update() {
        let (sender, receiver) = bounded(1024);
        let ancestor_hashes_request_statuses = DashMap::new();
        let mut dead_slot_pool = HashSet::new();
        let mut repairable_dead_slot_pool = HashSet::new();
        let mut popular_pruned_slot_pool = HashSet::new();

        let root_slot: Slot = 15;
        let skipped_slot = root_slot; // should be skipped due to slot <= root
        let actionable_slot = root_slot + 2; // should be processed

        sender
            .send(AncestorHashesReplayUpdate::Dead(skipped_slot))
            .unwrap();
        sender
            .send(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(
                actionable_slot,
            ))
            .unwrap();

        // Run processing; previously, an early return would have aborted after the first
        // (skipped) update and ignored the actionable second update in the same batch.
        AncestorHashesService::process_replay_updates(
            &receiver,
            &ancestor_hashes_request_statuses,
            &mut dead_slot_pool,
            &mut repairable_dead_slot_pool,
            &mut popular_pruned_slot_pool,
            root_slot,
        );

        // The skipped update should not pollute pools
        assert!(!dead_slot_pool.contains(&skipped_slot));

        // The actionable update must have been processed in the same call
        assert!(repairable_dead_slot_pool.contains(&actionable_slot));
    }
}
