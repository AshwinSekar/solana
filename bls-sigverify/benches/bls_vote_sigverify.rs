/*
    To run this benchmark:
    `cargo bench --bench bls_vote_sigverify`
*/

use {
    agave_bls_sigverify::bls_vote_sigverify::{UnverifiedVoteGroup, verify_individual_votes},
    agave_votor_messages::{
        consensus_message::Block,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::{Keypair as BLSKeypair, PreparedHashedMessage, VerifySignature},
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_runtime::{
        bank::Bank,
        epoch_stakes::BLSPubkeyToRankMap,
        genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
    },
    solana_signer::Signer,
    std::{hint::black_box, sync::Arc},
};

static BATCH_SIZES: &[usize] = &[8, 16, 32, 64, 128];

fn get_thread_pool() -> ThreadPool {
    let num_threads = 4;
    ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .unwrap()
}

fn generate_test_data(
    shred_version: u16,
    batch_size: usize,
) -> (
    VotePayloadToSign,
    UnverifiedVoteGroup,
    Arc<BLSPubkeyToRankMap>,
) {
    // Pre-calculate the payloads to ensure exact distinctness
    let slot = 100;
    let vote = Vote::new_notarization_vote(Block {
        slot,
        block_id: Hash::new_unique(),
    });
    let payload = get_vote_payload_to_sign(vote, shred_version);
    let validator_keypairs = (0..batch_size)
        .map(|_| ValidatorVoteKeypairs::new_rand())
        .collect::<Vec<_>>();
    let stakes = (1..=batch_size)
        .rev()
        .map(|stake| u64::try_from(stake).unwrap())
        .collect();
    let mut genesis = create_genesis_config_with_alpenglow_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        stakes,
    );
    genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
    let bank = Bank::new_for_tests(&genesis.genesis_config);
    let rank_map = bank.get_rank_map(slot).unwrap().clone();
    let mut group = UnverifiedVoteGroup::default();
    for validator in validator_keypairs {
        let rank = rank_map
            .get_ranked_entry_for_node(&validator.node_keypair.pubkey())
            .unwrap()
            .0;
        group.push(validator.bls_keypair.sign(&payload).into(), rank);
    }
    (
        VotePayloadToSign::new_from_vote(vote, shred_version),
        group,
        rank_map,
    )
}

// Single Signature Verification
// This is just for reference
fn bench_verify_single_signature(c: &mut Criterion) {
    let mut group = c.benchmark_group("verify_single_signature");

    let keypair = BLSKeypair::new();
    let msg = b"benchmark_message_payload";
    let sig = keypair.sign(msg);
    let pubkey = keypair.public;

    group.bench_function("1_item", |b| {
        b.iter(|| {
            // We use the raw verify method from the underlying library
            // to establish the cryptographic floor.
            let res = pubkey.verify_signature(black_box(&sig), black_box(msg));
            black_box(res).unwrap();
        })
    });
    group.finish();
}

fn bench_verify_single_signature_with_prepared_message(c: &mut Criterion) {
    let mut group = c.benchmark_group("verify_single_signature_with_prepared_message");

    let keypair = BLSKeypair::new();
    let msg = b"benchmark_message_payload";
    let sig = keypair.sign(msg);
    let pubkey = keypair.public;
    let prepared_msg = PreparedHashedMessage::new(msg);

    group.bench_function("1_item", |b| {
        b.iter(|| {
            let res = pubkey.verify_signature_prepared(black_box(&sig), black_box(&prepared_msg));
            black_box(res).unwrap();
        })
    });
    group.finish();
}

// Individual Verification - verifies each signature in parallel threads
// Message distinctness is irrelevant.
fn bench_verify_individual_votes(c: &mut Criterion) {
    let shred_version = 134;
    let mut group = c.benchmark_group("verify_votes_fallback");
    let thread_pool = get_thread_pool();

    for &batch_size in BATCH_SIZES {
        // Distinctness doesn't affect the cost of N individual verifications.
        let (vote_payload_to_sign, unverified_votes, rank_map) =
            generate_test_data(shred_version, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter_batched(
                || {
                    let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
                    let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
                    (unverified_votes.clone(), prepared_hash_msg)
                },
                |(votes, prepared_hash_map)| {
                    let res = verify_individual_votes(
                        &rank_map,
                        vote_payload_to_sign,
                        black_box(votes),
                        black_box(prepared_hash_map),
                        &thread_pool,
                    );
                    black_box(res);
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_verify_single_signature,
    bench_verify_single_signature_with_prepared_message,
    bench_verify_individual_votes
);
criterion_main!(benches);
