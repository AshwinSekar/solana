//! Collection of all runtime features.
//!
//! Steps to add a new feature are outlined below. Note that these steps only cover
//! the process of getting a feature into the core Solana code.
//! - For features that are unambiguously good (ie bug fixes), these steps are sufficient.
//! - For features that should go up for community vote (ie fee structure changes), more
//!   information on the additional steps to follow can be found at:
//!   <https://spl.solana.com/feature-proposal#feature-proposal-life-cycle>
//!
//! 1. Generate a new keypair with `solana-keygen new --outfile feature.json --no-passphrase`
//!    - Keypairs should be held by core contributors only. If you're a non-core contirbutor going
//!      through these steps, the PR process will facilitate a keypair holder being picked. That
//!      person will generate the keypair, provide pubkey for PR, and ultimately enable the feature.
//! 2. Add a public module for the feature, specifying keypair pubkey as the id with
//!    `solana_sdk::declare_id!()` within the module.
//!    Additionally, add an entry to `FEATURE_NAMES` map.
//! 3. Add desired logic to check for and switch on feature availability.
//!
//! For more information on how features are picked up, see comments for `Feature`.

use {
    lazy_static::lazy_static,
    solana_sdk::{
        clock::Slot,
        hash::{Hash, Hasher},
        pubkey::Pubkey,
    },
    std::collections::{HashMap, HashSet},
};

