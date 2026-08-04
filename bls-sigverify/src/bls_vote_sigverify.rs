#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
#[cfg(debug_assertions)]
use std::collections::HashSet;
use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, SigVerifierChannels},
        errors::SigVerifyVoteError,
        rewards::rewards_wants_vote,
        stats::SigVerifyVoteStats,
        utils::{
            send_sig_verified_batch_to_pool, send_votes_to_metrics, send_votes_to_repair,
            send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        consensus_message::VoteMessage,
        metric_types::ConsensusMetricsEvent,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        vote::Vote,
        wire::VotePayloadToSign,
    },
    log::info,
    rayon::{
        ThreadPool, current_thread_index,
        iter::{
            Either, IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
            ParallelIterator,
        },
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage, PubkeyProjective, Signature as BLSSignature,
        SignatureProjective,
        pubkey::{PopVerified, VerifySignature},
    },
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        epoch_stakes::{BLSPubkeyStakeEntry, BLSPubkeyToRankMap},
    },
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{collections::HashMap, sync::Arc},
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
struct VerifiedVotePayload {
    vote_aggregate: VoteAggregate,
    sender_vote_account_pubkeys: Vec<Pubkey>,
}

/// Signatures and validator ranks for one distinct vote payload.
///
/// All validator metadata is resolved from the epoch rank map during verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug, Default)]
pub(super) struct UnverifiedVoteGroup {
    signatures: Vec<BLSSignature>,
    ranks: Vec<u16>,
}

impl UnverifiedVoteGroup {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(super) fn push(&mut self, signature: BLSSignature, rank: u16) {
        self.signatures.push(signature);
        self.ranks.push(rank);
    }

    fn len(&self) -> usize {
        debug_assert_eq!(self.signatures.len(), self.ranks.len());
        self.signatures.len()
    }
}

#[inline]
fn get_entry_for_rank(rank_map: &BLSPubkeyToRankMap, rank: u16) -> &BLSPubkeyStakeEntry {
    rank_map
        .get_pubkey_stake_entry(usize::from(rank))
        .expect("vote group rank must exist in the epoch rank map")
}

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Any vote that fails fallback individual signature verification will have its sender banlisted.
pub(super) fn verify_and_send_votes(
    unverified_votes: HashMap<VotePayloadToSign, UnverifiedVoteGroup>,
    rank_map_cache: &HashMap<Epoch, Arc<BLSPubkeyToRankMap>>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
    channels: &SigVerifierChannels,
) -> Result<SigVerifyVoteStats, SigVerifyVoteError> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = SigVerifyVoteStats::default();
    if unverified_votes.is_empty() {
        return Ok(stats);
    }
    stats
        .distinct_votes_stats
        .add_sample(unverified_votes.len() as u64);

    for (vote_payload_to_sign, unverified_votes) in unverified_votes {
        stats.votes_to_sig_verify += unverified_votes.len() as u64;
        let vote_slot = vote_payload_to_sign.slot();
        let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
        let rank_map = rank_map_cache.get(&vote_epoch).unwrap();
        let verified_votes = verify_votes(
            rank_map,
            vote_payload_to_sign,
            unverified_votes,
            &mut stats,
            banlist,
            thread_pool,
        );

        let (sig_verified_batch, msgs_for_repair, msg_for_reward, msg_for_metrics) =
            process_verified_votes(verified_votes, root_bank, cluster_info, leader_schedule);

        send_sig_verified_batch_to_pool(sig_verified_batch, &channels.channel_to_pool, &mut stats)?;
        send_votes_to_repair(msgs_for_repair, &channels.channel_to_repair, &mut stats)?;
        send_votes_to_rewards(msg_for_reward, &channels.channel_to_reward, &mut stats)?;
        send_votes_to_metrics(msg_for_metrics, &channels.channel_to_metrics, &mut stats)?;
    }

    measure.stop();
    stats
        .fn_verify_and_send_votes_stats
        .add_sample(measure.as_us());
    Ok(stats)
}

/// If the vote is relevant to repair, then adds it to the [`msgs_for_repair`] so it can eventually
/// be sent to repair.
fn inspect_for_repair(
    vote: &VerifiedVotePayload,
    msgs_for_repair: &mut HashMap<Pubkey, Vec<Slot>>,
) {
    let vote_slot = vote.vote_aggregate.vote().slot();
    match vote.vote_aggregate.vote() {
        Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
            for pubkey in &vote.sender_vote_account_pubkeys {
                msgs_for_repair.entry(*pubkey).or_default().push(vote_slot);
            }
        }
        Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
    }
}

