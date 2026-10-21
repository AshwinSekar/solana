use {
    crate::cluster_info_vote_listener::VoteTracker,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::blockstore::Blockstore,
    solana_runtime::bank::Bank,
    solana_time_utils::timestamp,
    std::{collections::BTreeSet, time::Instant},
};

pub struct OptimisticConfirmationVerifier {
    snapshot_start_slot: Slot,
    unchecked_slots: BTreeSet<(Slot, Hash)>,
    last_optimistic_slot_ts: Instant,
}

impl OptimisticConfirmationVerifier {
    pub fn new(snapshot_start_slot: Slot) -> Self {
        Self {
            snapshot_start_slot,
            unchecked_slots: BTreeSet::default(),
            last_optimistic_slot_ts: Instant::now(),
        }
    }

    // Returns any optimistic slots that were not rooted
    pub fn verify_for_unrooted_optimistic_slots(
        &mut self,
        root_bank: &Bank,
        blockstore: &Blockstore,
    ) -> Vec<(Slot, Hash)> {
        let root = root_bank.slot();
        let root_ancestors = &root_bank.ancestors;
        let slots_after_root = self
            .unchecked_slots
            .split_off(&((root + 1), Hash::default()));
        // `slots_before_root` now contains all slots <= root
        let slots_before_root = std::mem::replace(&mut self.unchecked_slots, slots_after_root);
        slots_before_root
            .into_iter()
            .filter(|(optimistic_slot, optimistic_hash)| {
                (*optimistic_slot == root && *optimistic_hash != root_bank.hash())
                    || (!root_ancestors.contains_key(optimistic_slot) &&
                    // In this second part of the `and`, we account for the possibility that
                    // there was some other root `rootX` set in BankForks where:
                    //
                    // `root` > `rootX` > `optimistic_slot`
                    //
                    // in which case `root` may  not contain the ancestor information for
                    // slots < `rootX`, so we also have to check if `optimistic_slot` was rooted
                    // through blockstore.
                    !blockstore.is_root(*optimistic_slot))
            })
            .collect()
    }

    pub fn add_new_optimistic_confirmed_slots(
        &mut self,
        new_optimistic_slots: Vec<(Slot, Hash)>,
        blockstore: &Blockstore,
    ) {
        if new_optimistic_slots.is_empty() {
            return;
        }

        // We don't have any information about ancestors before the snapshot root,
        // so ignore those slots
        for (new_optimistic_slot, hash) in new_optimistic_slots {
            if new_optimistic_slot > self.snapshot_start_slot {
                if let Err(e) = blockstore.insert_optimistic_slot(
                    new_optimistic_slot,
                    &hash,
                    timestamp().try_into().unwrap(),
                ) {
                    error!(
                        "failed to record optimistic slot in blockstore: \
                         slot={new_optimistic_slot}: {e:?}"
                    );
                }
                datapoint_info!("optimistic_slot", ("slot", new_optimistic_slot, i64),);
                self.unchecked_slots.insert((new_optimistic_slot, hash));
            }
        }

        self.last_optimistic_slot_ts = Instant::now();
    }

    pub fn format_optimistic_confirmed_slot_violation_log(slot: Slot) -> String {
        format!("Optimistically confirmed slot {slot} was not rooted")
    }

    pub fn log_unrooted_optimistic_slots(
        root_bank: &Bank,
        vote_tracker: &VoteTracker,
        unrooted_optimistic_slots: &[(Slot, Hash)],
    ) {
        let root = root_bank.slot();
        for (optimistic_slot, hash) in unrooted_optimistic_slots.iter() {
            let epoch = root_bank.epoch_schedule().get_epoch(*optimistic_slot);
            let epoch_stakes = root_bank.epoch_stakes(epoch);
            let total_epoch_stake = epoch_stakes.map(|e| e.total_stake()).unwrap_or(0);
            let voted_stake = {
                let slot_tracker = vote_tracker.get_slot_vote_tracker(*optimistic_slot);
                let r_slot_tracker = slot_tracker.as_ref().map(|s| s.read().unwrap());
                let voted_stake = r_slot_tracker
                    .as_ref()
                    .and_then(|s| s.optimistic_votes_tracker(hash))
                    .map(|s| s.stake())
                    .unwrap_or(0);

                error!(
                    "{}, hash: {hash}, epoch: {epoch}, voted keys: {:?}, root: {root}, root bank \
                     hash: {}, voted stake: {voted_stake}, total epoch stake: \
                     {total_epoch_stake}, pct: {}",
                    Self::format_optimistic_confirmed_slot_violation_log(*optimistic_slot),
                    r_slot_tracker
                        .as_ref()
                        .and_then(|s| s.optimistic_votes_tracker(hash))
                        .map(|s| s.voted()),
                    root_bank.hash(),
                    voted_stake as f64 / total_epoch_stake as f64,
                );
                voted_stake
            };

            datapoint_warn!(
                "optimistic_slot_not_rooted",
                ("slot", *optimistic_slot, i64),
                ("epoch", epoch, i64),
                ("root", root, i64),
                ("voted_stake", voted_stake, i64),
                ("total_epoch_stake", total_epoch_stake, i64),
            );
        }
    }
}

#[cfg(test)]
mod test {
    use {super::*, solana_ledger::get_tmp_ledger_path_auto_delete};

    #[test]
    fn test_add_new_optimistic_confirmed_slots() {
        let snapshot_start_slot = 10;
        let bank_hash = Hash::default();
        let mut optimistic_confirmation_verifier =
            OptimisticConfirmationVerifier::new(snapshot_start_slot);
        let blockstore_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(blockstore_path.path()).unwrap();
        optimistic_confirmation_verifier.add_new_optimistic_confirmed_slots(
            vec![(snapshot_start_slot - 1, bank_hash)],
            &blockstore,
        );
        assert_eq!(blockstore.get_latest_optimistic_slots(10).unwrap().len(), 0);
        optimistic_confirmation_verifier.add_new_optimistic_confirmed_slots(
            vec![(snapshot_start_slot, bank_hash)],
            &blockstore,
        );
        assert_eq!(blockstore.get_latest_optimistic_slots(10).unwrap().len(), 0);
        optimistic_confirmation_verifier.add_new_optimistic_confirmed_slots(
            vec![(snapshot_start_slot + 1, bank_hash)],
            &blockstore,
        );
        assert_eq!(blockstore.get_latest_optimistic_slots(10).unwrap().len(), 1);
        assert_eq!(optimistic_confirmation_verifier.unchecked_slots.len(), 1);
        assert!(
            optimistic_confirmation_verifier
                .unchecked_slots
                .contains(&(snapshot_start_slot + 1, bank_hash))
        );
    }
}
