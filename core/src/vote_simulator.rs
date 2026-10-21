#![cfg(feature = "dev-context-only-utils")]
use {
    crate::consensus::{
        ComputedBankState, Tower,
        fork_choice::{
            ForkChoice, HeaviestForkFailures, SelectVoteAndResetForkResult,
            select_vote_and_reset_forks,
        },
        heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
        latest_validator_votes_for_frozen_banks::LatestValidatorVotesForFrozenBanks,
        progress_map::{ForkProgress, LockoutInterval, ProgressMap},
        tower_vote_state::TowerVoteState,
    },
    agave_votor_messages::consensus_message::BlockId,
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_leader_schedule::SlotLeader,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, BankTestConfig},
        bank_forks::BankForks,
        genesis_utils::{
            GenesisConfigInfo, ValidatorVoteKeypairs,
            create_genesis_config_with_tower_vote_accounts,
        },
    },
    solana_signer::Signer,
    solana_vote::vote_transaction,
    solana_vote_program::vote_state::{Lockout, MAX_LOCKOUT_HISTORY, TowerSync},
    std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{Arc, RwLock},
    },
    trees::{Tree, TreeWalk, tr},
};
#[cfg(test)]
use {
    solana_accounts_db::accounts_db::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig},
    solana_ledger::{blockstore::Blockstore, get_tmp_ledger_path},
};

/// Tower fork-choice state retained by the consensus test fixture.
///
/// The production replay stage no longer owns Tower state, but the repair and
/// consensus tests built on `VoteSimulator` still exercise the legacy fork
/// choice directly.
pub struct VoteSimulatorForkChoice {
    pub heaviest_subtree_fork_choice: HeaviestSubtreeForkChoice,
}

pub struct VoteSimulator {
    pub validator_keypairs: HashMap<Pubkey, ValidatorVoteKeypairs>,
    pub node_pubkeys: Vec<Pubkey>,
    pub vote_pubkeys: Vec<Pubkey>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub progress: ProgressMap,
    pub latest_validator_votes_for_frozen_banks: LatestValidatorVotesForFrozenBanks,
    pub tbft_structs: VoteSimulatorForkChoice,
}

#[cfg(test)]
pub(crate) type GenerateVotes = Box<dyn Fn(Vec<Pubkey>) -> HashMap<Pubkey, Vec<Slot>>>;

