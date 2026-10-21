use {
    crate::{
        duplicate_shred::{self, DuplicateShred, Error},
        duplicate_shred_listener::DuplicateShredHandlerTrait,
        epoch_specs::EpochSpecs,
    },
    agave_votor_messages::migration::MigrationStatus,
    crossbeam_channel::Sender,
    log::error,
    solana_clock::Slot,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    std::{cmp::Reverse, collections::HashMap, sync::Arc},
};

// Normally num_chunks is 3, because there are two shreds (each is one packet)
// and meta data. So we discard anything larger than 3 chunks.
const MAX_NUM_CHUNKS: usize = 3;
// Limit number of entries per node.
const MAX_NUM_ENTRIES_PER_PUBKEY: usize = 128;
const BUFFER_CAPACITY: usize = 512 * MAX_NUM_ENTRIES_PER_PUBKEY;

type BufferEntry = [Option<DuplicateShred>; MAX_NUM_CHUNKS];

pub struct DuplicateShredHandler {
    // Because we use UDP for packet transfer, we can normally only send ~1500 bytes
    // in each packet. We send both shreds and meta data in duplicate shred proof, and
    // each shred is normally 1 packet(1500 bytes), so the whole proof is larger than
    // 1 packet and it needs to be cut down as chunks for transfer. So we need to piece
    // together the chunks into the original proof before anything useful is done.
    buffer: HashMap<(Slot, Pubkey), BufferEntry>,
    // Slots for which a duplicate proof is already ingested.
    consumed: HashMap<Slot, bool>,
    // Cache last root to reduce read lock.
    last_root: Slot,
    blockstore: Arc<Blockstore>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    epoch_specs: Box<dyn EpochSpecs>,
    cached_slots_in_epoch: u64,
    // Used to notify duplicate consensus state machine
    duplicate_slots_sender: Sender<Slot>,
    shred_version: u16,
    /// Alpenglow migration status
    migration_status: Arc<MigrationStatus>,
}

impl DuplicateShredHandlerTrait for DuplicateShredHandler {
    // Here we are sending data one by one rather than in a batch because in the future
    // we may send different type of CrdsData to different senders.
    fn handle(&mut self, shred_data: DuplicateShred) {
        if self.migration_status.is_full_alpenglow_epoch() {
            // turn into noop and clear any existing buffer
            self.buffer.clear();
            return;
        }
        self.cache_root_info();
        self.maybe_prune_buffer();
        let slot = shred_data.slot;
        let pubkey = shred_data.from;
        if let Err(error) = self.handle_shred_data(shred_data) {
            if error.is_non_critical() {
                info!(
                    "Received invalid duplicate shred proof from {pubkey} for slot {slot}: \
                     {error:?}"
                );
            } else {
                error!(
                    "Unable to process duplicate shred proof from {pubkey} for slot {slot}: \
                     {error:?}"
                );
            }
        }
    }
}

impl DuplicateShredHandler {
    pub fn new(
        blockstore: Arc<Blockstore>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        epoch_specs: Box<dyn EpochSpecs>,
        duplicate_slots_sender: Sender<Slot>,
        shred_version: u16,
        migration_status: Arc<MigrationStatus>,
    ) -> Self {
        Self {
            buffer: HashMap::<(Slot, Pubkey), BufferEntry>::default(),
            consumed: HashMap::<Slot, bool>::default(),
            last_root: 0,
            cached_slots_in_epoch: 0,
            blockstore,
            leader_schedule_cache,
            epoch_specs,
            duplicate_slots_sender,
            shred_version,
            migration_status,
        }
    }

    fn cache_root_info(&mut self) {
        let last_root = self.epoch_specs.root_slot();
        if last_root == self.last_root && self.cached_slots_in_epoch != 0 {
            return;
        }
        self.last_root = last_root;
        self.cached_slots_in_epoch = self.epoch_specs.epoch_slots();
    }

