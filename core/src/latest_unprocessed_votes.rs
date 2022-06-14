use {
    crate::unprocessed_packet_batches::DeserializedPacket,
    rand::thread_rng,
    solana_runtime::bank::Bank,
    solana_sdk::{clock::Slot, pubkey::Pubkey},
    std::{
        cell::RefCell,
        collections::{BinaryHeap, HashMap},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, RwLock,
        },
    },
};

#[derive(Debug, Default)]
pub struct LatestUnprocessedVotes {
    latest_votes_per_pubkey:
        RwLock<HashMap<Pubkey, RwLock<RefCell<(u64, Option<DeserializedPacket>)>>>>,
    size: AtomicUsize,
}

unsafe impl Sync for LatestUnprocessedVotes {}

impl LatestUnprocessedVotes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> usize {
        self.len() == 0
    }

    /// If this vote causes an old vote to be removed, returns Some(old_vote)
    /// If there is a newer vote already present returns Some(vote)
    /// Otherwise returns None
    pub fn update_vote(
        &self,
        pubkey: Pubkey,
        slot: Slot,
        vote: DeserializedPacket,
    ) -> Option<DeserializedPacket> {
        if let Some(latest_vote) = self.latest_votes_per_pubkey.read().unwrap().get(&pubkey) {
            let mut latest_slot = 0;
            {
                if let Some(latest) = latest_vote
                    .read()
                    .ok()
                    .and_then(|v| v.try_borrow().ok().map(|v| v.0))
                {
                    latest_slot = latest;
                }
            }
            if slot > latest_slot {
                if let Ok(latest_vote) = latest_vote.write() {
                    // At this point no one should have a borrow to this refcell as all borrows are
                    // hidden behind previous read()
                    if let Ok(mut latest_vote) = latest_vote.try_borrow_mut() {
                        let latest_slot = latest_vote.0;
                        if slot > latest_slot {
                            if latest_vote.1.is_none() {
                                self.size.fetch_add(1, Ordering::AcqRel);
                            }
                            let ret = std::mem::take(&mut latest_vote.1);
                            *latest_vote = (slot, Some(vote));
                            return ret;
                        }
                    } else {
                        error!("Implementation error {} {} {:?}", slot, latest_slot, self);
                    }
                }
            }
            return Some(vote);
        }

        // Should have low lock contention because this is only hit on the first few blocks of startup
        // and when a new validator starts voting.
        let mut latest_votes_per_pubkey = self.latest_votes_per_pubkey.write().unwrap();
        latest_votes_per_pubkey.insert(pubkey, RwLock::new(RefCell::new((slot, Some(vote)))));
        self.size.fetch_add(1, Ordering::AcqRel);
        None
    }

    pub fn get_latest_vote_slot(&self, pubkey: Pubkey) -> Option<Slot> {
        self.latest_votes_per_pubkey
            .read()
            .ok()
            .and_then(|latest_votes_per_pubkey| {
                latest_votes_per_pubkey
                    .get(&pubkey)
                    .and_then(|l| l.read().ok())
                    .and_then(|c| c.try_borrow().ok().map(|v| (*v).0))
            })
    }

    /// Based on the stake distribution present in the supplied bank, drain the unprocessed votes
    /// from each validator using a weighted random sample based on stake.
    pub fn drain_unprocessed_votes_by_stake(
        &self,
        bank: &Arc<Bank>,
        chunk_size: usize,
    ) -> impl Iterator<Item = DeserializedPacket> {
        // Efraimidis and Spirakis algo for weighted random sample without replacement
        let sorted_pubkeys: BinaryHeap<_> = bank
            .staked_nodes()
            .iter()
            .map(|(pubkey, stake)| (thread_rng.gen::<f64>().powf(1.0 / stake), pubkey))
            .collect();
        let latest_votes_per_pubkey = self.latest_votes_per_pubkey.read().unwrap();
        sorted_pubkeys
            .into_iter_sorted()
            .filter_map(|(_, pubkey)| {
                if let Some(lock) = latest_votes_per_pubkey.get(&pubkey) {
                    if let Ok(latest_vote) = lock.write() {
                        if let Ok(mut latest_vote) = latest_vote.try_borrow_mut() {
                            let deserialized_packet = latest_vote.1;
                            *latest_vote.1 = None;
                            self.size.fetch_sub(1, Ordering::AcqRel);
                            return deserialized_packet;
                        }
                    }
                }
                None
            })
            .chunks(chunk_size)
            .into_iter();
    }
}