/// Tower-based bank-forks fixture retained for consensus and repair tests.
#[cfg(test)]
pub(crate) fn setup_forks_from_tree(
    tree: Tree<Slot>,
    num_keys: usize,
    generate_votes: Option<GenerateVotes>,
) -> (VoteSimulator, Blockstore) {
    let ledger_path = get_tmp_ledger_path!();
    let mut vote_simulator = VoteSimulator::new_with(
        num_keys,
        BankTestConfig {
            accounts_db_config: AccountsDbConfig {
                bank_hash_details_dir: ledger_path.clone(),
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
        },
    );
    let pubkeys = vote_simulator
        .validator_keypairs
        .values()
        .map(|keypairs| keypairs.node_keypair.pubkey())
        .collect();
    let cluster_votes = generate_votes
        .map(|generate_votes| generate_votes(pubkeys))
        .unwrap_or_default();
    vote_simulator.fill_bank_forks(tree.clone(), &cluster_votes, true);
    let blockstore = Blockstore::open(&ledger_path).unwrap();
    blockstore.add_tree(tree, false, true, 2, Hash::default());
    (vote_simulator, blockstore)
}

/// Shared components used by repair tests that still exercise Tower fork choice.
#[cfg(test)]
pub(crate) struct ReplayBlockstoreComponents {
    pub(crate) blockstore: Arc<Blockstore>,
    pub(crate) vote_simulator: VoteSimulator,
}

#[cfg(test)]
pub(crate) fn replay_blockstore_components(
    forks: Option<Tree<Slot>>,
    num_validators: usize,
    generate_votes: Option<GenerateVotes>,
) -> ReplayBlockstoreComponents {
    let (vote_simulator, blockstore) = setup_forks_from_tree(
        forks.unwrap_or_else(|| tr(0)),
        num_validators,
        generate_votes,
    );
    ReplayBlockstoreComponents {
        blockstore: Arc::new(blockstore),
        vote_simulator,
    }
}

impl VoteSimulator {
    pub fn new(num_keypairs: usize) -> Self {
        Self::new_with(num_keypairs, BankTestConfig::default())
    }

    pub fn new_with(num_keypairs: usize, bank_test_config: BankTestConfig) -> Self {
        let (
            validator_keypairs,
            node_pubkeys,
            vote_pubkeys,
            bank_forks,
            progress,
            heaviest_subtree_fork_choice,
        ) = Self::init_state(num_keypairs, bank_test_config);
        Self {
            validator_keypairs,
            node_pubkeys,
            vote_pubkeys,
            bank_forks,
            progress,
            latest_validator_votes_for_frozen_banks: LatestValidatorVotesForFrozenBanks::default(),
            tbft_structs: VoteSimulatorForkChoice {
                heaviest_subtree_fork_choice,
            },
        }
    }

    pub fn fill_bank_forks(
        &mut self,
        forks: Tree<u64>,
        cluster_votes: &HashMap<Pubkey, Vec<u64>>,
        is_frozen: bool,
    ) {
        let root = *forks.root().data();
        assert!(self.bank_forks.read().unwrap().get(root).is_some());

        let mut walk = TreeWalk::from(forks);

        while let Some(visit) = walk.get() {
            let slot = *visit.node().data();
            if self.bank_forks.read().unwrap().get(slot).is_some() {
                walk.forward();
                continue;
            }
            let parent = *walk.get_parent().unwrap().data();
            let parent_bank = self.bank_forks.read().unwrap().get(parent).unwrap();
            let new_bank = Bank::new_from_parent(parent_bank.clone(), SlotLeader::default(), slot);
            let new_bank = self
                .bank_forks
                .write()
                .unwrap()
                .insert(new_bank)
                .clone_without_scheduler();
            self.progress
                .entry(slot)
                .or_insert_with(|| ForkProgress::new(Hash::default(), None, None, 0, 0, None));
            for (pubkey, vote) in cluster_votes.iter() {
                if vote.contains(&parent) {
                    let keypairs = self.validator_keypairs.get(pubkey).unwrap();
                    let latest_blockhash = parent_bank.last_blockhash();
                    let tower_sync = if let Some(vote_account) =
                        parent_bank.get_vote_account(&keypairs.vote_keypair.pubkey())
                    {
                        let mut vote_state = TowerVoteState::from(vote_account.vote_state_view());
                        vote_state.process_next_vote_slot(parent);
                        TowerSync::new(
                            vote_state.votes,
                            vote_state.root_slot,
                            parent_bank.hash(),
                            Hash::default(),
                        )
                    } else {
                        TowerSync::new(
                            VecDeque::from([Lockout::new(parent)]),
                            Some(root),
                            parent_bank.hash(),
                            Hash::default(),
                        )
                    };
                    let vote_tx = vote_transaction::new_tower_sync_transaction(
                        tower_sync,
                        latest_blockhash,
                        &keypairs.node_keypair,
                        &keypairs.vote_keypair,
                        &keypairs.vote_keypair,
                        None,
                    );
                    info!("voting {} {}", parent_bank.slot(), parent_bank.hash());
                    new_bank.process_transaction(&vote_tx).unwrap();

                    // Check the vote landed
                    let vote_account = new_bank
                        .get_vote_account(&keypairs.vote_keypair.pubkey())
                        .unwrap();
                    let vote_state_view = vote_account.vote_state_view();
                    assert!(
                        vote_state_view
                            .votes_iter()
                            .any(|lockout| lockout.slot() == parent)
                    );
                }
            }

            new_bank.fill_bank_with_ticks_for_tests();
            if !visit.node().has_no_child() || is_frozen {
                new_bank.set_block_id(Some(BlockId::new_unique()));
                new_bank.freeze();
                self.progress
                    .get_fork_stats_mut(new_bank.slot())
                    .expect("All frozen banks must exist in the Progress map")
                    .bank_hash = Some(new_bank.hash());
                self.tbft_structs
                    .heaviest_subtree_fork_choice
                    .add_new_leaf_slot(
                        (new_bank.slot(), new_bank.hash()),
                        Some((new_bank.parent_slot(), new_bank.parent_hash())),
                    );
            }

            walk.forward();
        }
    }

    pub fn simulate_vote(
        &mut self,
        vote_slot: Slot,
        my_pubkey: &Pubkey,
        tower: &mut Tower,
    ) -> Vec<HeaviestForkFailures> {
        // Try to simulate the vote
        let ancestors = self.bank_forks.read().unwrap().ancestors();
        let mut frozen_banks: Vec<_> = self
            .bank_forks
            .read()
            .unwrap()
            .frozen_banks()
            .map(|(_slot, bank)| bank)
            .collect();
        self.compute_bank_stats(my_pubkey, &ancestors, &mut frozen_banks, tower);

        let vote_bank = self
            .bank_forks
            .read()
            .unwrap()
            .get(vote_slot)
            .expect("Bank must have been created before vote simulation");

        // Try to vote on the given slot
        let descendants = self.bank_forks.read().unwrap().descendants();
        let SelectVoteAndResetForkResult {
            heaviest_fork_failures,
            ..
        } = select_vote_and_reset_forks(
            &vote_bank,
            None,
            &ancestors,
            &descendants,
            &self.progress,
            tower,
            &self.latest_validator_votes_for_frozen_banks,
            &self.tbft_structs.heaviest_subtree_fork_choice,
        );

        // Make sure this slot isn't locked out or failing threshold
        info!("Checking vote: {}", vote_bank.slot());
        if !heaviest_fork_failures.is_empty() {
            return heaviest_fork_failures;
        }

        let new_root = tower.record_bank_vote(&vote_bank);
        if let Some(new_root) = new_root {
            self.set_root(my_pubkey, new_root);
        }

        vec![]
    }

    fn compute_bank_stats(
        &mut self,
        my_vote_pubkey: &Pubkey,
        ancestors: &HashMap<Slot, HashSet<Slot>>,
        frozen_banks: &mut [Arc<Bank>],
        tower: &mut Tower,
    ) {
        frozen_banks.sort_by_key(|bank| bank.slot());
        let mut vote_slots = HashSet::default();
        for bank in frozen_banks.iter() {
            let bank_slot = bank.slot();
            let is_computed = self
                .progress
                .get_fork_stats_mut(bank_slot)
                .expect("All frozen banks must exist in the Progress map")
                .computed;
            if !is_computed {
                Self::adopt_on_chain_tower_if_behind(
                    my_vote_pubkey,
                    ancestors,
                    frozen_banks,
                    tower,
                    &mut self.progress,
                    bank,
                    &self.bank_forks,
                );

                let root_slot = self.bank_forks.read().unwrap().root();
                let computed_bank_state = Tower::collect_vote_lockouts(
                    my_vote_pubkey,
                    bank_slot,
                    root_slot,
                    &bank.vote_accounts(),
                    ancestors,
                    |slot| self.progress.get_hash(slot),
                    &mut self.latest_validator_votes_for_frozen_banks,
                    &mut vote_slots,
                );
                self.tbft_structs
                    .heaviest_subtree_fork_choice
                    .compute_bank_stats(
                        bank,
                        tower,
                        &mut self.latest_validator_votes_for_frozen_banks,
                    );

                let ComputedBankState {
                    voted_stakes,
                    total_stake,
                    fork_stake,
                    lockout_intervals,
                    my_latest_landed_vote,
                    ..
                } = computed_bank_state;
                let stats = self
                    .progress
                    .get_fork_stats_mut(bank_slot)
                    .expect("All frozen banks must exist in the Progress map");
                stats.fork_stake = fork_stake;
                stats.total_stake = total_stake;
                stats.voted_stakes = voted_stakes;
                stats.lockout_intervals = lockout_intervals;
                stats.block_height = bank.block_height();
                stats.my_latest_landed_vote = my_latest_landed_vote;
                stats.computed = true;
            }

            Self::cache_tower_stats(&mut self.progress, tower, bank_slot, ancestors);
        }

        vote_slots.shrink_to(MAX_LOCKOUT_HISTORY + 1);
    }

    fn adopt_on_chain_tower_if_behind(
        my_vote_pubkey: &Pubkey,
        ancestors: &HashMap<Slot, HashSet<Slot>>,
        frozen_banks: &[Arc<Bank>],
        tower: &mut Tower,
        progress: &mut ProgressMap,
        bank: &Arc<Bank>,
        bank_forks: &RwLock<BankForks>,
    ) {
        let Some(vote_account) = bank.get_vote_account(my_vote_pubkey) else {
            return;
        };
        let mut bank_vote_state = TowerVoteState::from(vote_account.vote_state_view());
        if bank_vote_state.last_voted_slot() <= tower.vote_state.last_voted_slot() {
            return;
        }

        if let Some(local_root) = tower.vote_state.root_slot
            && bank_vote_state
                .root_slot
                .map(|bank_root| local_root > bank_root)
                .unwrap_or(true)
        {
            bank_vote_state.root_slot = Some(local_root);
            bank_vote_state
                .votes
                .retain(|lockout| lockout.slot() > local_root);

            if let Some(first_vote) = bank_vote_state.votes.front() {
                assert!(
                    ancestors
                        .get(&first_vote.slot())
                        .expect(
                            "Ancestors map must contain an entry for all slots on this fork \
                             greater than the local root and less than the bank slot",
                        )
                        .contains(&local_root)
                );
            }
        }

        tower.vote_state = bank_vote_state;
        let last_voted_slot = tower.vote_state.last_voted_slot().unwrap_or_else(|| {
            tower
                .vote_state
                .root_slot
                .expect("root_slot cannot be None here")
        });
        let block_id = bank_forks
            .read()
            .unwrap()
            .get(last_voted_slot)
            .expect("Last voted slot that we are adopting must exist in bank forks")
            .block_id()
            .unwrap_or_default();
        tower.update_last_vote_from_vote_state(
            progress
                .get_hash(last_voted_slot)
                .expect("Must exist for us to have frozen descendant"),
            block_id.to_hash(),
        );

        for slot in frozen_banks.iter().map(|bank| bank.slot()) {
            if !progress
                .get_fork_stats(slot)
                .expect("All frozen banks must exist in fork stats")
                .computed
            {
                continue;
            }
            Self::cache_tower_stats(progress, tower, slot, ancestors);
        }
    }

    fn cache_tower_stats(
        progress: &mut ProgressMap,
        tower: &Tower,
        slot: Slot,
        ancestors: &HashMap<Slot, HashSet<Slot>>,
    ) {
        let stats = progress
            .get_fork_stats_mut(slot)
            .expect("All frozen banks must exist in the Progress map");
        stats.vote_threshold =
            tower.check_vote_stake_thresholds(slot, &stats.voted_stakes, stats.total_stake);
        stats.is_locked_out = tower.is_locked_out(
            slot,
            ancestors
                .get(&slot)
                .expect("Ancestors map should contain slot for is_locked_out() check"),
        );
        stats.has_voted = tower.has_voted(slot);
        stats.is_recent = tower.is_recent(slot);
    }

    pub fn set_root(&mut self, _my_pubkey: &Pubkey, new_root: Slot) {
        let dropped_banks = self
            .bank_forks
            .write()
            .unwrap()
            .set_root(new_root, None, None);
        drop(dropped_banks);

        let bank_forks = self.bank_forks.read().unwrap();
        self.progress.handle_new_root(&bank_forks);
        self.tbft_structs
            .heaviest_subtree_fork_choice
            .set_tree_root((new_root, bank_forks.root_bank().hash()));
    }

    pub fn create_and_vote_new_branch(
        &mut self,
        start_slot: Slot,
        end_slot: Slot,
        cluster_votes: &HashMap<Pubkey, Vec<u64>>,
        votes_to_simulate: &HashSet<Slot>,
        my_pubkey: &Pubkey,
        tower: &mut Tower,
    ) -> HashMap<Slot, Vec<HeaviestForkFailures>> {
        (start_slot + 1..=end_slot)
            .filter_map(|slot| {
                let mut fork_tip_parent = tr(slot - 1);
                fork_tip_parent.push_front(tr(slot));
                self.fill_bank_forks(fork_tip_parent, cluster_votes, true);
                if votes_to_simulate.contains(&slot) {
                    Some((slot, self.simulate_vote(slot, my_pubkey, tower)))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn simulate_lockout_interval(
        &mut self,
        slot: Slot,
        lockout_interval: (u64, u64),
        vote_account_pubkey: &Pubkey,
    ) {
        self.progress
            .entry(slot)
            .or_insert_with(|| ForkProgress::new(Hash::default(), None, None, 0, 0, None))
            .fork_stats
            .lockout_intervals
            .push(LockoutInterval {
                start: lockout_interval.0,
                end: lockout_interval.1,
                voter: *vote_account_pubkey,
            });
    }

    pub fn clear_lockout_intervals(&mut self, slot: Slot) {
        self.progress
            .entry(slot)
            .or_insert_with(|| ForkProgress::new(Hash::default(), None, None, 0, 0, None))
            .fork_stats
            .lockout_intervals
            .clear()
    }

    pub fn can_progress_on_fork(
        &mut self,
        my_pubkey: &Pubkey,
        tower: &mut Tower,
        start_slot: u64,
        num_slots: u64,
        cluster_votes: &mut HashMap<Pubkey, Vec<u64>>,
    ) -> bool {
        // Check that within some reasonable time, validator can make a new
        // root on this fork
        let old_root = tower.root();

        for i in 1..num_slots {
            // The parent of the tip of the fork
            let mut fork_tip_parent = tr(start_slot + i - 1);
            // The tip of the fork
            fork_tip_parent.push_front(tr(start_slot + i));
            self.fill_bank_forks(fork_tip_parent, cluster_votes, true);
            if self
                .simulate_vote(i + start_slot, my_pubkey, tower)
                .is_empty()
            {
                cluster_votes
                    .entry(*my_pubkey)
                    .or_default()
                    .push(start_slot + i);
            }
            if old_root != tower.root() {
                return true;
            }
        }

        false
    }

    #[allow(clippy::type_complexity)]
    fn init_state(
        num_keypairs: usize,
        bank_test_config: BankTestConfig,
    ) -> (
        HashMap<Pubkey, ValidatorVoteKeypairs>,
        Vec<Pubkey>,
        Vec<Pubkey>,
        Arc<RwLock<BankForks>>,
        ProgressMap,
        HeaviestSubtreeForkChoice,
    ) {
        let keypairs: HashMap<_, _> = std::iter::repeat_with(|| {
            let vote_keypairs = ValidatorVoteKeypairs::new_rand();
            (vote_keypairs.node_keypair.pubkey(), vote_keypairs)
        })
        .take(num_keypairs)
        .collect();
        let node_pubkeys: Vec<_> = keypairs
            .values()
            .map(|keys| keys.node_keypair.pubkey())
            .collect();
        let vote_pubkeys: Vec<_> = keypairs
            .values()
            .map(|keys| keys.vote_keypair.pubkey())
            .collect();

        let (bank_forks, progress, heaviest_subtree_fork_choice) =
            initialize_state_with(&keypairs, 10_000, bank_test_config);
        (
            keypairs,
            node_pubkeys,
            vote_pubkeys,
            bank_forks,
            progress,
            heaviest_subtree_fork_choice,
        )
    }
}

// Setup BankForks with bank 0 and all the validator accounts
pub fn initialize_state(
    validator_keypairs_map: &HashMap<Pubkey, ValidatorVoteKeypairs>,
    stake: u64,
) -> (
    Arc<RwLock<BankForks>>,
    ProgressMap,
    HeaviestSubtreeForkChoice,
) {
    initialize_state_with(validator_keypairs_map, stake, BankTestConfig::default())
}

pub fn initialize_state_with(
    validator_keypairs_map: &HashMap<Pubkey, ValidatorVoteKeypairs>,
    stake: u64,
    bank_test_config: BankTestConfig,
) -> (
    Arc<RwLock<BankForks>>,
    ProgressMap,
    HeaviestSubtreeForkChoice,
) {
    let validator_keypairs: Vec<_> = validator_keypairs_map.values().collect();
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_tower_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        vec![stake; validator_keypairs.len()],
    );

    genesis_config.epoch_schedule = EpochSchedule::without_warmup();
    genesis_config.poh_config.hashes_per_tick = Some(4);
    let (bank0, bank_forks) =
        Bank::new_with_paths_for_tests(&genesis_config, Some(bank_test_config), vec![], None)
            .wrap_with_bank_forks_for_tests();
    bank0.set_block_id(Some(BlockId::new_unique()));

    for pubkey in validator_keypairs_map.keys() {
        bank0.transfer(10_000, &mint_keypair, pubkey).unwrap();
    }

    bank0.fill_bank_with_ticks_for_tests();
    bank0.freeze();
    let mut progress = ProgressMap::default();
    progress.insert(
        0,
        ForkProgress::new_from_bank(
            &bank0,
            bank0.leader_id(),
            &Pubkey::default(),
            None,
            0,
            0,
            None,
        ),
    );
    let heaviest_subtree_fork_choice =
        HeaviestSubtreeForkChoice::new_from_bank_forks(bank_forks.clone());
    (bank_forks, progress, heaviest_subtree_fork_choice)
}
