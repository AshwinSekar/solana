use {
    crate::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    crossbeam_channel::{Receiver, Sender, bounded},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    thiserror::Error,
};

const CHANNEL_SIZE: usize = 16;

#[derive(Debug, Error)]
pub enum BankForksControllerError {
    #[error("bank forks controller is disconnected")]
    Disconnected,
}

pub enum BankForksCommand {
    InsertBank {
        bank: Box<Bank>,
        response_sender: Sender<BankWithScheduler>,
    },
    SetRoot {
        my_pubkey: Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
        response_sender: Sender<()>,
    },
}

pub trait BankForksController: Send + Sync {
    fn insert_bank(&self, bank: Bank) -> Result<BankWithScheduler, BankForksControllerError>;

    fn set_root(
        &self,
        my_pubkey: Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
    ) -> Result<(), BankForksControllerError>;
}

/// Handle used by non-replay threads to serialize BankForks writes onto ReplayStage.
#[derive(Clone)]
pub struct BankForksControllerHandle {
    sender: Sender<BankForksCommand>,
}

impl BankForksControllerHandle {
    pub fn new() -> (Self, BankForksCommandReceiver) {
        let (sender, receiver) = bounded(CHANNEL_SIZE);
        (Self { sender }, BankForksCommandReceiver { receiver })
    }
}

impl BankForksController for BankForksControllerHandle {
    fn insert_bank(&self, bank: Bank) -> Result<BankWithScheduler, BankForksControllerError> {
        let (response_sender, response_receiver) = bounded(1);
        self.sender
            .send(BankForksCommand::InsertBank {
                bank: Box::new(bank),
                response_sender,
            })
            .map_err(|_| BankForksControllerError::Disconnected)?;
        response_receiver
            .recv()
            .map_err(|_| BankForksControllerError::Disconnected)
    }

    fn set_root(
        &self,
        my_pubkey: Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
    ) -> Result<(), BankForksControllerError> {
        let (response_sender, response_receiver) = bounded(1);
        self.sender
            .send(BankForksCommand::SetRoot {
                my_pubkey,
                parent_slot,
                new_root,
                highest_super_majority_root,
                response_sender,
            })
            .map_err(|_| BankForksControllerError::Disconnected)?;
        response_receiver
            .recv()
            .map_err(|_| BankForksControllerError::Disconnected)
    }
}

pub struct BankForksCommandReceiver {
    receiver: Receiver<BankForksCommand>,
}

impl BankForksCommandReceiver {
    pub fn receiver(&self) -> &Receiver<BankForksCommand> {
        &self.receiver
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{bank::SlotLeader, bank_forks::BankForks, genesis_utils::create_genesis_config},
        std::thread,
    };

    #[test]
    fn test_bank_forks_controller_insert_and_set_root() {
        let genesis = create_genesis_config(10_000);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
        let (controller, receiver) = BankForksControllerHandle::new();
        let replay_bank_forks = bank_forks.clone();
        let replay_thread = thread::spawn(move || {
            loop {
                let Ok(command) = receiver.receiver().recv() else {
                    break;
                };
                match command {
                    BankForksCommand::InsertBank {
                        bank,
                        response_sender,
                    } => {
                        let bank = {
                            let mut bank_forks = replay_bank_forks.write().unwrap();
                            bank_forks.insert(*bank)
                        };
                        response_sender.send(bank).unwrap();
                    }
                    BankForksCommand::SetRoot {
                        new_root,
                        response_sender,
                        ..
                    } => {
                        {
                            let mut bank_forks = replay_bank_forks.write().unwrap();
                            bank_forks.set_root(new_root, None, None);
                        }
                        response_sender.send(()).unwrap();
                    }
                }
            }
        });

        let parent_bank = bank_forks.read().unwrap().root_bank();
        let bank = Bank::new_from_parent(parent_bank, SlotLeader::default(), 1);
        bank.freeze();
        let inserted_bank = controller.insert_bank(bank).unwrap();
        assert_eq!(inserted_bank.slot(), 1);
        assert!(bank_forks.read().unwrap().get(1).is_some());

        controller.set_root(Pubkey::default(), 1, 1, None).unwrap();
        assert_eq!(bank_forks.read().unwrap().root(), 1);

        drop(controller);
        replay_thread.join().unwrap();
    }
}
