use ic_ledger_canister_blocks_synchronizer::{
    balance_book::BalanceBook,
    store::{BlockStoreError, SQLiteStore},
};
use ic_ledger_canister_blocks_synchronizer_test_utils::{
    create_tmp_dir, init_test_logger, sample_data::Scribe,
};
use ic_ledger_canister_core::ledger::LedgerTransaction;
use ic_ledger_core::{block::BlockType, Tokens};
use icp_ledger::{AccountIdentifier, Block, BlockIndex};
use rusqlite::params;
use std::{collections::BTreeMap, path::Path};
pub(crate) fn sqlite_on_disk_store(path: &Path) -> SQLiteStore {
    SQLiteStore::new_on_disk(path).expect("Unable to create store")
}

#[actix_rt::test]
async fn store_smoke_test() {
    init_test_logger();
    let tmpdir = create_tmp_dir();
    let store = sqlite_on_disk_store(tmpdir.path());
    let scribe = Scribe::new_with_sample_data(10, 100);

    for hb in &scribe.blockchain {
        store.push(hb).unwrap();
    }

    for hb in &scribe.blockchain {
        assert_eq!(store.get_at(hb.index).unwrap(), *hb);
        let block = hb.block.clone();
        assert_eq!(
            store.get_transaction(&hb.index).unwrap(),
            Block::decode(block).unwrap().transaction
        );
    }

    let last_idx = scribe.blockchain.back().unwrap().index;
    assert_eq!(
        store.get_at(last_idx + 1).unwrap_err(),
        BlockStoreError::NotFound(last_idx + 1)
    );
}

#[actix_rt::test]
async fn store_coherance_test() {
    init_test_logger();
    let tmpdir = create_tmp_dir();

    let location = tmpdir.path();

    let store = sqlite_on_disk_store(location);
    let scribe = Scribe::new_with_sample_data(10, 100);
    let path = location.join("db.sqlite");
    let con = rusqlite::Connection::open(path).unwrap();
    for hb in &scribe.blockchain {
        let hash = hb.hash.into_bytes().to_vec();
        let parent_hash = hb.parent_hash.map(|ph| ph.into_bytes().to_vec());
        let command = "INSERT INTO blocks (hash, block, parent_hash, idx, verified) VALUES (?1, ?2, ?3, ?4, FALSE)";
        con.execute(
            command,
            params![hash, hb.block.clone().into_vec(), parent_hash, hb.index],
        )
        .unwrap();
    }
    drop(con);
    for hb in &scribe.blockchain {
        assert_eq!(store.get_at(hb.index).unwrap(), *hb);
        assert_eq!(store.get_transaction_hash(&hb.index).unwrap(), None);
    }
    let store = sqlite_on_disk_store(location);
    for hb in &scribe.blockchain {
        assert_eq!(store.get_at(hb.index).unwrap(), *hb);
        assert_eq!(
            store.get_transaction_hash(&hb.index).unwrap(),
            Some(Block::decode(hb.block.clone()).unwrap().transaction.hash())
        );
    }
}
#[actix_rt::test]
async fn store_prune_test() {
    init_test_logger();
    let tmpdir = create_tmp_dir();
    let mut store = sqlite_on_disk_store(tmpdir.path());
    let scribe = Scribe::new_with_sample_data(10, 100);

    for hb in &scribe.blockchain {
        store.push(hb).unwrap();
    }

    prune(&scribe, &mut store, 10);
    verify_pruned(&scribe, &mut store, 10);

    prune(&scribe, &mut store, 20);
    verify_pruned(&scribe, &mut store, 20);
}

#[actix_rt::test]
async fn store_prune_corner_cases_test() {
    init_test_logger();
    let tmpdir = create_tmp_dir();
    let mut store = sqlite_on_disk_store(tmpdir.path());
    let scribe = Scribe::new_with_sample_data(10, 100);

    for hb in &scribe.blockchain {
        store.push(hb).unwrap();
    }

    prune(&scribe, &mut store, 0);
    verify_pruned(&scribe, &mut store, 0);

    prune(&scribe, &mut store, 1);
    verify_pruned(&scribe, &mut store, 0);

    let last_idx = scribe.blockchain.back().unwrap().index;

    prune(&scribe, &mut store, last_idx);
    verify_pruned(&scribe, &mut store, last_idx);
}

#[actix_rt::test]
async fn store_prune_first_balance_test() {
    init_test_logger();
    let tmpdir = create_tmp_dir();
    let mut store = sqlite_on_disk_store(tmpdir.path());
    let scribe = Scribe::new_with_sample_data(10, 100);

    for hb in &scribe.blockchain {
        store.push(hb).unwrap();
    }

    prune(&scribe, &mut store, 10);
    verify_pruned(&scribe, &mut store, 10);
    verify_balance_snapshot(&scribe, &mut store, 10);

    prune(&scribe, &mut store, 20);
    verify_pruned(&scribe, &mut store, 20);
    verify_balance_snapshot(&scribe, &mut store, 20);
}