pub mod deprecate_rewards_sysvar {
    solana_sdk::declare_id!("FGpWPxZLzxLCB76cDFag5Dd8MmYJUdqq4yp6krUSruSN");
}
pub mod pico_inflation {
    solana_sdk::declare_id!("3qFS9SA44vvxBAHKVwo8Jh6SoTeP1HkfF9X6VMJMVs1i");
}
pub mod full_inflation {
    solana_sdk::declare_id!("AuY1gMeg6uZAZfmFmigNbjuUwqqFQudd4ScnFUyfsxSx");
}
pub mod spl_token_v2_multisig_fix {
    solana_sdk::declare_id!("3pwXcXtj4GA3EvQk8mBMcpzVtYbkX4deFDnVRLj5bYX5");
}
pub mod no_overflow_rent_distribution {
    solana_sdk::declare_id!("8DqnxRJzJWfkLqVimuDJd47SusLTnBbbgjVbBCTMauQX");
}
pub mod filter_stake_delegation_accounts {
    solana_sdk::declare_id!("3xHFrSYXjYfd8PZvPrSZ11TGswtXoRfYzsEtB8omzVhH");
}
pub mod require_custodian_for_locked_stake_authorize {
    solana_sdk::declare_id!("EbieokGP5xp6xM2m3DRUYvRYcJBoTgXWzXTA9riwEKSs");
}
pub mod spl_token_v2_self_transfer_fix {
    solana_sdk::declare_id!("H6btcCeZzQ9EPrVBfSPauhpeemATL3NZvtkRjA3yzuH8");
}
pub mod warp_timestamp_again {
    solana_sdk::declare_id!("GT9LRbtd3hkgyHkAih51FfwLQfY1ZVSUu5MiUU9kAv6h");
}
pub mod check_init_vote_data {
    solana_sdk::declare_id!("71qYzpHbhyT1Ad37S4j5MqVr8hDPtkk4DNDfBncHws1n");
}
pub mod secp256k1_recover_syscall_enabled {
    solana_sdk::declare_id!("GBe1WQSZUM3EMoE5tTAjVx8VznbE5No86hvE8FS6syiq");
}
pub mod system_transfer_zero_check {
    solana_sdk::declare_id!("8717kr3CUKEw4R7R64sT1GdP1x8i6MhbnrjWsFoxTwo5");
}
pub mod blake3_syscall_enabled {
    solana_sdk::declare_id!("HwcgvWUZcdrZ2mo2imq26LiMHBcEQ4e7f9TMT2gy4GUt");
}
pub mod dedupe_config_program_signers {
    solana_sdk::declare_id!("DjE7Y5enrKhWdPz3MZfwRdgVcDpxmpsYPvAtfiTNkx6C");
}
pub mod deterministic_shred_seed_enabled {
    solana_sdk::declare_id!("7uxsxDNFr1EunPS6VpoWhHPFhKLX1CEknEFcF5oKiwC2");
}
pub mod verify_tx_signatures_len {
    solana_sdk::declare_id!("z7aqhkFo6Nzr6upNyAwTs8SmLXdffG2xjodmZSzV7Qm");
}
pub mod vote_stake_checked_instructions {
    solana_sdk::declare_id!("6S53raTXVuzbPyvqds9N9tTL7Ze94Fxbkyvpm7heFfLn");
}
pub mod neon_evm_compute_budget {
    solana_sdk::declare_id!("hQk7yM6vZNZmntEyScU9oRUiuVzGNLA5V8bJ4ifE2bX");
}
pub mod rent_for_sysvars {
    solana_sdk::declare_id!("5RmQy4QRK7VUF3AMVSc9HxDFEgqHqvHmLBpFZx9XQJUU");
}
pub mod libsecp256k1_0_5_upgrade_enabled {
    solana_sdk::declare_id!("4T1vifMe3LTszpeiRD7b6VESr65Gw9je7x5iA7wuyRti");
}
pub mod tx_wide_compute_cap {
    solana_sdk::declare_id!("CD3zGnnuag1RortC9p4zKqsQEXWqhtkCZuAWyDmKhY1r");
}
pub mod spl_token_v2_set_authority_fix {
    solana_sdk::declare_id!("AQJeNufzEet8ztdUJ8cjDtfCQrysoRUEC6xK79rkiuav");
}
pub mod merge_nonce_error_into_system_error {
    solana_sdk::declare_id!("2Rmhyk6YvtXp1Zqu5PN1JJh1m81FsYfjvTqDuis2U2gm");
}
pub mod disable_fees_sysvar {
    solana_sdk::declare_id!("DXQmfgfEoVsqu8XtaJWM77VFCKGXQzUtLQtVFAC8WrnB");
}
pub mod stake_merge_with_unmatched_credits_observed {
    solana_sdk::declare_id!("7P5qTf8Sn97z1Yo4te9Fr6TCPMmQfeKqLhgqHNFKWrne");
}
pub mod gate_large_block {
    solana_sdk::declare_id!("5GgX3bPiMUCYec8vEMCyzMifW1rpMSYGPzHE7KhnpFGu");
}
pub mod zk_token_sdk_enabled {
    solana_sdk::declare_id!("FsdDG1GYVVoemC9MHhmDzy46jRkeYEjQQFogA2CmbPFm");
}
pub mod versioned_tx_message_enabled {
    solana_sdk::declare_id!("845P3y8jxhwxzpmG4W8CbhnPrGBdRkYZF59WztVnWxJU");
}
pub mod libsecp256k1_fail_on_bad_count {
    solana_sdk::declare_id!("9TZcXgspfRVAdyFNBs1BE3njVTFhXj63VCm92qF9yKmq");
}
pub mod instructions_sysvar_owned_by_sysvar {
    solana_sdk::declare_id!("7Z1gG4wJCXnSrzYc2CMscBZqNBxs777drWk5aZeKBa3A");
}
pub mod stake_program_advance_activating_credits_observed {
    solana_sdk::declare_id!("9ornnpLBSyLb8MTKWdgrUduhGX1maun8Lo7suewmXPmu");
}
pub mod demote_program_write_locks {
    solana_sdk::declare_id!("DTLUJZgnVjyrygM5EmCaWwrQ2u1687YnDhnpeSVwZhip");
}
pub mod ed25519_program_enabled {
    solana_sdk::declare_id!("7opUoT7uuEcyCwh62G5c1jQMLJYSoYDRzLDCS6znKZk4");
}
pub mod return_data_syscall_enabled {
    solana_sdk::declare_id!("BNo4ijyX8Zd7UZ2SyPn8XsNyRMRkX9JKMLbXpyEeBpwz");
}
pub mod reduce_required_deploy_balance {
    solana_sdk::declare_id!("DGhgbet8wJXU6MJ7JBxLh6f9jr2Tz6asAufYSezA5yeU");
}
pub mod sol_log_data_syscall_enabled {
    solana_sdk::declare_id!("5KEvGGRPSLyZhHeEZmDEmTbNY2fvaag1RUJguUDTvDVJ");
}
pub mod stakes_remove_delegation_if_inactive {
    solana_sdk::declare_id!("9ttykDExvEyhfvbHegWxqvfCioN5QboMydkhC4a2pxSy");
}
pub mod do_support_realloc {
    solana_sdk::declare_id!("3TrfuioYo4mPfpwLTyeFaLhLLPBHoxoKWd3bcNvgjn8t");
}
pub mod prevent_calling_precompiles_as_programs {
    solana_sdk::declare_id!("5qv7vFELxtSMALD3qo6M2qrSKKmopiWf8oHg9j8Ym2Fr");
}
pub mod optimize_epoch_boundary_updates {
    solana_sdk::declare_id!("9h8vt53Pdb3ZWsXHZgsXsMkg2eGTurVBzG6NSuDA9UcY");
}
pub mod remove_native_loader {
    solana_sdk::declare_id!("6LFXD1GQLfcNq2v5R9DyZ3ntkhz6QnzduumxKoQAmHWc");
}
pub mod send_to_tpu_vote_port {
    solana_sdk::declare_id!("BYnrrhummeTVsD41arxwqfGPZS53Do2MWiWCXyBMzx3b");
}
pub mod turbine_peers_shuffle {
    solana_sdk::declare_id!("DpYdmrdXKaNnGsqSM7ThrdpCw6h8TDxq8GKLBVbHCf4a");
}
pub mod requestable_heap_size {
    solana_sdk::declare_id!("CnpLAm3xQYdwzodvmpAW9Fop1rw43w5c9r69kRKQwTjg");
}
pub mod disable_fee_calculator {
    solana_sdk::declare_id!("6LG7nCRuCAxNZsFXukRPoVjW71PLH6AnNAFWovYF9MmG");
}
pub mod add_compute_budget_program {
    solana_sdk::declare_id!("dJxcqo2UqcB49JWthDcfmEeFzXkMvBbZdEC78WBPdLY");
}
pub mod nonce_must_be_writable {
    solana_sdk::declare_id!("4YSA8LJzgtZGxcx7tcX41Una87fSTRmY4BQ2rEfcFj5D");
}
pub mod spl_token_v3_3_0_release {
    solana_sdk::declare_id!("E1Dku735fP3BdAftEhEJUDFvM7Q4jJfm532i1qVHYAXZ");
}
pub mod leave_nonce_on_success {
    solana_sdk::declare_id!("C86U4hVQMGu2Kr8nVAdeekX3iCUK6XZu9XBuqaMmbf1P");
}
pub mod reject_empty_instruction_without_program {
    solana_sdk::declare_id!("4JTKsHpvo26AwuCPyBLZTjj7RxjCaS7xwyxhBx5e2FQm");
}
pub mod fixed_memcpy_nonoverlapping_check {
    solana_sdk::declare_id!("5MGxwxRUz1VhVMRXgWDUrVnQWkzh4e5ushsMqGjGp9Vk");
}
pub mod reject_non_rent_exempt_vote_withdraws {
    solana_sdk::declare_id!("AMbzsaX7hWYErxRAViUYpChwjt48yfGWZQJhsXzTqfkd");
}
pub mod evict_invalid_stakes_cache_entries {
    solana_sdk::declare_id!("GdhFun6iRM193JhEEQxbbd4GtFyG4YfvfEeyV32odReP");
}
pub mod allow_votes_to_directly_update_vote_state {
    solana_sdk::declare_id!("5vS6Rx2f2mkSBVKEZsE3fWGKpj8fsGE3KyHe12EryerT");
}
pub mod cap_accounts_data_len {
    solana_sdk::declare_id!("4pN2iCxPFRHHMwxdQwAodWbwn9TNdkaufcTDFoSQr687");
}
pub mod max_tx_account_locks {
    solana_sdk::declare_id!("Gz5ixKejm2JAun1gtRNF8xZzkxZa2nTQLWgieo6EZkRR");
}
pub mod require_rent_exempt_accounts {
    solana_sdk::declare_id!("5BGh1fQpNddQSqDcaNMtYWanLbbs4WSaCcGhC5C7dUVy");
}
pub mod filter_votes_outside_slot_hashes {
    solana_sdk::declare_id!("DYyMRLnhsA3qvfixFznmphKbedxW89Z5FV3hUKJjf4qY");
}
pub mod update_syscall_base_costs {
    solana_sdk::declare_id!("33Re8fE3qVddMuxvsC2q3UbV9uXgQU4VGv8DxhuE5vs2");
}
pub mod vote_withdraw_authority_may_change_authorized_voter {
    solana_sdk::declare_id!("4NK2V7kqVYYhTqHfTvGBErC5kWwWfkBBYqFE5uLfQf98");
}
pub mod spl_associated_token_account_v1_0_4 {
    solana_sdk::declare_id!("DQeGEKcPkFhKP6Kv25HSZd8CYrb2HZzJYNxdX7ckDEzZ");
}
pub mod reject_vote_account_close_unless_zero_credit_epoch {
    solana_sdk::declare_id!("EC9nawC61AyoCm4QtGAHDovsVXpD8haZRkCeAvJjhdp");
}
pub mod add_get_processed_sibling_instruction_syscall {
    solana_sdk::declare_id!("87h42UUmdT1w8pCMQDei9QJ5gJV7F9dxdruEHWTRawk7");
}
pub mod bank_tranaction_count_fix {
    solana_sdk::declare_id!("9HCTre7KzVoGezsgAnoNPN5jTMTxejFtkzbNnTQ9GjpK");
}
pub mod disable_bpf_deprecated_load_instructions {
    solana_sdk::declare_id!("FAGLcQx4yrDiPoBPpeiDysU8yXNUBQsttEBnPAdYyrVS");
}
pub mod disable_bpf_unresolved_symbols_at_runtime {
    solana_sdk::declare_id!("Fy6cWD1bEKXLvjTXQJRTZDsiyiWj9sW2me6WuRpmcLvP");
}
pub mod record_instruction_in_transaction_context_push {
    solana_sdk::declare_id!("5qtUKvB9nw2pH6hhuMF4Cj8L2Bvj4JPhD9WSDf1c64k1");
}

