use crate::consensus::{SwitchForkDecision, Tower, TowerError, TowerVersions};
use crate::tower_storage::{SavedTowerVersion, TowerStorage};
use solana_sdk::{
    clock::Slot,
    hash::Hash,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
};
use solana_vote_program::vote_state::{BlockTimestamp, Vote, VoteState};
use std::fs::File;

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
    pub fn save(
        &self,
        tower_storage: &dyn TowerStorage,
        node_keypair: &Keypair,
    ) -> Result<(), TowerError> {
        let saved_tower = SavedTower1_7_14::new(self, node_keypair)?;
        tower_storage.store(&saved_tower)?;
        Ok(())
    }
}

#[frozen_abi(digest = "Gaxfwvx5MArn52mKZQgzHmDCyn5YfCuTHvp5Et3rFfpp")]
#[derive(Default, Clone, Serialize, Deserialize, Debug, PartialEq, AbiExample)]
pub struct SavedTower1_7_14 {
    pub(crate) signature: Signature,
    pub(crate) data: Vec<u8>,
    #[serde(skip)]
    pub(crate) node_pubkey: Pubkey,
}

impl SavedTower1_7_14 {
    pub fn new<T: Signer>(tower: &Tower1_7_14, keypair: &T) -> Result<Self, TowerError> {
        let node_pubkey = keypair.pubkey();
        if tower.node_pubkey != node_pubkey {
            return Err(TowerError::WrongTower(format!(
                "node_pubkey is {:?} but found tower for {:?}",
                node_pubkey, tower.node_pubkey
            )));
        }

        let data = bincode::serialize(tower)?;
        let signature = keypair.sign_message(&data);
        Ok(Self {
            signature,
            data,
            node_pubkey,
        })
    }
}

#[typetag::serde]
impl SavedTowerVersion for SavedTower1_7_14 {
    fn try_into_tower(&self, node_pubkey: &Pubkey) -> Result<Tower, TowerError> {
        // This method assumes that `self` was just deserialized
        assert_eq!(self.node_pubkey, Pubkey::default());

        if !self.signature.verify(node_pubkey.as_ref(), &self.data) {
            return Err(TowerError::InvalidSignature);
        }
        bincode::deserialize(&self.data)
            .map(TowerVersions::V1_17_14)
            .map_err(|e| e.into())
            .and_then(|tv: TowerVersions| {
                let tower = tv.convert_to_current();
                if tower.node_pubkey != *node_pubkey {
                    return Err(TowerError::WrongTower(format!(
                        "node_pubkey is {:?} but found tower for {:?}",
                        node_pubkey, tower.node_pubkey
                    )));
                }
                Ok(tower)
            })
    }

    fn serialize_into(&self, file: &mut File) -> Result<(), TowerError> {
        bincode::serialize_into(file, self).map_err(|e| e.into())
    }

    fn pubkey(&self) -> Pubkey {
        self.node_pubkey
    }
}