/// Processes the verified votes for various downstream services.
///
/// In particular, collects and returns the relevant messages for the consensus pool; rewards;
/// repair; and metrics;
fn process_verified_votes(
    verified_votes: Vec<VerifiedVotePayload>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
) -> (
    SigVerifiedBatch,
    HashMap<Pubkey, Vec<Slot>>,
    Vec<VoteAggregate>,
    Vec<ConsensusMetricsEvent>,
) {
    let mut votes_for_reward = Vec::with_capacity(verified_votes.len());
    let mut msgs_for_repair = HashMap::new();
    let mut vote_aggregates_for_pool = Vec::with_capacity(verified_votes.len());
    let mut votes_for_metrics = Vec::with_capacity(verified_votes.len());
    for payload in verified_votes {
        inspect_for_repair(&payload, &mut msgs_for_repair);

        for pubkey in &payload.sender_vote_account_pubkeys {
            votes_for_metrics.push(ConsensusMetricsEvent::Vote {
                id: *pubkey,
                vote: *payload.vote_aggregate.vote(),
            });
        }
        if rewards_wants_vote(
            cluster_info,
            leader_schedule,
            root_bank.slot(),
            payload.vote_aggregate.vote(),
        ) {
            votes_for_reward.push(payload.vote_aggregate.clone());
        }
        vote_aggregates_for_pool.push(payload.vote_aggregate);
    }
    let msgs_for_repair = msgs_for_repair
        .into_iter()
        .map(|(pubkey, mut slots)| {
            slots.sort_unstable();
            slots.dedup();
            (pubkey, slots)
        })
        .collect();
    let sig_verified_batch = SigVerifiedBatch::Votes(vote_aggregates_for_pool);
    (
        sig_verified_batch,
        msgs_for_repair,
        votes_for_reward,
        votes_for_metrics,
    )
}

