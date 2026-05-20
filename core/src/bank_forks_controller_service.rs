use {
    agave_votor::root_utils,
    crossbeam_channel::RecvTimeoutError,
    solana_clock::Slot,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_rpc::{
        optimistically_confirmed_bank_tracker::BankNotificationSenderConfig,
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank_forks::BankForks,
        bank_forks_controller::{BankForksCommand, BankForksCommandReceiver},
        installed_scheduler_pool::BankWithScheduler,
        snapshot_controller::SnapshotController,
    },
    std::{
        collections::BTreeSet,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct BankForksControllerService {
    thread_hdl: JoinHandle<()>,
}

pub struct BankForksControllerServiceConfig {
    pub exit: Arc<AtomicBool>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub blockstore: Arc<Blockstore>,
    pub snapshot_controller: Option<Arc<SnapshotController>>,
    pub bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    pub drop_bank_sender: crossbeam_channel::Sender<Vec<BankWithScheduler>>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
}

impl BankForksControllerService {
    pub fn new(
        config: BankForksControllerServiceConfig,
        receiver: BankForksCommandReceiver,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solBankForksCtl".to_string())
            .spawn(move || {
                info!("BankForksControllerService has started");
                Self::run(config, receiver);
                info!("BankForksControllerService has stopped");
            })
            .unwrap();

        Self { thread_hdl }
    }

    fn run(config: BankForksControllerServiceConfig, receiver: BankForksCommandReceiver) {
        let BankForksControllerServiceConfig {
            exit,
            bank_forks,
            blockstore,
            snapshot_controller,
            bank_notification_sender,
            rpc_subscriptions,
            drop_bank_sender,
            leader_schedule_cache,
        } = config;
        let context = BankForksControllerServiceContext {
            bank_forks,
            blockstore,
            snapshot_controller,
            bank_notification_sender,
            rpc_subscriptions,
            drop_bank_sender,
            leader_schedule_cache,
        };

        while !exit.load(Ordering::Relaxed) {
            match receiver.receiver().recv_timeout(Duration::from_millis(100)) {
                Ok(command) => Self::process_command(command, &context),
                Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn process_command(command: BankForksCommand, context: &BankForksControllerServiceContext) {
        match command {
            BankForksCommand::InsertBank {
                bank,
                response_sender,
            } => {
                let bank = {
                    let mut bank_forks = context.bank_forks.write().unwrap();
                    if bank_forks.get(bank.slot()).is_none()
                        && bank_forks.get(bank.parent_slot()).is_some()
                    {
                        Some(bank_forks.insert(*bank))
                    } else {
                        None
                    }
                };

                response_sender.send(bank).unwrap_or_else(|_| {
                    warn!("bank forks controller insert-bank response receiver dropped")
                });
            }
            BankForksCommand::SetRoot {
                my_pubkey,
                parent_slot,
                new_root,
                highest_super_majority_root,
            } => {
                root_utils::check_and_handle_new_root(
                    parent_slot,
                    new_root,
                    context.snapshot_controller.as_deref(),
                    highest_super_majority_root,
                    &context.bank_notification_sender,
                    &context.drop_bank_sender,
                    &context.blockstore,
                    &context.leader_schedule_cache,
                    &context.bank_forks,
                    context.rpc_subscriptions.as_deref(),
                    &my_pubkey,
                    |_| {},
                );
            }
            BankForksCommand::ClearBanks {
                slots_to_clear,
                response_sender,
            } => {
                Self::clear_banks(slots_to_clear, &context.bank_forks);
                response_sender.send(()).unwrap_or_else(|_| {
                    warn!("bank forks controller clear-banks response receiver dropped")
                });
            }
        }
    }

    fn clear_banks(slots_to_clear: BTreeSet<Slot>, bank_forks: &RwLock<BankForks>) {
        if slots_to_clear.is_empty() {
            return;
        }

        let banks_to_remove = {
            let bank_forks = bank_forks.read().unwrap();
            slots_to_clear
                .iter()
                .filter_map(|slot| bank_forks.get_with_scheduler(*slot))
                .collect::<Vec<_>>()
        };

        for bank in banks_to_remove {
            let _ = bank.wait_for_completed_scheduler();
        }

        let (root_bank, slots_to_purge, removed_banks) = {
            let mut w_bank_forks = bank_forks.write().unwrap();
            let slots_to_clear = slots_to_clear
                .iter()
                .copied()
                .filter(|slot| w_bank_forks.get(*slot).is_some())
                .collect::<BTreeSet<_>>();
            if slots_to_clear.is_empty() {
                return;
            }

            let root_bank = w_bank_forks.root_bank();
            let (slots_to_purge, removed_banks) =
                w_bank_forks.dump_slots(slots_to_clear.iter(), true);
            (root_bank, slots_to_purge, removed_banks)
        };

        root_bank.remove_unrooted_slots(&slots_to_purge);
        drop(removed_banks);

        for (slot, _) in slots_to_purge {
            root_bank.clear_slot_signatures(slot);
            root_bank.prune_program_cache_by_deployment_slot(slot);
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

struct BankForksControllerServiceContext {
    bank_forks: Arc<RwLock<BankForks>>,
    blockstore: Arc<Blockstore>,
    snapshot_controller: Option<Arc<SnapshotController>>,
    bank_notification_sender: Option<BankNotificationSenderConfig>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    drop_bank_sender: crossbeam_channel::Sender<Vec<BankWithScheduler>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
}
