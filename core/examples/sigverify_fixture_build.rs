//! Build a slot-based synthetic sigverify fixture and save it to disk.

#![allow(clippy::arithmetic_side_effects)]

extern crate clap4 as clap;

#[path = "support/sigverify_fixture_common.rs"]
mod sigverify_fixture_common;

use {
    agave_votor::consensus_pool::certificate_builder::CertificateBuilder,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, ConsensusMessage, VoteMessage},
        vote::Vote,
        wire::{VersionedWireConsensusMessage, VotePayloadToSign},
    },
    clap::Parser,
    rand::{Rng, SeedableRng, rngs::StdRng},
    rayon::prelude::*,
    sigverify_fixture_common::{
        ExampleContext, StoredPacket, StoredPacketKind, StoredWorkload, fixture_max_slot,
        init_example_context,
    },
    solana_bls_signatures::Signature as BLSSignature,
    solana_hash::Hash,
    solana_keypair::Signer,
    solana_pubkey::Pubkey,
    solana_runtime::genesis_utils::ValidatorVoteKeypairs,
    std::{
        fs::File,
        io::BufWriter,
        path::Path,
        process::exit,
        sync::atomic::{AtomicUsize, Ordering},
    },
};

// Synthetic slot duration used only for assigning packet arrival times.
const SLOT_WINDOW_US: u64 = 200_000;

// Shred version stamped into every signed payload and wire message. It must
// stay 0 and cannot be configured: the emulated node's ContactInfo is built via
// `ContactInfo::new_localhost` (in init_example_context), which hardcodes
// shred_version 0, and the sigverifier drops any packet whose shred_version !=
// cluster_info.my_shred_version(). Changing this without also changing the
// emulated node's shred version would make every fixture packet get rejected.
const FIXTURE_SHRED_VERSION: u16 = 0;

// Upper bound on the validator set. A certificate's signer bitmap grows with
// the rank space, and the whole wire message must fit in one packet
// (PACKET_DATA_SIZE = 1232 bytes) or `stored_packet_to_packet` overflows the
// fixed packet buffer. 2000 validators keeps the serialized cert safely under
// that limit.
const MAX_VALIDATORS: usize = 2000;

#[derive(Parser)]
pub struct FixtureBuildConfig {
    #[arg(long, help = "Seed value kept for reproducibility and metadata")]
    pub seed: u64,

    #[arg(long = "num-slots", help = "Number of slots to generate")]
    pub num_slots: usize,

    #[arg(
        long = "votes-per-slot",
        help = "Number of vote packets generated per slot"
    )]
    pub votes_per_slot: usize,

    #[arg(
        long = "certs-per-slot",
        help = "Number of certificate packets generated per slot"
    )]
    pub certs_per_slot: usize,

    #[arg(
        long = "base-slot",
        default_value_t = 10,
        help = "First slot used in the synthetic dataset"
    )]
    pub base_slot: u64,

    #[arg(long = "num-validators", help = "Size of the synthetic validator set")]
    pub num_validators: usize,

    #[arg(long = "output", help = "Path to write the fixture file")]
    pub output: String,
}

/// Number of certificate signers: 61% of the vote-casting node set, rounded up.
fn cert_signers(num_validators: usize) -> usize {
    (num_validators * 61).div_ceil(100)
}

fn validate_fixture_build_config(config: &FixtureBuildConfig) -> Result<(), String> {
    if config.num_slots == 0 {
        return Err("num_slots must be > 0".to_string());
    }

    if config.votes_per_slot == 0 && config.certs_per_slot == 0 {
        return Err("votes_per_slot and certs_per_slot cannot both be 0".to_string());
    }

    if config.num_validators == 0 {
        return Err("num_validators must be > 0".to_string());
    }

    if config.num_validators > MAX_VALIDATORS {
        return Err(format!(
            "num_validators must be <= {MAX_VALIDATORS} so a certificate fits in one packet"
        ));
    }

    Ok(())
}

