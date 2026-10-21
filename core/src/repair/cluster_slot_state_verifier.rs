#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        consensus::{
            fork_choice::ForkChoice, heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
        },
        repair::ancestor_hashes_service::{
            AncestorHashesReplayUpdate, AncestorHashesReplayUpdateSender,
        },
    },
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::blockstore::Blockstore,
    std::collections::{BTreeMap, BTreeSet, HashMap},
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) type DuplicateSlotsTracker = BTreeSet<Slot>;
pub(crate) type DuplicateSlotsToRepair = HashMap<Slot, Hash>;
pub(crate) type PurgeRepairSlotCounter = BTreeMap<Slot, usize>;
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) type EpochSlotsFrozenSlots = BTreeMap<Slot, Hash>;
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) type DuplicateConfirmedSlots = BTreeMap<Slot, Hash>;

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum ClusterConfirmedHash {
    // Ordered from strongest confirmation to weakest. Stronger
    // confirmations take precedence over weaker ones.
    DuplicateConfirmed(Hash),
    EpochSlotsFrozen(Hash),
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub enum BankStatus {
    Frozen(Hash),
    Dead,
    Unprocessed,
}

impl BankStatus {
    pub fn new(is_dead: impl Fn() -> bool, get_hash: impl Fn() -> Option<Hash>) -> Self {
        if is_dead() {
            Self::new_dead()
        } else {
            Self::new_from_hash(get_hash())
        }
    }

    fn new_dead() -> Self {
        BankStatus::Dead
    }

    fn new_from_hash(hash: Option<Hash>) -> Self {
        if let Some(hash) = hash {
            if hash == Hash::default() {
                BankStatus::Unprocessed
            } else {
                BankStatus::Frozen(hash)
            }
        } else {
            BankStatus::Unprocessed
        }
    }

    fn bank_hash(&self) -> Option<Hash> {
        match self {
            BankStatus::Frozen(hash) => Some(*hash),
            BankStatus::Dead => None,
            BankStatus::Unprocessed => None,
        }
    }

    fn is_dead(&self) -> bool {
        match self {
            BankStatus::Frozen(_) => false,
            BankStatus::Dead => true,
            BankStatus::Unprocessed => false,
        }
    }

    fn can_be_further_replayed(&self) -> bool {
        match self {
            BankStatus::Unprocessed => true,
            BankStatus::Dead => false,
            BankStatus::Frozen(_) => false,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct DeadState {
    // Keep fields private, forces construction
    // via constructor
    cluster_confirmed_hash: Option<ClusterConfirmedHash>,
    is_slot_duplicate: bool,
}

impl DeadState {
    pub fn new_from_state(
        slot: Slot,
        duplicate_slots_tracker: &DuplicateSlotsTracker,
        duplicate_confirmed_slots: &DuplicateConfirmedSlots,
        fork_choice: &HeaviestSubtreeForkChoice,
        epoch_slots_frozen_slots: &EpochSlotsFrozenSlots,
    ) -> Self {
        let cluster_confirmed_hash = get_cluster_confirmed_hash_from_state(
            slot,
            duplicate_confirmed_slots,
            epoch_slots_frozen_slots,
            fork_choice,
            None,
        );
        let is_slot_duplicate = duplicate_slots_tracker.contains(&slot);
        Self::new(cluster_confirmed_hash, is_slot_duplicate)
    }

    fn new(cluster_confirmed_hash: Option<ClusterConfirmedHash>, is_slot_duplicate: bool) -> Self {
        Self {
            cluster_confirmed_hash,
            is_slot_duplicate,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct BankFrozenState {
    // Keep fields private, forces construction
    // via constructor
    frozen_hash: Hash,
    cluster_confirmed_hash: Option<ClusterConfirmedHash>,
    is_slot_duplicate: bool,
}

impl BankFrozenState {
    pub fn new_from_state(
        slot: Slot,
        frozen_hash: Hash,
        duplicate_slots_tracker: &DuplicateSlotsTracker,
        duplicate_confirmed_slots: &DuplicateConfirmedSlots,
        fork_choice: &HeaviestSubtreeForkChoice,
        epoch_slots_frozen_slots: &EpochSlotsFrozenSlots,
    ) -> Self {
        let cluster_confirmed_hash = get_cluster_confirmed_hash_from_state(
            slot,
            duplicate_confirmed_slots,
            epoch_slots_frozen_slots,
            fork_choice,
            Some(frozen_hash),
        );
        let is_slot_duplicate = duplicate_slots_tracker.contains(&slot);
        Self::new(frozen_hash, cluster_confirmed_hash, is_slot_duplicate)
    }

    fn new(
        frozen_hash: Hash,
        cluster_confirmed_hash: Option<ClusterConfirmedHash>,
        is_slot_duplicate: bool,
    ) -> Self {
        assert!(frozen_hash != Hash::default());
        Self {
            frozen_hash,
            cluster_confirmed_hash,
            is_slot_duplicate,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct DuplicateConfirmedState {
    // Keep fields private, forces construction
    // via constructor
    duplicate_confirmed_hash: Hash,
    bank_status: BankStatus,
}
impl DuplicateConfirmedState {
    pub fn new_from_state(
        duplicate_confirmed_hash: Hash,
        is_dead: impl Fn() -> bool,
        get_hash: impl Fn() -> Option<Hash>,
    ) -> Self {
        let bank_status = BankStatus::new(is_dead, get_hash);
        Self::new(duplicate_confirmed_hash, bank_status)
    }

    fn new(duplicate_confirmed_hash: Hash, bank_status: BankStatus) -> Self {
        Self {
            duplicate_confirmed_hash,
            bank_status,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct DuplicateState {
    // Keep fields private, forces construction
    // via constructor
    duplicate_confirmed_hash: Option<Hash>,
    bank_status: BankStatus,
}
impl DuplicateState {
    pub fn new_from_state(
        slot: Slot,
        duplicate_confirmed_slots: &DuplicateConfirmedSlots,
        fork_choice: &HeaviestSubtreeForkChoice,
        is_dead: impl Fn() -> bool,
        get_hash: impl Fn() -> Option<Hash>,
    ) -> Self {
        let bank_status = BankStatus::new(is_dead, get_hash);

        // We can only skip marking duplicate if this slot has already been
        // duplicate confirmed, any weaker confirmation levels are not sufficient
        // to skip marking the slot as duplicate.
        let duplicate_confirmed_hash = get_duplicate_confirmed_hash_from_state(
            slot,
            duplicate_confirmed_slots,
            fork_choice,
            bank_status.bank_hash(),
        );
        Self::new(duplicate_confirmed_hash, bank_status)
    }

    fn new(duplicate_confirmed_hash: Option<Hash>, bank_status: BankStatus) -> Self {
        Self {
            duplicate_confirmed_hash,
            bank_status,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct EpochSlotsFrozenState {
    // Keep fields private, forces construction
    // via constructor
    epoch_slots_frozen_hash: Hash,
    duplicate_confirmed_hash: Option<Hash>,
    bank_status: BankStatus,
    is_popular_pruned: bool,
}
impl EpochSlotsFrozenState {
    pub fn new_from_state(
        slot: Slot,
        epoch_slots_frozen_hash: Hash,
        duplicate_confirmed_slots: &DuplicateConfirmedSlots,
        fork_choice: &HeaviestSubtreeForkChoice,
        is_dead: impl Fn() -> bool,
        get_hash: impl Fn() -> Option<Hash>,
        is_popular_pruned: bool,
    ) -> Self {
        let bank_status = BankStatus::new(is_dead, get_hash);
        let duplicate_confirmed_hash = get_duplicate_confirmed_hash_from_state(
            slot,
            duplicate_confirmed_slots,
            fork_choice,
            bank_status.bank_hash(),
        );
        Self::new(
            epoch_slots_frozen_hash,
            duplicate_confirmed_hash,
            bank_status,
            is_popular_pruned,
        )
    }

    fn new(
        epoch_slots_frozen_hash: Hash,
        duplicate_confirmed_hash: Option<Hash>,
        bank_status: BankStatus,
        is_popular_pruned: bool,
    ) -> Self {
        Self {
            epoch_slots_frozen_hash,
            duplicate_confirmed_hash,
            bank_status,
            is_popular_pruned,
        }
    }

    fn is_popular_pruned(&self) -> bool {
        self.is_popular_pruned
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum SlotStateUpdate {
    BankFrozen(BankFrozenState),
    DuplicateConfirmed(DuplicateConfirmedState),
    Dead(DeadState),
    Duplicate(DuplicateState),
    EpochSlotsFrozen(EpochSlotsFrozenState),
    // The fork is pruned but has reached `DUPLICATE_THRESHOLD` from votes aggregated across
    // descendants and all versions of the slots on this fork.
    PopularPrunedFork,
}

impl SlotStateUpdate {
    fn into_state_changes(self, slot: Slot) -> Vec<ResultingStateChange> {
        if self.can_be_further_replayed() {
            // If the bank is still awaiting replay, then there's nothing to do yet
            return vec![];
        }

        match self {
            SlotStateUpdate::Dead(dead_state) => on_dead_slot(slot, dead_state),
            SlotStateUpdate::BankFrozen(bank_frozen_state) => {
                on_frozen_slot(slot, bank_frozen_state)
            }
            SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state) => {
                on_duplicate_confirmed(slot, duplicate_confirmed_state)
            }
            SlotStateUpdate::Duplicate(duplicate_state) => on_duplicate(duplicate_state),
            SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state) => {
                on_epoch_slots_frozen(slot, epoch_slots_frozen_state)
            }
            SlotStateUpdate::PopularPrunedFork => on_popular_pruned_fork(slot),
        }
    }

    fn can_be_further_replayed(&self) -> bool {
        match self {
            SlotStateUpdate::BankFrozen(_) => false,
            SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state) => {
                duplicate_confirmed_state
                    .bank_status
                    .can_be_further_replayed()
            }
            SlotStateUpdate::Dead(_) => false,
            SlotStateUpdate::Duplicate(duplicate_state) => {
                duplicate_state.bank_status.can_be_further_replayed()
            }
            SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state) => {
                epoch_slots_frozen_state
                    .bank_status
                    .can_be_further_replayed()
                    // If we have the slot pruned then it will never be replayed
                    && !epoch_slots_frozen_state.is_popular_pruned()
            }
            SlotStateUpdate::PopularPrunedFork => false,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum ResultingStateChange {
    // Bank was frozen
    BankFrozen(Hash),
    // Hash of our current frozen version of the slot
    MarkSlotDuplicate(Hash),
    // Hash of the either:
    // 1) Cluster duplicate confirmed slot
    // 2) Epoch Slots frozen sampled slot
    // that is not equivalent to our frozen version of the slot
    RepairDuplicateConfirmedVersion(Hash),
    // Hash of our current frozen version of the slot
    DuplicateConfirmedSlotMatchesCluster(Hash),
    SendAncestorHashesReplayUpdate(AncestorHashesReplayUpdate),
}

/// Checks the duplicate confirmed hash we observed against our local bank status
///
/// 1) If we haven't replayed locally do nothing
/// 2) If our local bank is dead, mark for dump and repair
/// 3) If our local bank is replayed but mismatch hash, notify fork choice of duplicate and dump and
///    repair
/// 4) If our local bank is replayed and matches the `duplicate_confirmed_hash`, notify fork choice
///    that we have the correct version
fn check_duplicate_confirmed_hash_against_bank_status(
    state_changes: &mut Vec<ResultingStateChange>,
    slot: Slot,
    duplicate_confirmed_hash: Hash,
    bank_status: BankStatus,
) {
    match bank_status {
        BankStatus::Unprocessed => {}
        BankStatus::Dead => {
            // If the cluster duplicate confirmed some version of this slot, then
            // there's another version of our dead slot
            warn!(
                "Cluster duplicate confirmed slot {slot} with hash {duplicate_confirmed_hash}, \
                 but we marked slot dead"
            );
            state_changes.push(ResultingStateChange::RepairDuplicateConfirmedVersion(
                duplicate_confirmed_hash,
            ));
        }
        BankStatus::Frozen(bank_frozen_hash) if duplicate_confirmed_hash == bank_frozen_hash => {
            // If the versions match, then add the slot to the candidate
            // set to account for the case where it was removed earlier
            // by the `on_duplicate_slot()` handler
            state_changes.push(ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(
                bank_frozen_hash,
            ));
        }
        BankStatus::Frozen(bank_frozen_hash) => {
            // The duplicate confirmed slot hash does not match our frozen hash.
            // Modify fork choice rule to exclude our version from being voted
            // on and also repair the correct version
            warn!(
                "Cluster duplicate confirmed slot {slot} with hash {duplicate_confirmed_hash}, \
                 but our version has hash {bank_frozen_hash}"
            );
            state_changes.push(ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash));
            state_changes.push(ResultingStateChange::RepairDuplicateConfirmedVersion(
                duplicate_confirmed_hash,
            ));
        }
    }
}

/// Checks the epoch slots hash we observed against our local bank status
/// Note epoch slots here does not refer to gossip but responses from ancestor_hashes_service
///
/// * If `epoch_slots_frozen_hash` matches our local frozen hash do nothing
/// * If `slot` has not yet been replayed and is not pruned fail, as we should not be checking
///   against this bank until it is replayed.
///
/// Dump and repair the slot if any of the following occur
/// * Our version is dead
/// * Our version is popular pruned and unplayed
/// * If `epoch_slots_frozen_hash` does not match our local frozen hash (additionally notify fork
///   choice of duplicate `slot`)
fn check_epoch_slots_hash_against_bank_status(
    state_changes: &mut Vec<ResultingStateChange>,
    slot: Slot,
    epoch_slots_frozen_hash: Hash,
    bank_status: BankStatus,
    is_popular_pruned: bool,
) {
    match bank_status {
        BankStatus::Frozen(bank_frozen_hash) if bank_frozen_hash == epoch_slots_frozen_hash => {
            // Matches, nothing to do
            return;
        }
        BankStatus::Frozen(bank_frozen_hash) => {
            // The epoch slots hash does not match our frozen hash.
            warn!(
                "EpochSlots sample returned slot {slot} with hash {epoch_slots_frozen_hash}, but \
                 our version has hash {bank_frozen_hash:?}",
            );
            if !is_popular_pruned {
                // If the slot is not already pruned notify fork choice to mark as invalid
                state_changes.push(ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash));
            }
        }
        BankStatus::Dead => {
            // Cluster sample found a hash for our dead slot, we must have the wrong version
            warn!(
                "EpochSlots sample returned slot {slot} with hash {epoch_slots_frozen_hash}, but \
                 we marked slot dead",
            );
        }
        BankStatus::Unprocessed => {
            // If the bank was not popular pruned, we would never have made it here, as the bank is
            // yet to be replayed
            assert!(is_popular_pruned);
            // The cluster sample found the troublesome slot which caused this fork to be pruned
            warn!(
                "EpochSlots sample returned slot {slot} with hash {epoch_slots_frozen_hash}, but \
                 we have pruned it due to incorrect ancestry"
            );
        }
    }
    state_changes.push(ResultingStateChange::RepairDuplicateConfirmedVersion(
        epoch_slots_frozen_hash,
    ));
}

fn on_dead_slot(slot: Slot, dead_state: DeadState) -> Vec<ResultingStateChange> {
    let DeadState {
        cluster_confirmed_hash,
        ..
    } = dead_state;

    let mut state_changes = vec![];
    if let Some(cluster_confirmed_hash) = cluster_confirmed_hash {
        match cluster_confirmed_hash {
            ClusterConfirmedHash::DuplicateConfirmed(duplicate_confirmed_hash) => {
                // If the cluster duplicate_confirmed some version of this slot, then
                // check if our version agrees with the cluster,
                state_changes.push(ResultingStateChange::SendAncestorHashesReplayUpdate(
                    AncestorHashesReplayUpdate::DeadDuplicateConfirmed(slot),
                ));
                check_duplicate_confirmed_hash_against_bank_status(
                    &mut state_changes,
                    slot,
                    duplicate_confirmed_hash,
                    BankStatus::Dead,
                );
            }
            ClusterConfirmedHash::EpochSlotsFrozen(epoch_slots_frozen_hash) => {
                // Lower priority than having seen an actual duplicate confirmed hash in the
                // match arm above.
                let is_popular_pruned = false;
                check_epoch_slots_hash_against_bank_status(
                    &mut state_changes,
                    slot,
                    epoch_slots_frozen_hash,
                    BankStatus::Dead,
                    is_popular_pruned,
                );
            }
        }
    } else {
        state_changes.push(ResultingStateChange::SendAncestorHashesReplayUpdate(
            AncestorHashesReplayUpdate::Dead(slot),
        ));
    }

    state_changes
}

fn on_frozen_slot(slot: Slot, bank_frozen_state: BankFrozenState) -> Vec<ResultingStateChange> {
    let BankFrozenState {
        frozen_hash,
        cluster_confirmed_hash,
        is_slot_duplicate,
    } = bank_frozen_state;
    let mut state_changes = vec![ResultingStateChange::BankFrozen(frozen_hash)];
    if let Some(cluster_confirmed_hash) = cluster_confirmed_hash {
        match cluster_confirmed_hash {
            ClusterConfirmedHash::DuplicateConfirmed(duplicate_confirmed_hash) => {
                // If the cluster duplicate_confirmed some version of this slot, then
                // check if our version agrees with the cluster,
                check_duplicate_confirmed_hash_against_bank_status(
                    &mut state_changes,
                    slot,
                    duplicate_confirmed_hash,
                    BankStatus::Frozen(frozen_hash),
                );
            }
            ClusterConfirmedHash::EpochSlotsFrozen(epoch_slots_frozen_hash) => {
                // Lower priority than having seen an actual duplicate confirmed hash in the
                // match arm above.
                let is_popular_pruned = false;
                check_epoch_slots_hash_against_bank_status(
                    &mut state_changes,
                    slot,
                    epoch_slots_frozen_hash,
                    BankStatus::Frozen(frozen_hash),
                    is_popular_pruned,
                );
            }
        }
    } else if is_slot_duplicate {
        // If `cluster_confirmed_hash` is Some above we should have already pushed a
        // `MarkSlotDuplicate` state change
        state_changes.push(ResultingStateChange::MarkSlotDuplicate(frozen_hash));
    }

    state_changes
}

fn on_duplicate_confirmed(
    slot: Slot,
    duplicate_confirmed_state: DuplicateConfirmedState,
) -> Vec<ResultingStateChange> {
    let DuplicateConfirmedState {
        bank_status,
        duplicate_confirmed_hash,
    } = duplicate_confirmed_state;

    match bank_status {
        BankStatus::Dead | BankStatus::Frozen(_) => (),
        // No action to be taken yet
        BankStatus::Unprocessed => {
            return vec![];
        }
    }

    let mut state_changes = vec![];
    if bank_status.is_dead() {
        state_changes.push(ResultingStateChange::SendAncestorHashesReplayUpdate(
            AncestorHashesReplayUpdate::DeadDuplicateConfirmed(slot),
        ));
    }
    check_duplicate_confirmed_hash_against_bank_status(
        &mut state_changes,
        slot,
        duplicate_confirmed_hash,
        bank_status,
    );

    state_changes
}

fn on_duplicate(duplicate_state: DuplicateState) -> Vec<ResultingStateChange> {
    let DuplicateState {
        bank_status,
        duplicate_confirmed_hash,
    } = duplicate_state;

    match bank_status {
        BankStatus::Dead | BankStatus::Frozen(_) => (),
        // No action to be taken yet
        BankStatus::Unprocessed => {
            return vec![];
        }
    }

    // If the cluster duplicate_confirmed some version of this slot
    // then either the `SlotStateUpdate::DuplicateConfirmed`, `SlotStateUpdate::BankFrozen`,
    // or `SlotStateUpdate::Dead` state transitions will take care of marking the fork as
    // duplicate if there's a mismatch with our local version.
    if duplicate_confirmed_hash.is_none() {
        // If we have not yet seen any version of the slot duplicate confirmed, then mark
        // the slot as duplicate
        if let Some(bank_hash) = bank_status.bank_hash() {
            return vec![ResultingStateChange::MarkSlotDuplicate(bank_hash)];
        }
    }

    vec![]
}

fn on_epoch_slots_frozen(
    slot: Slot,
    epoch_slots_frozen_state: EpochSlotsFrozenState,
) -> Vec<ResultingStateChange> {
    let EpochSlotsFrozenState {
        bank_status,
        epoch_slots_frozen_hash,
        duplicate_confirmed_hash,
        is_popular_pruned,
    } = epoch_slots_frozen_state;

    // If `slot` has already been duplicate confirmed, `epoch_slots_frozen` becomes redundant as
    // one of the following triggers would have already processed `slot`:
    //
    // 1) If the bank was replayed and then duplicate confirmed through turbine/gossip, the
    //    corresponding `SlotStateUpdate::DuplicateConfirmed`
    // 2) If the slot was first duplicate confirmed through gossip and then replayed, the
    //    corresponding `SlotStateUpdate::BankFrozen` or `SlotStateUpdate::Dead`
    //
    // However if `slot` was first duplicate confirmed through gossip and then pruned before
    // we got a chance to replay, there was no trigger that would have processed `slot`.
    // The original `SlotStateUpdate::DuplicateConfirmed` is a no-op when the bank has not been
    // replayed yet, and unlike 2) there is no upcoming `SlotStateUpdate::BankFrozen` or
    // `SlotStateUpdate::Dead`, as `slot` is pruned and will not be replayed.
    //
    // Thus if we have a duplicate confirmation, but `slot` is pruned, we continue
    // processing it as `epoch_slots_frozen`.
    if !is_popular_pruned && let Some(duplicate_confirmed_hash) = duplicate_confirmed_hash {
        if epoch_slots_frozen_hash != duplicate_confirmed_hash {
            warn!(
                "EpochSlots sample returned slot {slot} with hash {epoch_slots_frozen_hash}, but \
                 we already saw duplicate confirmation on hash: {duplicate_confirmed_hash:?}",
            );
        }
        return vec![];
    }

    match bank_status {
        BankStatus::Dead | BankStatus::Frozen(_) => (),
        // No action to be taken yet unless `slot` is pruned in which case it will never be played
        BankStatus::Unprocessed => {
            if !is_popular_pruned {
                return vec![];
            }
        }
    }

    let mut state_changes = vec![];
    check_epoch_slots_hash_against_bank_status(
        &mut state_changes,
        slot,
        epoch_slots_frozen_hash,
        bank_status,
        is_popular_pruned,
    );

    state_changes
}

fn on_popular_pruned_fork(slot: Slot) -> Vec<ResultingStateChange> {
    warn!(
        "{slot} is part of a pruned fork which has reached the DUPLICATE_THRESHOLD aggregating \
         across descendants and slot versions. It is suspected to be duplicate or have an \
         ancestor that is duplicate. Notifying ancestor_hashes_service"
    );
    vec![ResultingStateChange::SendAncestorHashesReplayUpdate(
        AncestorHashesReplayUpdate::PopularPrunedFork(slot),
    )]
}

/// Finds the cluster confirmed hash
///
/// 1) If we have a frozen hash, check if it's been duplicate confirmed by cluster
///    in turbine or gossip
/// 2) Otherwise poll `epoch_slots_frozen_slots` to see if we have a hash
///
/// Note `epoch_slots_frozen_slots` is not populated from `EpochSlots` in gossip but actually
/// aggregated through hashes sent in response to requests from `ancestor_hashes_service`
fn get_cluster_confirmed_hash_from_state(
    slot: Slot,
    duplicate_confirmed_slots: &DuplicateConfirmedSlots,
    epoch_slots_frozen_slots: &EpochSlotsFrozenSlots,
    fork_choice: &HeaviestSubtreeForkChoice,
    bank_frozen_hash: Option<Hash>,
) -> Option<ClusterConfirmedHash> {
    let duplicate_confirmed_hash = duplicate_confirmed_slots.get(&slot).cloned();
    // If the bank hasn't been frozen yet, then we haven't duplicate confirmed a local version
    // this slot through replay yet.
    let is_local_replay_duplicate_confirmed = if let Some(bank_frozen_hash) = bank_frozen_hash {
        fork_choice
            .is_duplicate_confirmed(&(slot, bank_frozen_hash))
            .unwrap_or(false)
    } else {
        false
    };

    get_duplicate_confirmed_hash(
        slot,
        duplicate_confirmed_hash,
        bank_frozen_hash,
        is_local_replay_duplicate_confirmed,
    )
    .map(ClusterConfirmedHash::DuplicateConfirmed)
    .or_else(|| {
        epoch_slots_frozen_slots
            .get(&slot)
            .map(|hash| ClusterConfirmedHash::EpochSlotsFrozen(*hash))
    })
}

fn get_duplicate_confirmed_hash_from_state(
    slot: Slot,
    duplicate_confirmed_slots: &DuplicateConfirmedSlots,
    fork_choice: &HeaviestSubtreeForkChoice,
    bank_frozen_hash: Option<Hash>,
) -> Option<Hash> {
    let duplicate_confirmed_hash = duplicate_confirmed_slots.get(&slot).cloned();
    // If the bank hasn't been frozen yet, then we haven't duplicate confirmed a local version
    // this slot through replay yet.
    let is_local_replay_duplicate_confirmed = if let Some(bank_frozen_hash) = bank_frozen_hash {
        fork_choice
            .is_duplicate_confirmed(&(slot, bank_frozen_hash))
            .unwrap_or(false)
    } else {
        false
    };

    get_duplicate_confirmed_hash(
        slot,
        duplicate_confirmed_hash,
        bank_frozen_hash,
        is_local_replay_duplicate_confirmed,
    )
}

/// Finds the duplicate confirmed hash for a slot.
///
/// 1) If `is_local_replay_duplicate_confirmed`, return Some(local frozen hash)
/// 2) If we have a `duplicate_confirmed_hash`, return Some(duplicate_confirmed_hash)
/// 3) Else return None
///
/// Assumes that if `is_local_replay_duplicate_confirmed`, `bank_frozen_hash` is not None
fn get_duplicate_confirmed_hash(
    slot: Slot,
    duplicate_confirmed_hash: Option<Hash>,
    bank_frozen_hash: Option<Hash>,
    is_local_replay_duplicate_confirmed: bool,
) -> Option<Hash> {
    let local_duplicate_confirmed_hash = if is_local_replay_duplicate_confirmed {
        // If local replay has duplicate_confirmed this slot, this slot must have
        // descendants with votes for this slot, hence this slot must be
        // frozen.
        let bank_frozen_hash = bank_frozen_hash.unwrap();
        Some(bank_frozen_hash)
    } else {
        None
    };

    match (local_duplicate_confirmed_hash, duplicate_confirmed_hash) {
        (Some(local_duplicate_confirmed_hash), Some(duplicate_confirmed_hash)) => {
            if local_duplicate_confirmed_hash != duplicate_confirmed_hash {
                error!(
                    "For slot {slot}, the gossip duplicate confirmed hash: \
                     {duplicate_confirmed_hash} is not equal to the confirmed hash we replayed: \
                     {local_duplicate_confirmed_hash}",
                );
            }
            Some(local_duplicate_confirmed_hash)
        }
        (Some(bank_frozen_hash), None) => Some(bank_frozen_hash),
        _ => duplicate_confirmed_hash,
    }
}

fn apply_state_changes(
    slot: Slot,
    fork_choice: &mut HeaviestSubtreeForkChoice,
    duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
    blockstore: &Blockstore,
    ancestor_hashes_replay_update_sender: &AncestorHashesReplayUpdateSender,
    purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
    state_changes: Vec<ResultingStateChange>,
) {
    // Handle cases where the bank is frozen, but not duplicate confirmed
    // yet.
    let mut not_duplicate_confirmed_frozen_hash = None;
    for state_change in state_changes {
        match state_change {
            ResultingStateChange::BankFrozen(bank_frozen_hash) => {
                if !fork_choice
                    .is_duplicate_confirmed(&(slot, bank_frozen_hash))
                    .expect("frozen bank must exist in fork choice")
                {
                    not_duplicate_confirmed_frozen_hash = Some(bank_frozen_hash);
                }
            }
            ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash) => {
                fork_choice.mark_fork_invalid_candidate(&(slot, bank_frozen_hash));
            }
            ResultingStateChange::RepairDuplicateConfirmedVersion(duplicate_confirmed_hash) => {
                duplicate_slots_to_repair.insert(slot, duplicate_confirmed_hash);
            }
            ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(bank_frozen_hash) => {
                not_duplicate_confirmed_frozen_hash = None;
                // When we detect that our frozen slot matches the cluster version (note this
                // will catch both bank frozen first -> confirmation, or confirmation first ->
                // bank frozen), mark all the newly duplicate confirmed slots in blockstore
                let new_duplicate_confirmed_slot_hashes =
                    fork_choice.mark_fork_valid_candidate(&(slot, bank_frozen_hash));
                blockstore
                    .set_duplicate_confirmed_slots_and_hashes(
                        new_duplicate_confirmed_slot_hashes.into_iter(),
                    )
                    .unwrap();
                duplicate_slots_to_repair.remove(&slot);
                purge_repair_slot_counter.remove(&slot);
            }
            ResultingStateChange::SendAncestorHashesReplayUpdate(ancestor_hashes_replay_update) => {
                let _ = ancestor_hashes_replay_update_sender.send(ancestor_hashes_replay_update);
            }
        }
    }

    if let Some(frozen_hash) = not_duplicate_confirmed_frozen_hash {
        blockstore.insert_bank_hash(slot, frozen_hash, false);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_slot_agrees_with_cluster(
    slot: Slot,
    root: Slot,
    blockstore: &Blockstore,
    duplicate_slots_tracker: &mut DuplicateSlotsTracker,
    epoch_slots_frozen_slots: &mut EpochSlotsFrozenSlots,
    fork_choice: &mut HeaviestSubtreeForkChoice,
    duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
    ancestor_hashes_replay_update_sender: &AncestorHashesReplayUpdateSender,
    purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
    slot_state_update: SlotStateUpdate,
) {
    info!(
        "check_slot_agrees_with_cluster() slot: {slot}, root: {root}, slot_state_update: \
         {slot_state_update:?}"
    );

    if slot <= root {
        return;
    }

    // Needs to happen before the bank_frozen_hash.is_none() check below to account for duplicate
    // signals arriving before the bank is constructed in replay.
    if let SlotStateUpdate::Duplicate(ref state) = slot_state_update {
        // If this slot has already been processed before, return
        if !duplicate_slots_tracker.insert(slot) {
            return;
        }

        datapoint_info!(
            "duplicate_slot",
            ("slot", slot, i64),
            (
                "duplicate_confirmed_hash",
                state
                    .duplicate_confirmed_hash
                    .unwrap_or_default()
                    .to_string(),
                String
            ),
            (
                "my_hash",
                state
                    .bank_status
                    .bank_hash()
                    .unwrap_or_default()
                    .to_string(),
                String
            ),
        );
    }

    // Avoid duplicate work from multiple of the same DuplicateConfirmed signal. This can
    // happen if we get duplicate confirmed from gossip and from local replay.
    if let SlotStateUpdate::DuplicateConfirmed(state) = &slot_state_update {
        if let Some(bank_hash) = state.bank_status.bank_hash()
            && let Some(true) = fork_choice.is_duplicate_confirmed(&(slot, bank_hash))
        {
            return;
        }

        datapoint_info!(
            "duplicate_confirmed_slot",
            ("slot", slot, i64),
            (
                "duplicate_confirmed_hash",
                state.duplicate_confirmed_hash.to_string(),
                String
            ),
            (
                "my_hash",
                state
                    .bank_status
                    .bank_hash()
                    .unwrap_or_default()
                    .to_string(),
                String
            ),
        );
    }

    if let SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state) = &slot_state_update
        && let Some(old_epoch_slots_frozen_hash) =
            epoch_slots_frozen_slots.insert(slot, epoch_slots_frozen_state.epoch_slots_frozen_hash)
        && old_epoch_slots_frozen_hash == epoch_slots_frozen_state.epoch_slots_frozen_hash
    {
        // If EpochSlots has already told us this same hash was frozen, return
        return;
    }

    let state_changes = slot_state_update.into_state_changes(slot);
    apply_state_changes(
        slot,
        fork_choice,
        duplicate_slots_to_repair,
        blockstore,
        ancestor_hashes_replay_update_sender,
        purge_repair_slot_counter,
        state_changes,
    );
}

#[cfg(test)]
mod test {
    use super::*;

    macro_rules! state_update_tests {
        ($($name:ident: $value:expr,)*) => {
        $(
            #[test]
            fn $name() {
                let slot = 10;
                let (state_update, expected) = $value;
                assert_eq!(expected, state_update.into_state_changes(slot));
            }
        )*
        }
    }

    state_update_tests! {
        bank_frozen_state_update_0: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = None;
            let is_slot_duplicate = false;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash)]
            )
        },
        bank_frozen_state_update_1: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = None;
            let is_slot_duplicate = true;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash), ResultingStateChange::MarkSlotDuplicate(frozen_hash)]
            )
        },
        bank_frozen_state_update_2: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::DuplicateConfirmed(frozen_hash));
            let is_slot_duplicate = false;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash),
                ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(frozen_hash)]
            )
        },
        bank_frozen_state_update_3: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::EpochSlotsFrozen(frozen_hash));
            let is_slot_duplicate = false;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash)]
            )
        },
        bank_frozen_state_update_4: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::DuplicateConfirmed(frozen_hash));
            let is_slot_duplicate = true;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash),
                ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(frozen_hash)]
            )
        },
        bank_frozen_state_update_5: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::EpochSlotsFrozen(frozen_hash));
            let is_slot_duplicate = true;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash)],
            )
        },
        bank_frozen_state_update_6: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::DuplicateConfirmed(duplicate_confirmed_hash));
            let is_slot_duplicate = false;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash),
                ResultingStateChange::MarkSlotDuplicate(frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(duplicate_confirmed_hash)],
            )
        },
        bank_frozen_state_update_7: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let epoch_slots_frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::EpochSlotsFrozen(epoch_slots_frozen_hash));
            let is_slot_duplicate = false;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash),
                ResultingStateChange::MarkSlotDuplicate(frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        bank_frozen_state_update_8: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::DuplicateConfirmed(duplicate_confirmed_hash));
            let is_slot_duplicate = true;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash),
                ResultingStateChange::MarkSlotDuplicate(frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(duplicate_confirmed_hash)],
            )
        },
        bank_frozen_state_update_9: {
            // frozen hash has to be non-default for frozen state transition
            let frozen_hash = Hash::new_unique();
            let epoch_slots_frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::EpochSlotsFrozen(epoch_slots_frozen_hash));
            let is_slot_duplicate = true;
            let bank_frozen_state = BankFrozenState::new(
                frozen_hash,
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::BankFrozen(bank_frozen_state),
                vec![ResultingStateChange::BankFrozen(frozen_hash),
                ResultingStateChange::MarkSlotDuplicate(frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        duplicate_confirmed_state_update_0: {
            let duplicate_confirmed_hash = Hash::new_unique();
            let bank_status = BankStatus::Unprocessed;
            let duplicate_confirmed_state = DuplicateConfirmedState::new(
                duplicate_confirmed_hash,
                bank_status,
            );
            (
                SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        duplicate_confirmed_state_update_1: {
            let duplicate_confirmed_hash = Hash::new_unique();
            let bank_status = BankStatus::Dead;
            let duplicate_confirmed_state = DuplicateConfirmedState::new(
                duplicate_confirmed_hash,
                bank_status,
            );
            (
                SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
                vec![
                ResultingStateChange::SendAncestorHashesReplayUpdate(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(10)),
                ResultingStateChange::RepairDuplicateConfirmedVersion(duplicate_confirmed_hash)],
            )
        },
        duplicate_confirmed_state_update_2: {
            let duplicate_confirmed_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(duplicate_confirmed_hash);
            let duplicate_confirmed_state = DuplicateConfirmedState::new(
                duplicate_confirmed_hash,
                bank_status,
            );
            (
                SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
                vec![
                ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(duplicate_confirmed_hash)]
            )
        },
        duplicate_confirmed_state_update_3: {
            let duplicate_confirmed_hash = Hash::new_unique();
            let frozen_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(frozen_hash);
            let duplicate_confirmed_state = DuplicateConfirmedState::new(
                duplicate_confirmed_hash,
                bank_status,
            );
            (
                SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
                vec![
                ResultingStateChange::MarkSlotDuplicate(frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(duplicate_confirmed_hash)],
            )
        },
        dead_state_update_0: {
            let cluster_confirmed_hash = None;
            let is_slot_duplicate = false;
            let dead_state = DeadState::new(
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::Dead(dead_state),
                vec![
                    ResultingStateChange::SendAncestorHashesReplayUpdate(AncestorHashesReplayUpdate::Dead(10))
                ],
            )
        },
        dead_state_update_1: {
            let cluster_confirmed_hash = None;
            let is_slot_duplicate = true;
            let dead_state = DeadState::new(
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::Dead(dead_state),
                vec![
                    ResultingStateChange::SendAncestorHashesReplayUpdate(AncestorHashesReplayUpdate::Dead(10))
                ],
            )
        },
        dead_state_update_2: {
            let duplicate_confirmed_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::DuplicateConfirmed(duplicate_confirmed_hash));
            let is_slot_duplicate = false;
            let dead_state = DeadState::new(
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::Dead(dead_state),
                vec![
                ResultingStateChange::SendAncestorHashesReplayUpdate(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(10)),
                ResultingStateChange::RepairDuplicateConfirmedVersion(duplicate_confirmed_hash)],
            )
        },
        dead_state_update_3: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::EpochSlotsFrozen(epoch_slots_frozen_hash));
            let is_slot_duplicate = false;
            let dead_state = DeadState::new(
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::Dead(dead_state),
                vec![
                ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        dead_state_update_4: {
            let duplicate_confirmed_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::DuplicateConfirmed(duplicate_confirmed_hash));
            let is_slot_duplicate = true;
            let dead_state = DeadState::new(
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::Dead(dead_state),
                vec![

                    ResultingStateChange::SendAncestorHashesReplayUpdate(AncestorHashesReplayUpdate::DeadDuplicateConfirmed(10)),
                ResultingStateChange::RepairDuplicateConfirmedVersion(duplicate_confirmed_hash)],
            )
        },
        dead_state_update_5: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let cluster_confirmed_hash = Some(ClusterConfirmedHash::EpochSlotsFrozen(epoch_slots_frozen_hash));
            let is_slot_duplicate = true;
            let dead_state = DeadState::new(
                cluster_confirmed_hash,
                is_slot_duplicate,
            );
            (
                SlotStateUpdate::Dead(dead_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        duplicate_state_update_0: {
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Unprocessed;
            let duplicate_state = DuplicateState::new(duplicate_confirmed_hash, bank_status);
            (
                SlotStateUpdate::Duplicate(duplicate_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        duplicate_state_update_1: {
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Dead;
            let duplicate_state = DuplicateState::new(duplicate_confirmed_hash, bank_status);
            (
                SlotStateUpdate::Duplicate(duplicate_state),
                Vec::<ResultingStateChange>::new(),
            )
        },
        duplicate_state_update_2: {
            let duplicate_confirmed_hash = None;
            let bank_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(bank_hash);
            let duplicate_state = DuplicateState::new(duplicate_confirmed_hash, bank_status);
            (
                SlotStateUpdate::Duplicate(duplicate_state),
                vec![ResultingStateChange::MarkSlotDuplicate(bank_hash)],
            )
        },
        duplicate_state_update_3: {
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Unprocessed;
            let duplicate_state = DuplicateState::new(duplicate_confirmed_hash, bank_status);
            (
                SlotStateUpdate::Duplicate(duplicate_state),
                Vec::<ResultingStateChange>::new(),
            )
        },
        duplicate_state_update_4: {
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Dead;
            let duplicate_state = DuplicateState::new(duplicate_confirmed_hash, bank_status);
            (
                SlotStateUpdate::Duplicate(duplicate_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        duplicate_state_update_5: {
            let duplicate_confirmed_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(duplicate_confirmed_hash);
            let duplicate_state = DuplicateState::new(Some(duplicate_confirmed_hash), bank_status);
            (
                SlotStateUpdate::Duplicate(duplicate_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        duplicate_state_update_6: {
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let frozen_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(frozen_hash);
            let duplicate_state = DuplicateState::new(duplicate_confirmed_hash, bank_status);
            (
                SlotStateUpdate::Duplicate(duplicate_state),
                Vec::<ResultingStateChange>::new(),
            )
        },
        epoch_slots_frozen_state_update_0: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Unprocessed;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_1: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Unprocessed;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_2: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(epoch_slots_frozen_hash);
            let bank_status = BankStatus::Unprocessed;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_3: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Dead;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_4: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Dead;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_5: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(epoch_slots_frozen_hash);
            let bank_status = BankStatus::Dead;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_6: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let frozen_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(frozen_hash);
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![
                ResultingStateChange::MarkSlotDuplicate(frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_7: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Frozen(epoch_slots_frozen_hash);
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_8: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Frozen(Hash::new_unique());
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_9: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Frozen(epoch_slots_frozen_hash);
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_10: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(duplicate_confirmed_hash);
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, Some(duplicate_confirmed_hash), bank_status, false);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_11: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Unprocessed;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_12: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Unprocessed;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_13: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(epoch_slots_frozen_hash);
            let bank_status = BankStatus::Unprocessed;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_14: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Dead;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_15: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Dead;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_16: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(epoch_slots_frozen_hash);
            let bank_status = BankStatus::Dead;
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_17: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let frozen_hash = Hash::new_unique();
            let bank_status = BankStatus::Frozen(frozen_hash);
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_18: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = None;
            let bank_status = BankStatus::Frozen(epoch_slots_frozen_hash);
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        epoch_slots_frozen_state_update_19: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Frozen(Hash::new_unique());
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                vec![ResultingStateChange::RepairDuplicateConfirmedVersion(epoch_slots_frozen_hash)],
            )
        },
        epoch_slots_frozen_state_update_20: {
            let epoch_slots_frozen_hash = Hash::new_unique();
            let duplicate_confirmed_hash = Some(Hash::new_unique());
            let bank_status = BankStatus::Frozen(epoch_slots_frozen_hash);
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new(epoch_slots_frozen_hash, duplicate_confirmed_hash, bank_status, true);
            (
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
                Vec::<ResultingStateChange>::new()
            )
        },
        popular_pruned_fork: {
            (
                SlotStateUpdate::PopularPrunedFork,
                vec![ResultingStateChange::SendAncestorHashesReplayUpdate(
                    AncestorHashesReplayUpdate::PopularPrunedFork(10),
                )]
            )
        },
    }
}
