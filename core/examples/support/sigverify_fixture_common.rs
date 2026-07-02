//! Code shared by both halves of the synthetic BLS sigverify example: the
//! fixture generator (`sigverify_fixture_build`) and the replay driver
//! (`sigverify_fixture_replay`).
use {
    agave_bls_sigverify::{
        bls_sigverifier::{SigVerifier, SigVerifierChannels, SigVerifierContext},
        generated_cert_types::GeneratedCertTypes,
        sig_verified_messages::SigVerifiedBatch,
    },
    agave_votor_messages::{
        VerifiedVoterSlotsReceiver, metric_types::ConsensusMetricsEvent,
        migration::MigrationStatus, reward_certificate::AddVoteMessage,
    },
    crossbeam_channel::{Receiver, Sender, bounded},
    rand::{RngCore, SeedableRng, rngs::StdRng},
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::{Keypair, Signer},
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_net_utils::SocketAddrSpace,
    solana_perf::packet::PacketBatch,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
    },
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::sync::Arc,
};

pub const CHANNEL_SIZE: usize = 1024;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum StoredPacketKind {
    Vote,
    Cert,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredPacket {
    pub message_bytes: Vec<u8>,
    pub remote_pubkey: Pubkey,
    pub kind: StoredPacketKind,
    pub slot_index: usize,
    pub arrival_us: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredWorkload {
    pub seed: u64,
    pub num_slots: usize,
    pub votes_per_slot: usize,
    pub certs_per_slot: usize,
    pub base_slot: u64,
    pub slot_window_us: u64,
    pub cert_signers: usize,
    pub num_validators: usize,
    pub total_packets: usize,
    pub vote_packets: usize,
    pub cert_packets: usize,
    pub packets: Vec<StoredPacket>,
}

// The generator reads only the keypairs/ranks; the replay driver uses the
// verifier and packet channel. Each binary leaves the other's fields unused.
#[allow(dead_code)]
pub struct ExampleContext {
    pub verifier: SigVerifier,
    pub packet_sender: Sender<PacketBatch>,
    pub validator_keypairs: Vec<ValidatorVoteKeypairs>,
    pub validator_ranks: Vec<u16>,
    pub _repair_receiver: VerifiedVoterSlotsReceiver,
    pub _reward_receiver: Receiver<AddVoteMessage>,
    pub _pool_receiver: Receiver<SigVerifiedBatch>,
    pub _metrics_receiver: Receiver<(std::time::Instant, Vec<ConsensusMetricsEvent>)>,
}

pub fn fixture_max_slot(base_slot: u64, num_slots: usize) -> u64 {
    base_slot.saturating_add(num_slots.saturating_sub(1) as u64)
}

fn derive_key_seed(global_seed: u64, validator_index: usize, key_kind: u64) -> [u8; 32] {
    let mixed = global_seed
        ^ (validator_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ key_kind.wrapping_mul(0xD6E8_FD50_4E33_9A4D);

    let mut rng = StdRng::seed_from_u64(mixed);
    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);
    seed
}

fn make_deterministic_validator_vote_keypairs(
    global_seed: u64,
    num_validators: usize,
) -> Vec<ValidatorVoteKeypairs> {
    (0..num_validators)
        .map(|validator_index| {
            let node_seed = derive_key_seed(global_seed, validator_index, 0);
            let vote_seed = derive_key_seed(global_seed, validator_index, 1);
            let stake_seed = derive_key_seed(global_seed, validator_index, 2);

            let node_keypair = Keypair::new_from_array(node_seed);
            let vote_keypair = Keypair::new_from_array(vote_seed);
            let stake_keypair = Keypair::new_from_array(stake_seed);

            ValidatorVoteKeypairs::new(node_keypair, vote_keypair, stake_keypair)
        })
        .collect()
}

pub fn init_example_context(
    num_threads: usize,
    num_validators: usize,
    seed: u64,
    base_slot: u64,
    max_slot: u64,
) -> ExampleContext {
    assert!(
        max_slot >= base_slot,
        "max_slot must be >= base_slot for sigverify fixture context"
    );

    let validator_keypairs = make_deterministic_validator_vote_keypairs(seed, num_validators);

    let stakes: Vec<_> = (0..validator_keypairs.len()).map(|_| 1_000_u64).collect();

    let genesis = create_genesis_config_with_alpenglow_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        stakes,
    );

    let bank0 = Bank::new_for_tests(&genesis.genesis_config);
    let (bank0, _temp_bank_forks) = bank0.wrap_with_bank_forks_for_tests();

    let mut parent = bank0;
    let mut bank = Bank::new_from_parent(
        parent.clone(),
        solana_runtime::bank::SlotLeader::default(),
        1,
    );

    for slot in 2..=max_slot {
        parent = Arc::new(bank);
        bank = Bank::new_from_parent(
            parent.clone(),
            solana_runtime::bank::SlotLeader::default(),
            slot,
        );
    }

    let bank_forks = BankForks::new_rw_arc(bank);
    let sharable_banks = bank_forks
        .read()
        .expect("bank_forks poisoned")
        .sharable_banks();

    let root_bank = sharable_banks.root();

    eprintln!(
        "[fixture-context] bank_root_slot={}, workload_slots={}..={}",
        root_bank.slot(),
        base_slot,
        max_slot,
    );

    for slot in base_slot..=max_slot {
        root_bank
            .get_rank_map(slot)
            .unwrap_or_else(|| panic!("rank map for slot {slot} must exist in bench"));
    }

    let rank_map = root_bank
        .get_rank_map(base_slot)
        .unwrap_or_else(|| panic!("rank map for base_slot {base_slot} must exist in bench"));

    let validator_ranks: Vec<u16> = validator_keypairs
        .iter()
        .map(|validator| {
            let validator_bls_pubkey = validator.bls_keypair.public;

            (0..validator_keypairs.len())
                .find_map(|i| {
                    rank_map.get_pubkey_stake_entry(i).and_then(|entry| {
                        (entry.bls_pubkey == validator_bls_pubkey)
                            .then_some(u16::try_from(i).expect("validator index must fit into u16"))
                    })
                })
                .expect("validator BLS pubkey must exist in rank map")
        })
        .collect();

    let keypair = Keypair::new();
    // new_localhost hardcodes shred_version 0; the generator's FIXTURE_SHRED_VERSION
    // must match it or the sigverifier drops every packet as malformed.
    let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);

    let cluster_info = Arc::new(ClusterInfo::new(
        contact_info,
        Arc::new(keypair),
        SocketAddrSpace::Unspecified,
    ));

    let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

    let (repair_sender, repair_receiver) = bounded(CHANNEL_SIZE);
    let (reward_sender, reward_receiver) = bounded(CHANNEL_SIZE);
    let (pool_sender, pool_receiver) = bounded(CHANNEL_SIZE);
    let (metrics_sender, metrics_receiver) = bounded(CHANNEL_SIZE);
    let (packet_sender, packet_receiver) = bounded(CHANNEL_SIZE);

    let banlist = {
        let (banlist, _) = SimpleQosBanlist::new();
        Arc::new(banlist)
    };

    let generated_cert_types = Arc::new(GeneratedCertTypes::default());

    let verifier = SigVerifier::new(
        SigVerifierContext::new(
            Arc::new(MigrationStatus::default()),
            banlist,
            sharable_banks,
            cluster_info,
            leader_schedule,
            num_threads,
            generated_cert_types,
        ),
        SigVerifierChannels::new(
            packet_receiver,
            repair_sender,
            reward_sender,
            pool_sender,
            metrics_sender,
        ),
    );

    ExampleContext {
        verifier,
        packet_sender,
        validator_keypairs,
        validator_ranks,
        _repair_receiver: repair_receiver,
        _reward_receiver: reward_receiver,
        _pool_receiver: pool_receiver,
        _metrics_receiver: metrics_receiver,
    }
}