fn create_signed_vote_message(
    validator_keypairs: &[ValidatorVoteKeypairs],
    validator_ranks: &[u16],
    vote: Vote,
    validator_index: usize,
) -> VoteMessage {
    let bls_keypair = &validator_keypairs[validator_index].bls_keypair;
    let payload_to_sign = VotePayloadToSign::new_from_vote(vote, FIXTURE_SHRED_VERSION);
    let payload = wincode::serialize(&payload_to_sign).expect("failed to serialize vote payload");
    let signature: BLSSignature = bls_keypair.sign(&payload).into();

    VoteMessage {
        vote,
        signature,
        rank: validator_ranks[validator_index],
    }
}

fn create_signed_certificate_message(
    validator_keypairs: &[ValidatorVoteKeypairs],
    validator_ranks: &[u16],
    cert_type: CertificateType,
    validator_indices: &[usize],
) -> Certificate {
    let mut builder = CertificateBuilder::new(cert_type);
    let vote = cert_type.to_source_vote();

    let vote_messages: Vec<VoteMessage> = validator_indices
        .iter()
        .map(|&validator_index| {
            create_signed_vote_message(validator_keypairs, validator_ranks, vote, validator_index)
        })
        .collect();

    builder
        .aggregate(&vote_messages)
        .expect("failed to aggregate votes for synthetic certificate");

    builder
        .build()
        .expect("failed to build synthetic certificate")
}

fn build_vote_message_and_remote(
    ctx: &ExampleContext,
    slot: u64,
    block_id: Hash,
    global_vote_index: usize,
) -> (ConsensusMessage, Pubkey) {
    let validator_index = global_vote_index % ctx.validator_keypairs.len();
    let validator = &ctx.validator_keypairs[validator_index];
    let rank = ctx.validator_ranks[validator_index];

    let block = Block { slot, block_id };
    let vote = Vote::new_notarization_vote(block);
    let payload_to_sign = VotePayloadToSign::new_from_vote(vote, FIXTURE_SHRED_VERSION);
    let payload = wincode::serialize(&payload_to_sign).expect("failed to serialize vote payload");
    let signature = validator.bls_keypair.sign(&payload).into();

    let vote_msg = VoteMessage {
        vote,
        signature,
        rank,
    };

    (
        ConsensusMessage::Vote(vote_msg),
        validator.node_keypair.pubkey(),
    )
}

fn build_cert_message_and_remote(
    ctx: &ExampleContext,
    slot: u64,
    block_id: Hash,
) -> (ConsensusMessage, Pubkey) {
    let block = Block { slot, block_id };
    let cert_type = CertificateType::Notarize(block);

    let validator_indices: Vec<usize> = (0..cert_signers(ctx.validator_keypairs.len())).collect();
    let cert = create_signed_certificate_message(
        &ctx.validator_keypairs,
        &ctx.validator_ranks,
        cert_type,
        &validator_indices,
    );

    (
        ConsensusMessage::Certificate(cert),
        ctx.validator_keypairs[0].node_keypair.pubkey(),
    )
}

fn consensus_message_to_stored_packet(
    message: &ConsensusMessage,
    remote_pubkey: Pubkey,
    kind: StoredPacketKind,
    slot_index: usize,
    arrival_us: u64,
) -> StoredPacket {
    let wire_message = VersionedWireConsensusMessage::new(message.clone(), FIXTURE_SHRED_VERSION);
    let mut message_bytes = Vec::new();

    wincode::config::serialize_into(
        &mut message_bytes,
        &wire_message,
        solana_perf::packet::packet_config(),
    )
    .expect("failed to serialize wire consensus message");

    StoredPacket {
        message_bytes,
        remote_pubkey,
        kind,
        slot_index,
        arrival_us,
    }
}