/// Sig verifies `unverified_votes` and returns a `Vec` of votes that passed verification.
fn verify_votes(
    rank_map: &BLSPubkeyToRankMap,
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: UnverifiedVoteGroup,
    stats: &mut SigVerifyVoteStats,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
) -> Vec<VerifiedVotePayload> {
    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    let res = verify_votes_optimistic(
        rank_map,
        vote_payload_to_sign,
        &unverified_votes,
        stats,
        thread_pool,
    );

    match res {
        Either::Left(signature) => {
            stats.optimistic_verification_succeeded += 1;
            stats
                .optimistic_batch
                .add_sample(unverified_votes.len() as u64);
            let vote_aggregate = VoteAggregate::new_from_verified_votes(
                rank_map.len(),
                vote_payload_to_sign,
                unverified_votes.ranks.iter().map(|&rank| {
                    let entry = get_entry_for_rank(rank_map, rank);
                    (rank, entry.stake)
                }),
                signature,
            );
            let sender_vote_account_pubkeys = unverified_votes
                .ranks
                .iter()
                .map(|&rank| get_entry_for_rank(rank_map, rank).vote_account_pubkey)
                .collect();
            vec![VerifiedVotePayload {
                vote_aggregate,
                sender_vote_account_pubkeys,
            }]
        }
        Either::Right(prepared_hash_msg) => {
            // Fallback to individual verification
            stats.optimistic_verification_failed += 1;
            let ((verified_votes, invalid_remote_pubkeys), time_us) =
                measure_us!(verify_individual_votes(
                    rank_map,
                    vote_payload_to_sign,
                    unverified_votes,
                    prepared_hash_msg,
                    thread_pool
                ));
            stats.num_individual_verified += verified_votes.len() as u64;
            for (sender_identity_pubkey, error) in invalid_remote_pubkeys {
                stats.banning_validator += 1;
                if banlist.ban(sender_identity_pubkey, BAN_TIMEOUT) {
                    stats.already_banned += 1;
                } else {
                    info!(
                        "bls_vote_sigverify: banned sender={sender_identity_pubkey} due to failed \
                         verification {error:?}"
                    );
                }
            }
            stats.fn_verify_individual_votes_stats.add_sample(time_us);
            verified_votes
        }
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
/// Attempts aggregate BLS verification across the full vote set.
///
/// This fast path aggregates all vote signatures and public keys for the shared
/// payload, minimizing the number of pairing operations needed for verification.
/// When aggregation or aggregate verification fails, the caller falls back to
/// individual vote verification so invalid votes can be identified precisely.
///
/// On success, returns the aggregate signature. On failure, returns the prepared
/// message so the fallback path can reuse it.
#[must_use]
fn verify_votes_optimistic(
    rank_map: &BLSPubkeyToRankMap,
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: &UnverifiedVoteGroup,
    stats: &mut SigVerifyVoteStats,
    thread_pool: &ThreadPool,
) -> Either<SignatureProjective, PreparedHashedMessage> {
    #[cfg(debug_assertions)]
    {
        let deduped = unverified_votes
            .ranks
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        assert_eq!(deduped.len(), unverified_votes.len());
    }

    let mut measure = Measure::start("verify_votes_optimistic");

    // For BLS verification, minimizing the expensive pairing operation is key.
    // Each BLS signature verification requires two pairings.
    //
    // However, the BLS verification formula allows us to:
    // 1. Aggregate all signatures into a single signature.
    // 2. Aggregate the public keys for the shared message.
    //
    // Verifying the aggregate signature against the aggregate public key requires
    // only two pairings for the group.
    let (signature_result, (prepared_hash_msg, pubkey_result)) = thread_pool.join(
        || aggregate_signatures(unverified_votes),
        || aggregate_pubkeys_by_payload(rank_map, vote_payload_to_sign, unverified_votes),
    );

    let Ok(aggregate_signature) = signature_result else {
        return Either::Right(prepared_hash_msg);
    };

    let Ok(aggregate_pubkey) = pubkey_result else {
        return Either::Right(prepared_hash_msg);
    };

    let verified = aggregate_pubkey
        .verify_signature_prepared(&aggregate_signature, &prepared_hash_msg)
        .is_ok();

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .add_sample(measure.as_us());
    if verified {
        Either::Left(aggregate_signature)
    } else {
        Either::Right(prepared_hash_msg)
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_signatures(votes: &UnverifiedVoteGroup) -> Result<SignatureProjective, BlsError> {
    debug_assert!(current_thread_index().is_some());
    let signatures = votes.signatures.par_iter();
    // TODO(sam): Currently, `par_aggregate` performs full validation
    // (on-curve + subgroup check) for every signature. Since the subgroup
    // check is expensive, we can use an `unchecked` deserialization here
    // (performing only the cheap on-curve check) and rely on a single subgroup
    // check on the final aggregated signature. This should save more than 80%
    // of the time for signature aggregation.
    SignatureProjective::par_aggregate(signatures)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_pubkeys_by_payload(
    rank_map: &BLSPubkeyToRankMap,
    vote_payload_to_sign: VotePayloadToSign,
    votes: &UnverifiedVoteGroup,
) -> (
    PreparedHashedMessage,
    Result<PopVerified<PubkeyProjective>, BlsError>,
) {
    debug_assert!(current_thread_index().is_some());
    let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
    let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
    // converting aggregate pubkey to `PopVerified` is safe here
    // since the pubkeys are all PoP verified in the vote account
    let pubkey = PubkeyProjective::par_aggregate(
        votes
            .ranks
            .par_iter()
            .map(|&rank| &get_entry_for_rank(rank_map, rank).bls_pubkey),
    )
    .map(|agg| unsafe { PopVerified::new_unchecked(*agg) });
    (prepared_hash_msg, pubkey)
}

/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - verified vote payloads for votes that passed verification.
/// - canonical node pubkeys and errors for votes that failed verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    rank_map: &BLSPubkeyToRankMap,
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: UnverifiedVoteGroup,
    prepared_hash_msg: PreparedHashedMessage,
    thread_pool: &ThreadPool,
) -> (Vec<VerifiedVotePayload>, Vec<(Pubkey, BlsError)>) {
    debug_assert_eq!(
        unverified_votes.signatures.len(),
        unverified_votes.ranks.len()
    );
    let UnverifiedVoteGroup { signatures, ranks } = unverified_votes;
    thread_pool.install(|| {
        signatures
            .into_par_iter()
            .zip(ranks.into_par_iter())
            .partition_map(|(signature, rank)| {
                let entry = get_entry_for_rank(rank_map, rank);
                match entry
                    .bls_pubkey
                    .verify_signature_prepared(&signature, &prepared_hash_msg)
                {
                    Ok(()) => {
                        let vote_message = VoteMessage {
                            vote: vote_payload_to_sign.into(),
                            signature,
                            rank,
                            stake: entry.stake,
                        };
                        Either::Left(VerifiedVotePayload {
                            vote_aggregate: VoteAggregate::new_from_verified_vote(
                                rank_map.len(),
                                vote_message,
                            ),
                            sender_vote_account_pubkeys: vec![entry.vote_account_pubkey],
                        })
                    }
                    Err(error) => Either::Right((entry.node_pubkey, error)),
                }
            })
    })
}

#[cfg(test)]
mod tests {
    use {super::*, solana_bls_signatures::BLS_SIGNATURE_AFFINE_SIZE, std::mem::size_of};

    #[test]
    fn test_unverified_vote_group_preserves_signature_rank_pairs() {
        let expected = [
            (BLSSignature([1; BLS_SIGNATURE_AFFINE_SIZE]), 7),
            (BLSSignature([2; BLS_SIGNATURE_AFFINE_SIZE]), 3),
            (BLSSignature([3; BLS_SIGNATURE_AFFINE_SIZE]), 11),
        ];
        let mut group = UnverifiedVoteGroup::default();
        for &(signature, rank) in &expected {
            group.push(signature, rank);
        }

        assert_eq!(group.len(), expected.len());
        assert_eq!(group.signatures.len(), group.ranks.len());
        assert_eq!(
            group
                .signatures
                .iter()
                .copied()
                .zip(group.ranks.iter().copied())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            size_of::<BLSSignature>() + size_of::<u16>(),
            BLS_SIGNATURE_AFFINE_SIZE + size_of::<u16>()
        );
        assert_eq!(BLS_SIGNATURE_AFFINE_SIZE + size_of::<u16>(), 194);
    }
}