#[actix_rt::test]
async fn store_prune_and_load_test() {
    init_test_logger();
    let tmpdir = create_tmp_dir();
    let mut store = sqlite_on_disk_store(tmpdir.path());

    let scribe = Scribe::new_with_sample_data(10, 100);

    for hb in &scribe.blockchain {
        store.push(hb).unwrap();
    }

    prune(&scribe, &mut store, 10);
    verify_pruned(&scribe, &mut store, 10);
    verify_balance_snapshot(&scribe, &mut store, 10);

    prune(&scribe, &mut store, 20);
    verify_pruned(&scribe, &mut store, 20);
    verify_balance_snapshot(&scribe, &mut store, 20);

    drop(store);
    // Now reload from disk
    let mut store = sqlite_on_disk_store(tmpdir.path());
    verify_pruned(&scribe, &mut store, 20);
    verify_balance_snapshot(&scribe, &mut store, 20);

    prune(&scribe, &mut store, 30);
    verify_pruned(&scribe, &mut store, 30);
    verify_balance_snapshot(&scribe, &mut store, 30);

    drop(store);
    // Reload once again
    let mut store = sqlite_on_disk_store(tmpdir.path());
    verify_pruned(&scribe, &mut store, 30);
    verify_balance_snapshot(&scribe, &mut store, 30);
}

pub(crate) fn to_balances(
    balances: BTreeMap<AccountIdentifier, Tokens>,
    index: BlockIndex,
) -> BalanceBook {
    let mut balance_book = BalanceBook::default();
    for (acc, amount) in balances {
        balance_book.token_pool -= amount;
        balance_book.store.insert(acc, index, amount);
    }
    balance_book
}

fn prune(scribe: &Scribe, store: &mut SQLiteStore, prune_at: u64) {
    let oldest_idx = prune_at;
    let oldest_block = scribe.blockchain.get(oldest_idx as usize).unwrap();
    let oldest_balance = to_balances(
        scribe
            .balance_history
            .get(oldest_idx as usize)
            .unwrap()
            .clone(),
        oldest_idx,
    );

    store.prune(oldest_block, &oldest_balance).unwrap();
}

fn verify_pruned(scribe: &Scribe, store: &mut SQLiteStore, prune_at: u64) {
    let after_last_idx = scribe.blockchain.len() as u64;
    let oldest_idx = prune_at.min(after_last_idx);

    if after_last_idx > 1 {
        // Genesis block (at idx 0) should still be accessible
        assert_eq!(store.get_at(0).unwrap(), *scribe.blockchain.get(0).unwrap());
    }

    for i in 1..oldest_idx {
        assert_eq!(store.get_at(i).unwrap_err(), BlockStoreError::NotFound(i));
        assert_eq!(
            store.get_transaction(&i).unwrap_err(),
            BlockStoreError::NotFound(i)
        );
    }

    if oldest_idx < after_last_idx {
        assert_eq!(
            store.get_first_hashed_block().ok().map(|x| x.index),
            Some(oldest_idx)
        );
    }

    for i in oldest_idx..after_last_idx {
        assert_eq!(
            store.get_at(i).unwrap(),
            *scribe.blockchain.get(i as usize).unwrap()
        );
    }

    for i in oldest_idx..after_last_idx {
        let block = (*scribe.blockchain.get(i as usize).unwrap()).clone().block;
        assert_eq!(
            store.get_transaction(&i).unwrap(),
            Block::decode(block).unwrap().transaction
        );
    }

    for i in after_last_idx..=scribe.blockchain.len() as u64 {
        assert_eq!(store.get_at(i).unwrap_err(), BlockStoreError::NotFound(i));
    }
}

fn verify_balance_snapshot(scribe: &Scribe, store: &mut SQLiteStore, prune_at: u64) {
    let oldest_idx = prune_at as usize;
    let (oldest_block, balances) = store.first_snapshot().unwrap();
    assert_eq!(oldest_block, *scribe.blockchain.get(oldest_idx).unwrap());

    let scribe_balances = scribe.balance_history.get(oldest_idx).unwrap().clone();

    assert_eq!(balances.store.acc_to_hist.len(), scribe_balances.len());
    for (acc, hist) in &balances.store.acc_to_hist {
        assert_eq!(
            balances.store.get_at(*acc, prune_at).unwrap(),
            *scribe_balances.get(acc).unwrap()
        );
        if let Some(last_entry) = hist.get_history(None).last().map(|x| x.0) {
            assert_eq!(last_entry, prune_at);
        }
    }

    let mut sum_icpt = Tokens::ZERO;
    for amount in scribe.balance_history.get(oldest_idx).unwrap().values() {
        sum_icpt += *amount;
    }
    assert_eq!((Tokens::MAX - sum_icpt).unwrap(), balances.token_pool);
}