lazy_static! {
    /// Map of feature identifiers to user-visible description
    pub static ref FEATURE_NAMES: HashMap<Pubkey, &'static str> = [
        (deprecate_rewards_sysvar::id(), "deprecate unused rewards sysvar"),
        (pico_inflation::id(), "pico inflation"),
        (full_inflation::id(), "full inflation on devnet and testnet"),
        (spl_token_v2_multisig_fix::id(), "spl-token multisig fix"),
        (no_overflow_rent_distribution::id(), "no overflow rent distribution"),
        (filter_stake_delegation_accounts::id(), "filter stake_delegation_accounts #14062"),
        (require_custodian_for_locked_stake_authorize::id(), "require custodian to authorize withdrawer change for locked stake"),
        (spl_token_v2_self_transfer_fix::id(), "spl-token self-transfer fix"),
        (warp_timestamp_again::id(), "warp timestamp again, adjust bounding to 25% fast 80% slow #15204"),
        (check_init_vote_data::id(), "check initialized Vote data"),
        (secp256k1_recover_syscall_enabled::id(), "secp256k1_recover syscall"),
        (system_transfer_zero_check::id(), "perform all checks for transfers of 0 lamports"),
        (blake3_syscall_enabled::id(), "blake3 syscall"),
        (dedupe_config_program_signers::id(), "dedupe config program signers"),
        (deterministic_shred_seed_enabled::id(), "deterministic shred seed"),
        (verify_tx_signatures_len::id(), "prohibit extra transaction signatures"),
        (vote_stake_checked_instructions::id(), "vote/state program checked instructions #18345"),
        (neon_evm_compute_budget::id(), "bump neon_evm's compute budget"),
        (rent_for_sysvars::id(), "collect rent from accounts owned by sysvars"),
        (libsecp256k1_0_5_upgrade_enabled::id(), "upgrade libsecp256k1 to v0.5.0"),
        (tx_wide_compute_cap::id(), "transaction wide compute cap"),
        (spl_token_v2_set_authority_fix::id(), "spl-token set_authority fix"),
        (merge_nonce_error_into_system_error::id(), "merge NonceError into SystemError"),
        (disable_fees_sysvar::id(), "disable fees sysvar"),
        (stake_merge_with_unmatched_credits_observed::id(), "allow merging active stakes with unmatched credits_observed #18985"),
        (gate_large_block::id(), "validator checks block cost against max limit in realtime, reject if exceeds."),
        (zk_token_sdk_enabled::id(), "enable Zk Token proof program and syscalls"),
        (versioned_tx_message_enabled::id(), "enable versioned transaction message processing"),
        (libsecp256k1_fail_on_bad_count::id(), "fail libsec256k1_verify if count appears wrong"),
        (instructions_sysvar_owned_by_sysvar::id(), "fix owner for instructions sysvar"),
        (stake_program_advance_activating_credits_observed::id(), "Enable advancing credits observed for activation epoch #19309"),
        (demote_program_write_locks::id(), "demote program write locks to readonly, except when upgradeable loader present #19593 #20265"),
        (ed25519_program_enabled::id(), "enable builtin ed25519 signature verify program"),
        (return_data_syscall_enabled::id(), "enable sol_{set,get}_return_data syscall"),
        (reduce_required_deploy_balance::id(), "reduce required payer balance for program deploys"),
        (sol_log_data_syscall_enabled::id(), "enable sol_log_data syscall"),
        (stakes_remove_delegation_if_inactive::id(), "remove delegations from stakes cache when inactive"),
        (do_support_realloc::id(), "support account data reallocation"),
        (prevent_calling_precompiles_as_programs::id(), "prevent calling precompiles as programs"),
        (optimize_epoch_boundary_updates::id(), "optimize epoch boundary updates"),
        (remove_native_loader::id(), "remove support for the native loader"),
        (send_to_tpu_vote_port::id(), "send votes to the tpu vote port"),
        (turbine_peers_shuffle::id(), "turbine peers shuffle patch"),
        (requestable_heap_size::id(), "Requestable heap frame size"),
        (disable_fee_calculator::id(), "deprecate fee calculator"),
        (add_compute_budget_program::id(), "Add compute_budget_program"),
        (nonce_must_be_writable::id(), "nonce must be writable"),
        (spl_token_v3_3_0_release::id(), "spl-token v3.3.0 release"),
        (leave_nonce_on_success::id(), "leave nonce as is on success"),
        (reject_empty_instruction_without_program::id(), "fail instructions which have native_loader as program_id directly"),
        (fixed_memcpy_nonoverlapping_check::id(), "use correct check for nonoverlapping regions in memcpy syscall"),
        (reject_non_rent_exempt_vote_withdraws::id(), "fail vote withdraw instructions which leave the account non-rent-exempt"),
        (evict_invalid_stakes_cache_entries::id(), "evict invalid stakes cache entries on epoch boundaries"),
        (allow_votes_to_directly_update_vote_state::id(), "enable direct vote state update"),
        (cap_accounts_data_len::id(), "cap the accounts data len"),
        (max_tx_account_locks::id(), "enforce max number of locked accounts per transaction"),
        (require_rent_exempt_accounts::id(), "require all new transaction accounts with data to be rent-exempt"),
        (filter_votes_outside_slot_hashes::id(), "filter vote slots older than the slot hashes history"),
        (update_syscall_base_costs::id(), "Update syscall base costs"),
        (vote_withdraw_authority_may_change_authorized_voter::id(), "vote account withdraw authority may change the authorized voter #22521"),
        (spl_associated_token_account_v1_0_4::id(), "SPL Associated Token Account Program release version 1.0.4, tied to token 3.3.0 #22648"),
        (reject_vote_account_close_unless_zero_credit_epoch::id(), "fail vote account withdraw to 0 unless account earned 0 credits in last completed epoch"),
        (add_get_processed_sibling_instruction_syscall::id(), "add add_get_processed_sibling_instruction_syscall"),
        (bank_tranaction_count_fix::id(), "Fixes Bank::transaction_count to include all committed transactions, not just successful ones"),
        (disable_bpf_deprecated_load_instructions::id(), "Disable ldabs* and ldind* BPF instructions"),
        (disable_bpf_unresolved_symbols_at_runtime::id(), "Disable reporting of unresolved BPF symbols at runtime"),
        (record_instruction_in_transaction_context_push::id(), "Move the CPI stack overflow check to the end of push"),
        /*************** ADD NEW FEATURES HERE ***************/
    ]
    .iter()
    .cloned()
    .collect();

    /// Unique identifier of the current software's feature set
    pub static ref ID: Hash = {
        let mut hasher = Hasher::default();
        let mut feature_ids = FEATURE_NAMES.keys().collect::<Vec<_>>();
        feature_ids.sort();
        for feature in feature_ids {
            hasher.hash(feature.as_ref());
        }
        hasher.result()
    };
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FullInflationFeaturePair {
    pub vote_id: Pubkey, // Feature that grants the candidate the ability to enable full inflation
    pub enable_id: Pubkey, // Feature to enable full inflation by the candidate
}

