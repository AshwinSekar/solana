use crate::consensus::{SwitchForkDecision, TowerError};
use crate::tower_storage::{TowerStorage, SavedTower};
use solana_sdk::{pubkey::Pubkey, hash::Hash, clock::Slot, signature::Keypair};
use solana_vote_program::vote_state::{VoteState, Vote, BlockTimestamp};

#[frozen_abi(digest = "GMs1FxKteU7K4ZFRofMBqNhBpM4xkPVxfYod6R8DQmpT")]
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, AbiExample)]
pub struct Tower1_7_14 {
    pub(crate) node_pubkey: Pubkey,
    pub(crate) threshold_depth: usize,
    pub(crate) threshold_size: f64,
    pub(crate) vote_state: VoteState,
    pub(crate) last_vote: Vote,
    #[serde(skip)]
    // The blockhash used in the last vote transaction, may or may not equal the
    // blockhash of the voted block itself, depending if the vote slot was refreshed.
    // For instance, a vote for slot 5, may be refreshed/resubmitted for inclusion in
    //  block 10, in  which case `last_vote_tx_blockhash` equals the blockhash of 10, not 5.
    pub(crate) last_vote_tx_blockhash: Hash,
    pub(crate) last_timestamp: BlockTimestamp,
    #[serde(skip)]
    // Restored last voted slot which cannot be found in SlotHistory at replayed root
    // (This is a special field for slashing-free validator restart with edge cases).
    // This could be emptied after some time; but left intact indefinitely for easier
    // implementation
    // Further, stray slot can be stale or not. `Stale` here means whether given
    // bank_forks (=~ ledger) lacks the slot or not.
    pub(crate) stray_restored_slot: Option<Slot>,
    #[serde(skip)]
    pub(crate) last_switch_threshold_check: Option<(Slot, SwitchForkDecision)>,
}

impl Tower1_7_14 {
    pub fn save(&self, tower_storage: &dyn TowerStorage, node_keypair: &Keypair) -> Result<(), TowerError> {
        let saved_tower = SavedTower::new1_7_14(self, node_keypair)?;
        tower_storage.store(&saved_tower)?;
        Ok(())
    }
}
