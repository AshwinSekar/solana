use {
    crate::root_utils,
    crossbeam_channel::Sender,
    solana_clock::Slot,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::BankNotificationSenderConfig,
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        bank_forks_controller::{BankForksController, BankForksControllerError},
        installed_scheduler_pool::BankWithScheduler,
        snapshot_controller::SnapshotController,
    },
    std::sync::{Arc, RwLock},
};

pub struct DirectBankForksController {
    bank_forks: Arc<RwLock<BankForks>>,
    root_context: Option<DirectBankForksControllerRootContext>,
}

pub struct DirectBankForksControllerRootContext {
    pub blockstore: Arc<Blockstore>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub drop_bank_sender: Sender<Vec<BankWithScheduler>>,
    pub snapshot_controller: Option<Arc<SnapshotController>>,
    pub bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
}

impl DirectBankForksController {
    pub fn new(bank_forks: Arc<RwLock<BankForks>>) -> Self {
        Self {
            bank_forks,
            root_context: None,
        }
    }

    pub fn new_with_root_context(
        bank_forks: Arc<RwLock<BankForks>>,
        root_context: DirectBankForksControllerRootContext,
    ) -> Self {
        Self {
            bank_forks,
            root_context: Some(root_context),
        }
    }

    pub fn new_shared(bank_forks: Arc<RwLock<BankForks>>) -> Arc<dyn BankForksController> {
        Arc::new(Self::new(bank_forks))
    }

    pub fn new_shared_with_root_context(
        bank_forks: Arc<RwLock<BankForks>>,
        root_context: DirectBankForksControllerRootContext,
    ) -> Arc<dyn BankForksController> {
        Arc::new(Self::new_with_root_context(bank_forks, root_context))
    }
}

impl BankForksController for DirectBankForksController {
    fn insert_bank(&self, bank: Bank) -> Result<BankWithScheduler, BankForksControllerError> {
        let slot = bank.slot();
        let mut bank_forks = self.bank_forks.write().unwrap();
        if bank_forks.get(slot).is_none() && bank_forks.get(bank.parent_slot()).is_some() {
            Ok(bank_forks.insert(bank))
        } else {
            Err(BankForksControllerError::UnableToInsertStaleBank(slot))
        }
    }

    fn set_root(
        &self,
        my_pubkey: Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        highest_super_majority_root: Option<Slot>,
    ) -> Result<(), BankForksControllerError> {
        if let Some(root_context) = &self.root_context {
            root_utils::check_and_handle_new_root(
                parent_slot,
                new_root,
                root_context.snapshot_controller.as_deref(),
                highest_super_majority_root,
                &root_context.bank_notification_sender,
                &root_context.drop_bank_sender,
                &root_context.blockstore,
                &root_context.leader_schedule_cache,
                &self.bank_forks,
                root_context.rpc_subscriptions.as_deref(),
                &my_pubkey,
                |_| {},
            );
        } else {
            self.bank_forks
                .write()
                .unwrap()
                .set_root(new_root, None, highest_super_majority_root);
        }
        Ok(())
    }

    fn clear_bank(&self, slot: Slot) -> Result<(), BankForksControllerError> {
        let bank_to_clear = self.bank_forks.read().unwrap().get_with_scheduler(slot);
        if let Some(bank) = bank_to_clear {
            let _ = bank.wait_for_completed_scheduler();
        }
        self.bank_forks.write().unwrap().clear_bank(slot, false);
        Ok(())
    }
}