lazy_static! {
    /// Set of feature pairs that once enabled will trigger full inflation
    pub static ref FULL_INFLATION_FEATURE_PAIRS: HashSet<FullInflationFeaturePair> = [
        FullInflationFeaturePair {
            vote_id: full_inflation::id(),
            enable_id: full_inflation::id(),
        },
    ]
    .iter()
    .cloned()
    .collect();
}

/// `FeatureSet` holds the set of currently active/inactive runtime features
#[derive(AbiExample, Debug, Clone)]
pub struct FeatureSet {
    pub active: HashMap<Pubkey, Slot>,
    pub inactive: HashSet<Pubkey>,
}
impl Default for FeatureSet {
    fn default() -> Self {
        // All features disabled
        Self {
            active: HashMap::new(),
            inactive: FEATURE_NAMES.keys().cloned().collect(),
        }
    }
}
impl FeatureSet {
    pub fn is_active(&self, feature_id: &Pubkey) -> bool {
        self.active.contains_key(feature_id)
    }

    pub fn activated_slot(&self, feature_id: &Pubkey) -> Option<Slot> {
        self.active.get(feature_id).copied()
    }

    /// List of enabled features that trigger full inflation
    pub fn full_inflation_features_enabled(&self) -> HashSet<Pubkey> {
        let mut hash_set = FULL_INFLATION_FEATURE_PAIRS
            .iter()
            .filter_map(|pair| {
                if self.is_active(&pair.vote_id) && self.is_active(&pair.enable_id) {
                    Some(pair.enable_id)
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();

        if self.is_active(&full_inflation::id()) {
            hash_set.insert(full_inflation::id());
        }
        hash_set
    }

    /// All features enabled, useful for testing
    pub fn all_enabled() -> Self {
        Self {
            active: FEATURE_NAMES.keys().cloned().map(|key| (key, 0)).collect(),
            inactive: HashSet::new(),
        }
    }

    /// Activate a feature
    pub fn activate(&mut self, feature_id: &Pubkey, slot: u64) {
        self.inactive.remove(feature_id);
        self.active.insert(*feature_id, slot);
    }

    /// Deactivate a feature
    pub fn deactivate(&mut self, feature_id: &Pubkey) {
        self.active.remove(feature_id);
        self.inactive.insert(*feature_id);
    }
}