fn build_stored_workload(ctx: &ExampleContext, config: &FixtureBuildConfig) -> StoredWorkload {
    let total_packets = config
        .num_slots
        .saturating_mul(config.votes_per_slot.saturating_add(config.certs_per_slot));

    let completed_slots = AtomicUsize::new(0);

    let per_slot_packets: Vec<Vec<StoredPacket>> = (0..config.num_slots)
        .into_par_iter()
        .map(|slot_index| {
            let slot = config.base_slot + slot_index as u64;
            let block_id = Hash::new_unique();

            let mut slot_rng = StdRng::seed_from_u64(
                config.seed ^ (slot_index as u64).wrapping_mul(0xA076_1D64_78BD_642F),
            );

            let slot_start_us = slot_index as u64 * SLOT_WINDOW_US;

            let mut slot_packets =
                Vec::with_capacity(config.votes_per_slot + config.certs_per_slot);

            let vote_base_index = slot_index.saturating_mul(config.votes_per_slot);

            for vote_index_in_slot in 0..config.votes_per_slot {
                let global_vote_index = vote_base_index.saturating_add(vote_index_in_slot);

                let (message, remote_pubkey) =
                    build_vote_message_and_remote(ctx, slot, block_id, global_vote_index);

                let arrival_us = slot_start_us + slot_rng.random_range(0..SLOT_WINDOW_US);

                slot_packets.push(consensus_message_to_stored_packet(
                    &message,
                    remote_pubkey,
                    StoredPacketKind::Vote,
                    slot_index,
                    arrival_us,
                ));
            }

            for _ in 0..config.certs_per_slot {
                let (message, remote_pubkey) = build_cert_message_and_remote(ctx, slot, block_id);

                let arrival_us = slot_start_us + slot_rng.random_range(0..SLOT_WINDOW_US);

                slot_packets.push(consensus_message_to_stored_packet(
                    &message,
                    remote_pubkey,
                    StoredPacketKind::Cert,
                    slot_index,
                    arrival_us,
                ));
            }

            // Report progress each time a 10% milestone is crossed. fetch_add
            // hands each completing slot a unique count, so a decile prints once.
            let done = completed_slots.fetch_add(1, Ordering::Relaxed) + 1;
            let decile = done * 10 / config.num_slots;
            if decile != (done - 1) * 10 / config.num_slots {
                eprintln!(
                    "  progress: {}% ({done}/{} slots)",
                    decile * 10,
                    config.num_slots
                );
            }

            slot_packets
        })
        .collect();

    let mut packets = per_slot_packets.into_iter().flatten().collect::<Vec<_>>();
    packets.sort_by_key(|packet| packet.arrival_us);

    StoredWorkload {
        seed: config.seed,
        num_slots: config.num_slots,
        votes_per_slot: config.votes_per_slot,
        certs_per_slot: config.certs_per_slot,
        base_slot: config.base_slot,
        slot_window_us: SLOT_WINDOW_US,
        cert_signers: cert_signers(config.num_validators),
        num_validators: config.num_validators,
        total_packets,
        vote_packets: config.num_slots.saturating_mul(config.votes_per_slot),
        cert_packets: config.num_slots.saturating_mul(config.certs_per_slot),
        packets,
    }
}

fn save_workload_to_file<P: AsRef<Path>>(
    workload: &StoredWorkload,
    path: P,
) -> Result<(), Box<dyn std::error::Error>> {
    let writer = BufWriter::new(File::create(path)?);
    bincode::serialize_into(writer, workload)?;
    Ok(())
}

fn main() {
    let config = FixtureBuildConfig::parse();

    validate_fixture_build_config(&config).unwrap_or_else(|err| {
        eprintln!("error: {err}");
        exit(1);
    });

    eprintln!("Preparing fixture data...");

    let max_slot = fixture_max_slot(config.base_slot, config.num_slots);
    let ctx = init_example_context(
        4,
        config.num_validators,
        config.seed,
        config.base_slot,
        max_slot,
    );

    eprintln!("Building stored workload...");

    let workload = build_stored_workload(&ctx, &config);

    eprintln!(
        "Writing fixture: output={}, slots={}, votes_per_slot={}, certs_per_slot={}, \
         total_packets={}",
        config.output,
        workload.num_slots,
        workload.votes_per_slot,
        workload.certs_per_slot,
        workload.total_packets,
    );

    save_workload_to_file(&workload, &config.output).unwrap_or_else(|err| {
        eprintln!("failed to save workload: {err}");
        exit(1);
    });

    eprintln!("Done.");
}