    fn handle_shred_data(&mut self, chunk: DuplicateShred) -> Result<(), Error> {
        if !self.should_consume_slot(chunk.slot) {
            return Ok(());
        }
        let slot = chunk.slot;
        let num_chunks = chunk.num_chunks();
        let chunk_index = chunk.chunk_index();
        if usize::from(num_chunks) > MAX_NUM_CHUNKS || chunk_index >= num_chunks {
            return Err(Error::InvalidChunkIndex {
                chunk_index,
                num_chunks,
            });
        }
        let entry = self.buffer.entry((chunk.slot, chunk.from)).or_default();
        *entry
            .get_mut(usize::from(chunk_index))
            .ok_or(Error::InvalidChunkIndex {
                chunk_index,
                num_chunks,
            })? = Some(chunk);
        // If all chunks are already received, reconstruct and store
        // the duplicate slot proof in blockstore
        if entry.iter().flatten().count() == usize::from(num_chunks) {
            let chunks = std::mem::take(entry).into_iter().flatten();
            let slot_leader = self
                .leader_schedule_cache
                .slot_leader_at(slot, /*bank:*/ None)
                .ok_or(Error::UnknownSlotLeader(slot))?;
            let (shred1, shred2) = duplicate_shred::into_shreds(
                &slot_leader.id,
                chunks,
                self.shred_version,
                self.epoch_specs.should_enforce_correct_proof_size(slot),
            )?;
            if !self.blockstore.has_duplicate_shreds_in_slot(slot) {
                self.blockstore.store_duplicate_slot(
                    slot,
                    shred1.into_payload(),
                    shred2.into_payload(),
                )?;

                // Notify the Tower duplicate-consensus state machine. ReplayStage stops
                // consuming this channel once Alpenglow is enabled, while duplicate proofs are
                // still stored during the mixed epoch for slashing.
                if !self.migration_status.is_alpenglow_enabled()
                    && self.duplicate_slots_sender.len() * 2
                        < self.duplicate_slots_sender.capacity().unwrap_or(usize::MAX)
                {
                    self.duplicate_slots_sender
                        .try_send(slot)
                        .map_err(|_| Error::DuplicateSlotSenderFailure)?;
                }
            }
            self.consumed.insert(slot, true);
        }
        Ok(())
    }

    fn should_consume_slot(&mut self, slot: Slot) -> bool {
        slot > self.last_root
            && slot < self.last_root.saturating_add(self.cached_slots_in_epoch)
            && should_consume_slot(slot, &self.blockstore, &mut self.consumed)
    }

    fn maybe_prune_buffer(&mut self) {
        // The buffer is allowed to grow to twice the intended capacity, at
        // which point the extraneous entries are removed in linear time,
        // resulting an amortized O(1) performance.
        if self.buffer.len() < BUFFER_CAPACITY.saturating_mul(2) {
            return;
        }
        self.consumed.retain(|&slot, _| slot > self.last_root);
        // Filter out obsolete slots and limit number of entries per pubkey.
        {
            let mut counts = HashMap::<Pubkey, usize>::new();
            self.buffer.retain(|(slot, pubkey), _| {
                *slot > self.last_root
                    && should_consume_slot(*slot, &self.blockstore, &mut self.consumed)
                    && {
                        let count = counts.entry(*pubkey).or_default();
                        *count = count.saturating_add(1);
                        *count <= MAX_NUM_ENTRIES_PER_PUBKEY
                    }
            });
        }
        if self.buffer.len() <= BUFFER_CAPACITY {
            return;
        }
        // Lookup stake for each entry.
        let staked_nodes = self.epoch_specs.current_epoch_staked_nodes();
        let mut buffer: Vec<_> = self
            .buffer
            .drain()
            .map(|entry @ ((_, pubkey), _)| {
                let stake = staked_nodes.get(&pubkey).copied().unwrap_or_default();
                (stake, entry)
            })
            .collect();
        // Drop entries with lowest stake and rebuffer remaining ones.
        buffer.select_nth_unstable_by_key(BUFFER_CAPACITY, |&(stake, _)| Reverse(stake));
        self.buffer.extend(
            buffer
                .into_iter()
                .take(BUFFER_CAPACITY)
                .map(|(_, entry)| entry),
        );
    }
}

// Returns false if a duplicate proof is already ingested for the slot,
// and updates local `consumed` cache with blockstore.
fn should_consume_slot(
    slot: Slot,
    blockstore: &Blockstore,
    consumed: &mut HashMap<Slot, bool>,
) -> bool {
    !*consumed
        .entry(slot)
        .or_insert_with(|| blockstore.has_duplicate_shreds_in_slot(slot))
}
