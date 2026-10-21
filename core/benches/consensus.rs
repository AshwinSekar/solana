use {
    criterion::{Criterion, criterion_group, criterion_main},
    solana_core::consensus::{Tower, tower_storage::FileTowerStorage},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    std::{sync::Arc, time::Duration},
    tempfile::TempDir,
};

fn bench_save_tower(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();

    let vote_account_pubkey = &Pubkey::default();
    let node_keypair = Arc::new(Keypair::new());
    let heaviest_bank = BankForks::new_rw_arc(Bank::default_for_tests())
        .read()
        .unwrap()
        .working_bank();
    let tower_storage = FileTowerStorage::new(dir.path().to_path_buf());
    let tower = Tower::new(
        &node_keypair.pubkey(),
        vote_account_pubkey,
        0,
        &heaviest_bank,
    );

    c.bench_function("bench_save_tower", |b| {
        b.iter(|| {
            tower.save(&tower_storage, &node_keypair).unwrap();
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(750))
        .sample_size(10)
        .without_plots();
    targets = bench_save_tower
}
criterion_main!(benches);
