// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This file defines state store APIs that are related account state Merkle tree.

use crate::{
    db_metadata::{DbMetadataKey, DbMetadataSchema, DbMetadataValue},
    epoch_by_version::EpochByVersionSchema,
    metrics::{STATE_ITEMS, TOTAL_STATE_BYTES},
    schema::state_value::StateValueSchema,
    stale_state_value_index::StaleStateValueIndexSchema,
    state_kv_db::StateKvDb,
    state_merkle_db::StateMerkleDb,
    state_restore::{StateSnapshotProgress, StateSnapshotRestore, StateValueWriter},
    state_store::buffered_state::BufferedState,
    utils::{
        iterators::PrefixedStateValueIterator,
        truncation_helper::{truncate_ledger_db, truncate_state_kv_db},
    },
    version_data::VersionDataSchema,
    AptosDbError, LedgerStore, ShardedStateKvSchemaBatch, StaleNodeIndexCrossEpochSchema,
    StaleNodeIndexSchema, StateKvPrunerManager, StateMerklePrunerManager, TransactionStore,
    OTHER_TIMERS_SECONDS,
};
use anyhow::{ensure, format_err, Result};
use aptos_crypto::{
    hash::{CryptoHash, SPARSE_MERKLE_PLACEHOLDER_HASH},
    HashValue,
};
use aptos_executor_types::in_memory_state_calculator::InMemoryStateCalculator;
use aptos_infallible::Mutex;
use aptos_jellyfish_merkle::iterator::JellyfishMerkleIterator;
use aptos_logger::info;
use aptos_schemadb::{ReadOptions, SchemaBatch, DB};
use aptos_state_view::StateViewId;
use aptos_storage_interface::{
    cached_state_view::CachedStateView, state_delta::StateDelta,
    sync_proof_fetcher::SyncProofFetcher, DbReader, StateSnapshotReceiver,
};
use aptos_types::{
    proof::{definition::LeafCount, SparseMerkleProofExt, SparseMerkleRangeProof},
    state_store::{
        state_key::StateKey,
        state_key_prefix::StateKeyPrefix,
        state_storage_usage::StateStorageUsage,
        state_value::{StaleStateValueIndex, StateValue, StateValueChunkWithProof},
    },
    transaction::Version,
};
use claims::{assert_ge, assert_le};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    sync::Arc,
};

pub(crate) mod buffered_state;
mod state_merkle_batch_committer;
mod state_snapshot_committer;

#[cfg(test)]
mod state_store_test;

type StateValueBatch = crate::state_restore::StateValueBatch<StateKey, Option<StateValue>>;

// We assume TARGET_SNAPSHOT_INTERVAL_IN_VERSION > block size.
const MAX_WRITE_SETS_AFTER_SNAPSHOT: LeafCount = buffered_state::TARGET_SNAPSHOT_INTERVAL_IN_VERSION
    * (buffered_state::ASYNC_COMMIT_CHANNEL_BUFFER_SIZE + 2 + 1/*  Rendezvous channel */)
    * 2;

const MAX_COMMIT_PROGRESS_DIFFERENCE: u64 = 100000;

static IO_POOL: Lazy<rayon::ThreadPool> = Lazy::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(32)
        .thread_name(|index| format!("kv_reader_{}", index))
        .build()
        .unwrap()
});

pub(crate) struct StateDb {
    pub ledger_db: Arc<DB>,
    pub state_merkle_db: Arc<StateMerkleDb>,
    pub state_kv_db: Arc<StateKvDb>,
    pub state_merkle_pruner: StateMerklePrunerManager<StaleNodeIndexSchema>,
    pub epoch_snapshot_pruner: StateMerklePrunerManager<StaleNodeIndexCrossEpochSchema>,
    pub state_kv_pruner: StateKvPrunerManager,
}

pub(crate) struct StateStore {
    pub(crate) state_db: Arc<StateDb>,
    // The `base` of buffered_state is the latest snapshot in state_merkle_db while `current`
    // is the latest state sparse merkle tree that is replayed from that snapshot until the latest
    // write set stored in ledger_db.
    buffered_state: Mutex<BufferedState>,
    buffered_state_target_items: usize,
}

impl Deref for StateStore {
    type Target = StateDb;

    fn deref(&self) -> &Self::Target {
        self.state_db.deref()
    }
}

// "using an Arc<dyn DbReader> as an Arc<dyn StateReader>" is not allowed in stable Rust. Actually we
// want another trait, `StateReader`, which is a subset of `DbReader` here but Rust does not support trait
// upcasting coercion for now. Should change it to a different trait once upcasting is stabilized.
// ref: https://github.com/rust-lang/rust/issues/65991
impl DbReader for StateDb {
    /// Returns the latest state snapshot strictly before `next_version` if any.
    fn get_state_snapshot_before(
        &self,
        next_version: Version,
    ) -> Result<Option<(Version, HashValue)>> {
        self.state_merkle_db
            .get_state_snapshot_version_before(next_version)?
            .map(|ver| Ok((ver, self.state_merkle_db.get_root_hash(ver)?)))
            .transpose()
    }

    /// Get the latest state value of the given key up to the given version. Only used for testing for now
    /// but should replace the `get_value_with_proof_by_version` call for VM execution if just fetch the
    /// value without proof.
    fn get_state_value_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<StateValue>> {
        Ok(self
            .get_state_value_with_version_by_version(state_key, version)?
            .map(|(_, value)| value))
    }

    /// Returns the proof of the given state key and version.
    fn get_state_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<SparseMerkleProofExt> {
        let (_, proof) = self
            .state_merkle_db
            .get_with_proof_ext(state_key, version)?;
        Ok(proof)
    }

    /// Get the state value with proof given the state key and version
    fn get_state_value_with_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<(Option<StateValue>, SparseMerkleProofExt)> {
        let (leaf_data, proof) = self
            .state_merkle_db
            .get_with_proof_ext(state_key, version)?;
        Ok((
            match leaf_data {
                Some((_, (key, version))) => Some(self.expect_value_by_version(&key, version)?),
                None => None,
            },
            proof,
        ))
    }

    fn get_state_storage_usage(&self, version: Option<Version>) -> Result<StateStorageUsage> {
        version.map_or(Ok(StateStorageUsage::zero()), |version| {
            Ok(self
                .ledger_db
                .get::<VersionDataSchema>(&version)?
                .ok_or_else(|| AptosDbError::NotFound(format!("VersionData at {}", version)))?
                .get_state_storage_usage())
        })
    }
}

impl StateDb {
    /// Get the latest state value and the its corresponding version when its of the given key up
    /// to the given version.
    pub fn get_state_value_with_version_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<(Version, StateValue)>> {
        let mut read_opts = ReadOptions::default();
        // We want `None` if the state_key changes in iteration.
        read_opts.set_prefix_same_as_start(true);
        let mut iter = self
            .state_kv_db
            .db_shard(state_key.get_shard_id())
            .iter::<StateValueSchema>(read_opts)?;
        iter.seek(&(state_key.clone(), version))?;
        Ok(iter
            .next()
            .transpose()?
            .and_then(|((_, version), value_opt)| value_opt.map(|value| (version, value))))
    }

    /// Get the latest ended epoch strictly before required version, i.e. if the passed in version
    /// ends an epoch, return one epoch early than that.
    pub fn get_previous_epoch_ending(&self, version: Version) -> Result<Option<(u64, Version)>> {
        if version == 0 {
            return Ok(None);
        }
        let prev_version = version - 1;

        let mut iter = self
            .ledger_db
            .iter::<EpochByVersionSchema>(ReadOptions::default())?;
        // Search for the end of the previous epoch.
        iter.seek_for_prev(&prev_version)?;
        iter.next().transpose()
    }
}

impl DbReader for StateStore {
    /// Returns the latest state snapshot strictly before `next_version` if any.
    fn get_state_snapshot_before(
        &self,
        next_version: Version,
    ) -> Result<Option<(Version, HashValue)>> {
        self.deref().get_state_snapshot_before(next_version)
    }

    /// Get the latest state value of the given key up to the given version. Only used for testing for now
    /// but should replace the `get_value_with_proof_by_version` call for VM execution if just fetch the
    /// value without proof.
    fn get_state_value_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<Option<StateValue>> {
        self.deref().get_state_value_by_version(state_key, version)
    }

    /// Returns the proof of the given state key and version.
    fn get_state_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<SparseMerkleProofExt> {
        self.deref()
            .get_state_proof_by_version_ext(state_key, version)
    }

    /// Get the state value with proof extension given the state key and version
    fn get_state_value_with_proof_by_version_ext(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<(Option<StateValue>, SparseMerkleProofExt)> {
        self.deref()
            .get_state_value_with_proof_by_version_ext(state_key, version)
    }
}

impl StateDb {
    fn expect_value_by_version(
        &self,
        state_key: &StateKey,
        version: Version,
    ) -> Result<StateValue> {
        self.get_state_value_by_version(state_key, version)
            .and_then(|opt| {
                opt.ok_or_else(|| {
                    format_err!(
                        "State Value is missing for key {:?} by version {}",
                        state_key,
                        version
                    )
                })
            })
    }
}

impl StateStore {
    pub fn new(
        ledger_db: Arc<DB>,
        state_merkle_db: Arc<StateMerkleDb>,
        state_kv_db: Arc<StateKvDb>,
        state_merkle_pruner: StateMerklePrunerManager<StaleNodeIndexSchema>,
        epoch_snapshot_pruner: StateMerklePrunerManager<StaleNodeIndexCrossEpochSchema>,
        state_kv_pruner: StateKvPrunerManager,
        buffered_state_target_items: usize,
        hack_for_tests: bool,
    ) -> Self {
        Self::sync_commit_progress(
            Arc::clone(&ledger_db),
            Arc::clone(&state_kv_db),
            /*crash_if_difference_is_too_large=*/ true,
        );
        let state_db = Arc::new(StateDb {
            ledger_db,
            state_merkle_db,
            state_kv_db,
            state_merkle_pruner,
            epoch_snapshot_pruner,
            state_kv_pruner,
        });
        let buffered_state = Mutex::new(
            Self::create_buffered_state_from_latest_snapshot(
                &state_db,
                buffered_state_target_items,
                hack_for_tests,
                /*check_max_versions_after_snapshot=*/ true,
            )
            .expect("buffered state creation failed."),
        );
        Self {
            state_db,
            buffered_state,
            buffered_state_target_items,
        }
    }

    // We commit the overall commit progress at the last, and use it as the source of truth of the
    // commit progress.
    pub fn sync_commit_progress(
        ledger_db: Arc<DB>,
        state_kv_db: Arc<StateKvDb>,
        crash_if_difference_is_too_large: bool,
    ) {
        if let Some(DbMetadataValue::Version(overall_commit_progress)) = ledger_db
            .get::<DbMetadataSchema>(&DbMetadataKey::OverallCommitProgress)
            .expect("Failed to read overall commit progress.")
        {
            info!(
                overall_commit_progress = overall_commit_progress,
                "Start syncing databases..."
            );
            let ledger_commit_progress = ledger_db
                .get::<DbMetadataSchema>(&DbMetadataKey::LedgerCommitProgress)
                .expect("Failed to read ledger commit progress.")
                .expect("Ledger commit progress cannot be None.")
                .expect_version();
            assert_ge!(ledger_commit_progress, overall_commit_progress);

            let state_kv_commit_progress = state_kv_db
                .metadata_db()
                .get::<DbMetadataSchema>(&DbMetadataKey::StateKvCommitProgress)
                .expect("Failed to read state K/V commit progress.")
                .expect("State K/V commit progress cannot be None.")
                .expect_version();
            assert_ge!(state_kv_commit_progress, overall_commit_progress);

            if ledger_commit_progress != overall_commit_progress {
                info!(
                    ledger_commit_progress = ledger_commit_progress,
                    "Start truncation...",
                );
                let difference = ledger_commit_progress - overall_commit_progress;
                if crash_if_difference_is_too_large {
                    assert_le!(difference, MAX_COMMIT_PROGRESS_DIFFERENCE);
                }
                truncate_ledger_db(
                    Arc::clone(&ledger_db),
                    ledger_commit_progress,
                    overall_commit_progress,
                    difference as usize,
                )
                .expect("Failed to truncate ledger db.");
            }

            if state_kv_commit_progress != overall_commit_progress {
                info!(
                    state_kv_commit_progress = state_kv_commit_progress,
                    "Start truncation..."
                );
                let difference = state_kv_commit_progress - overall_commit_progress;
                if crash_if_difference_is_too_large {
                    assert_le!(difference, MAX_COMMIT_PROGRESS_DIFFERENCE);
                }
                truncate_state_kv_db(
                    Arc::clone(&state_kv_db),
                    state_kv_commit_progress,
                    overall_commit_progress,
                    difference as usize,
                )
                .expect("Failed to truncate state K/V db.");
            }
        } else {
            info!("No overall commit progress was found!");
        }
    }

    #[cfg(feature = "db-debugger")]
    pub fn catch_up_state_merkle_db(
        ledger_db: Arc<DB>,
        state_merkle_db: Arc<StateMerkleDb>,
        state_kv_db: Arc<StateKvDb>,
    ) -> Result<Option<Version>> {
        use aptos_config::config::NO_OP_STORAGE_PRUNER_CONFIG;

        let state_merkle_pruner = StateMerklePrunerManager::new(
            Arc::clone(&state_merkle_db),
            NO_OP_STORAGE_PRUNER_CONFIG.state_merkle_pruner_config,
        );
        let epoch_snapshot_pruner = StateMerklePrunerManager::new(
            Arc::clone(&state_merkle_db),
            NO_OP_STORAGE_PRUNER_CONFIG.state_merkle_pruner_config,
        );
        let state_kv_pruner = StateKvPrunerManager::new(
            Arc::clone(&state_kv_db),
            NO_OP_STORAGE_PRUNER_CONFIG.ledger_pruner_config,
        );
        let state_db = Arc::new(StateDb {
            ledger_db,
            state_merkle_db,
            state_kv_db,
            state_merkle_pruner,
            epoch_snapshot_pruner,
            state_kv_pruner,
        });
        let buffered_state = Self::create_buffered_state_from_latest_snapshot(
            &state_db, 0, /*hack_for_tests=*/ false,
            /*check_max_versions_after_snapshot=*/ false,
        )?;
        Ok(buffered_state.current_state().base_version)
    }

    fn create_buffered_state_from_latest_snapshot(
        state_db: &Arc<StateDb>,
        buffered_state_target_items: usize,
        hack_for_tests: bool,
        check_max_versions_after_snapshot: bool,
    ) -> Result<BufferedState> {
        let ledger_store = LedgerStore::new(Arc::clone(&state_db.ledger_db));
        let num_transactions = ledger_store
            .get_latest_transaction_info_option()?
            .map(|(version, _)| version + 1)
            .unwrap_or(0);

        let latest_snapshot_version = state_db
            .state_merkle_db
            .get_state_snapshot_version_before(num_transactions)
            .expect("Failed to query latest node on initialization.");
        let latest_snapshot_root_hash = if let Some(version) = latest_snapshot_version {
            state_db
                .state_merkle_db
                .get_root_hash(version)
                .expect("Failed to query latest checkpoint root hash on initialization.")
        } else {
            *SPARSE_MERKLE_PLACEHOLDER_HASH
        };
        let usage = state_db.get_state_storage_usage(latest_snapshot_version)?;
        let mut buffered_state = BufferedState::new(
            state_db,
            StateDelta::new_at_checkpoint(
                latest_snapshot_root_hash,
                usage,
                latest_snapshot_version,
            ),
            buffered_state_target_items,
        );

        // In some backup-restore tests we hope to open the db without consistency check.
        if hack_for_tests {
            return Ok(buffered_state);
        }

        // Make sure the committed transactions is ahead of the latest snapshot.
        let snapshot_next_version = latest_snapshot_version.map_or(0, |v| v + 1);

        // For non-restore cases, always snapshot_next_version <= num_transactions.
        if snapshot_next_version > num_transactions {
            info!(
                snapshot_next_version = snapshot_next_version,
                num_transactions = num_transactions,
                "snapshot is after latest transaction version. It should only happen in restore mode",
            );
        }

        // Replaying the committed write sets after the latest snapshot.
        if snapshot_next_version < num_transactions {
            if check_max_versions_after_snapshot {
                ensure!(
                    num_transactions - snapshot_next_version <= MAX_WRITE_SETS_AFTER_SNAPSHOT,
                    "Too many versions after state snapshot. snapshot_next_version: {}, num_transactions: {}",
                    snapshot_next_version,
                    num_transactions,
                );
            }
            let latest_snapshot_state_view = CachedStateView::new(
                StateViewId::Miscellaneous,
                state_db.clone(),
                num_transactions,
                buffered_state.current_state().current.clone(),
                Arc::new(SyncProofFetcher::new(state_db.clone())),
            )?;
            let write_sets = TransactionStore::new(Arc::clone(&state_db.ledger_db))
                .get_write_sets(snapshot_next_version, num_transactions)?;
            let txn_info_iter =
                ledger_store.get_transaction_info_iter(snapshot_next_version, write_sets.len())?;
            let last_checkpoint_index = txn_info_iter
                .into_iter()
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .enumerate()
                .filter(|(_idx, txn_info)| txn_info.is_state_checkpoint())
                .last()
                .map(|(idx, _)| idx);
            latest_snapshot_state_view.prime_cache_by_write_set(&write_sets)?;
            let calculator = InMemoryStateCalculator::new(
                buffered_state.current_state(),
                latest_snapshot_state_view.into_state_cache(),
            );
            let (updates_until_last_checkpoint, state_after_last_checkpoint) = calculator
                .calculate_for_write_sets_after_snapshot(last_checkpoint_index, &write_sets)?;

            // synchronously commit the snapshot at the last checkpoint here if not committed to disk yet.
            buffered_state.update(
                updates_until_last_checkpoint,
                state_after_last_checkpoint,
                true, /* sync_commit */
            )?;
        }

        info!(
            latest_snapshot_version = buffered_state.current_state().base_version,
            latest_snapshot_root_hash = buffered_state.current_state().base.root_hash(),
            latest_in_memory_version = buffered_state.current_state().current_version,
            latest_in_memory_root_hash = buffered_state.current_state().current.root_hash(),
            "StateStore initialization finished.",
        );
        Ok(buffered_state)
    }

    pub fn reset(&self) {
        *self.buffered_state.lock() = Self::create_buffered_state_from_latest_snapshot(
            &self.state_db,
            self.buffered_state_target_items,
            false,
            true,
        )
        .expect("buffered state creation failed.");
    }

    pub fn buffered_state(&self) -> &Mutex<BufferedState> {
        &self.buffered_state
    }

    /// Returns the key, value pairs for a particular state key prefix at at desired version. This
    /// API can be used to get all resources of an account by passing the account address as the
    /// key prefix.
    pub fn get_prefixed_state_value_iterator(
        &self,
        key_prefix: &StateKeyPrefix,
        first_key_opt: Option<&StateKey>,
        desired_version: Version,
    ) -> Result<PrefixedStateValueIterator> {
        // TODO(grao): Support sharding here.
        PrefixedStateValueIterator::new(
            self.state_kv_db.metadata_db(),
            key_prefix.clone(),
            first_key_opt.cloned(),
            desired_version,
        )
    }

    /// Gets the proof that proves a range of accounts.
    pub fn get_value_range_proof(
        &self,
        rightmost_key: HashValue,
        version: Version,
    ) -> Result<SparseMerkleRangeProof> {
        self.state_merkle_db.get_range_proof(rightmost_key, version)
    }

    /// Put the `value_state_sets` into its own CF.
    pub fn put_value_sets(
        &self,
        value_state_sets: Vec<&HashMap<StateKey, Option<StateValue>>>,
        first_version: Version,
        expected_usage: StateStorageUsage,
        ledger_batch: &SchemaBatch,
        sharded_state_kv_batches: &ShardedStateKvSchemaBatch,
    ) -> Result<()> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["put_value_sets"])
            .start_timer();

        self.put_stats_and_indices(
            &value_state_sets,
            first_version,
            expected_usage,
            ledger_batch,
            sharded_state_kv_batches,
        )?;

        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["add_state_kv_batch"])
            .start_timer();

        value_state_sets
            .par_iter()
            .enumerate()
            .flat_map_iter(|(i, kvs)| {
                let version = first_version + i as Version;
                kvs.iter().map(move |(k, v)| {
                    sharded_state_kv_batches[k.get_shard_id() as usize]
                        .put::<StateValueSchema>(&(k.clone(), version), v)
                })
            })
            .collect()
    }

    pub fn get_usage(&self, version: Option<Version>) -> Result<StateStorageUsage> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["get_usage"])
            .start_timer();
        self.state_db.get_state_storage_usage(version)
    }

    /// Put storage usage stats and State key and value indices into the batch.
    /// The state KV indices will be generated as follows:
    /// 1. A deletion at current version is always coupled with stale index for the tombstone with
    /// `stale_since_version` equal to the version, to ensure tombstone is cleared from db after
    /// pruner processes the current version.
    /// 2. An update at current version will first try to find the corresponding old value, if it
    /// exists, a stale index of that old value will be added. Otherwise, it's a no-op. Because
    /// non-existence means either the key never shows up or it got deleted. Neither case needs
    /// extra stale index as 1 cover the latter case.
    pub fn put_stats_and_indices(
        &self,
        value_state_sets: &[&HashMap<StateKey, Option<StateValue>>],
        first_version: Version,
        expected_usage: StateStorageUsage,
        batch: &SchemaBatch,
        sharded_state_kv_batches: &ShardedStateKvSchemaBatch,
    ) -> Result<()> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["put_stats_and_indices"])
            .start_timer();

        let base_version = first_version.checked_sub(1);
        let mut usage = self.get_usage(base_version)?;
        let base_version_usage = usage;
        let cache = Arc::new(DashMap::<StateKey, (Version, Option<StateValue>)>::new());

        if let Some(base_version) = base_version {
            let _timer = OTHER_TIMERS_SECONDS
                .with_label_values(&["put_stats_and_indices__total_get"])
                .start_timer();
            let key_set = value_state_sets
                .iter()
                .flat_map(|value_state_set| value_state_set.iter())
                .map(|(key, _)| key)
                .collect::<HashSet<_>>();
            IO_POOL.scope(|s| {
                for key in key_set {
                    let cache = cache.clone();
                    s.spawn(move |_| {
                        let _timer = OTHER_TIMERS_SECONDS
                            .with_label_values(&["put_stats_and_indices__get_state_value"])
                            .start_timer();
                        let version_and_value = self
                            .state_db
                            .get_state_value_with_version_by_version(key, base_version)
                            .expect("Must succeed.");
                        if let Some((version, value)) = version_and_value {
                            cache.insert(key.clone(), (version, Some(value)));
                        } else {
                            cache.insert(key.clone(), (base_version, None));
                        }
                    });
                }
            });
        }

        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["put_stats_and_indices__calculate_total_size"])
            .start_timer();
        // calculate total state size in bytes
        for (idx, kvs) in value_state_sets.iter().enumerate() {
            let version = first_version + idx as Version;

            for (key, value) in kvs.iter() {
                if let Some(value) = value {
                    usage.add_item(key.size() + value.size());
                } else {
                    // stale index of the tombstone at current version.
                    sharded_state_kv_batches[key.get_shard_id() as usize]
                        .put::<StaleStateValueIndexSchema>(
                        &StaleStateValueIndex {
                            stale_since_version: version,
                            version,
                            state_key: key.clone(),
                        },
                        &(),
                    )?;
                }

                let old_version_and_value_opt = if let Some((old_version, old_value_opt)) =
                    cache.insert(key.clone(), (version, value.clone()))
                {
                    old_value_opt.map(|value| (old_version, value))
                } else {
                    None
                };

                if let Some((old_version, old_value)) = old_version_and_value_opt {
                    usage.remove_item(key.size() + old_value.size());
                    // stale index of the old value at its version.
                    sharded_state_kv_batches[key.get_shard_id() as usize]
                        .put::<StaleStateValueIndexSchema>(
                        &StaleStateValueIndex {
                            stale_since_version: version,
                            version: old_version,
                            state_key: key.clone(),
                        },
                        &(),
                    )?;
                }
            }

            STATE_ITEMS.set(usage.items() as i64);
            TOTAL_STATE_BYTES.set(usage.bytes() as i64);
            batch.put::<VersionDataSchema>(&version, &usage.into())?;
        }

        if !expected_usage.is_untracked() {
            ensure!(
                expected_usage == usage,
                "Calculated state db usage at version {} not expected. expected: {:?}, calculated: {:?}, base version: {:?}, base version usage: {:?}",
                first_version + value_state_sets.len() as u64 - 1,
                expected_usage,
                usage,
                base_version,
                base_version_usage,
            );
        }

        Ok(())
    }

    /// Merklize the results generated by `value_state_sets` to `batch` and return the result root
    /// hashes for each write set.
    #[cfg(test)]
    pub fn merklize_value_set(
        &self,
        value_set: Vec<(HashValue, Option<&(HashValue, StateKey)>)>,
        node_hashes: Option<&HashMap<aptos_types::nibble::nibble_path::NibblePath, HashValue>>,
        version: Version,
        base_version: Option<Version>,
    ) -> Result<HashValue> {
        let (top_levels_batch, sharded_batch, hash) = self.state_merkle_db.merklize_value_set(
            value_set,
            node_hashes,
            version,
            base_version,
            None, // previous epoch ending version
        )?;
        self.state_merkle_db
            .commit(version, top_levels_batch, sharded_batch)?;
        Ok(hash)
    }

    pub fn get_root_hash(&self, version: Version) -> Result<HashValue> {
        self.state_merkle_db.get_root_hash(version)
    }

    pub fn get_value_count(&self, version: Version) -> Result<usize> {
        self.state_merkle_db.get_leaf_count(version)
    }

    pub fn get_state_key_and_value_iter(
        self: &Arc<Self>,
        version: Version,
        start_hashed_key: HashValue,
    ) -> Result<impl Iterator<Item = Result<(StateKey, StateValue)>> + Send + Sync> {
        let store = Arc::clone(self);
        Ok(JellyfishMerkleIterator::new(
            Arc::clone(&self.state_merkle_db),
            version,
            start_hashed_key,
        )?
        .map(move |res| match res {
            Ok((_hashed_key, (key, version))) => {
                Ok((key.clone(), store.expect_value_by_version(&key, version)?))
            },
            Err(err) => Err(err),
        }))
    }

    pub fn get_value_chunk_with_proof(
        self: &Arc<Self>,
        version: Version,
        first_index: usize,
        chunk_size: usize,
    ) -> Result<StateValueChunkWithProof> {
        let result_iter = JellyfishMerkleIterator::new_by_index(
            Arc::clone(&self.state_merkle_db),
            version,
            first_index,
        )?
        .take(chunk_size);
        let state_key_values: Vec<(StateKey, StateValue)> = result_iter
            .into_iter()
            .map(|res| {
                res.and_then(|(_, (key, version))| {
                    Ok((key.clone(), self.expect_value_by_version(&key, version)?))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !state_key_values.is_empty(),
            AptosDbError::NotFound(format!("State chunk starting at {}", first_index)),
        );
        let last_index = (state_key_values.len() - 1 + first_index) as u64;
        let first_key = state_key_values.first().expect("checked to exist").0.hash();
        let last_key = state_key_values.last().expect("checked to exist").0.hash();
        let proof = self.get_value_range_proof(last_key, version)?;
        let root_hash = self.get_root_hash(version)?;

        Ok(StateValueChunkWithProof {
            first_index: first_index as u64,
            last_index,
            first_key,
            last_key,
            raw_values: state_key_values,
            proof,
            root_hash,
        })
    }

    // state sync doesn't query for the progress, but keeps its record by itself.
    // TODO: change to async comment once it does like https://github.com/aptos-labs/aptos-core/blob/159b00f3d53e4327523052c1b99dd9889bf13b03/storage/backup/backup-cli/src/backup_types/state_snapshot/restore.rs#L147 or overlap at least two chunks.
    pub fn get_snapshot_receiver(
        self: &Arc<Self>,
        version: Version,
        expected_root_hash: HashValue,
    ) -> Result<Box<dyn StateSnapshotReceiver<StateKey, StateValue>>> {
        Ok(Box::new(StateSnapshotRestore::new(
            &self.state_merkle_db,
            self,
            version,
            expected_root_hash,
            false, /* async_commit */
        )?))
    }

    #[cfg(test)]
    pub fn get_all_jmt_nodes_referenced(
        &self,
        version: Version,
    ) -> Result<Vec<aptos_jellyfish_merkle::node_type::NodeKey>> {
        aptos_jellyfish_merkle::JellyfishMerkleTree::new(self.state_merkle_db.as_ref())
            .get_all_nodes_referenced(version)
    }

    #[cfg(test)]
    pub fn get_all_jmt_nodes(&self) -> Result<Vec<aptos_jellyfish_merkle::node_type::NodeKey>> {
        // TODO(grao): Support sharding here.
        let mut iter = self
            .state_db
            .state_merkle_db
            .metadata_db()
            .iter::<crate::jellyfish_merkle_node::JellyfishMerkleNodeSchema>(
            Default::default(),
        )?;
        iter.seek_to_first();
        let all_rows = iter.collect::<Result<Vec<_>>>()?;
        Ok(all_rows.into_iter().map(|(k, _v)| k).collect())
    }
}

impl StateValueWriter<StateKey, StateValue> for StateStore {
    fn write_kv_batch(
        &self,
        version: Version,
        node_batch: &StateValueBatch,
        progress: StateSnapshotProgress,
    ) -> Result<()> {
        let _timer = OTHER_TIMERS_SECONDS
            .with_label_values(&["state_value_writer_write_chunk"])
            .start_timer();
        let batch = SchemaBatch::new();
        node_batch
            .par_iter()
            .map(|(k, v)| batch.put::<StateValueSchema>(k, v))
            .collect::<Result<Vec<_>>>()?;
        batch.put::<DbMetadataSchema>(
            &DbMetadataKey::StateSnapshotRestoreProgress(version),
            &DbMetadataValue::StateSnapshotProgress(progress),
        )?;
        // TODO(grao): Support sharding here.
        self.state_kv_db.commit_raw_batch(batch)
    }

    fn write_usage(&self, version: Version, usage: StateStorageUsage) -> Result<()> {
        self.ledger_db
            .put::<VersionDataSchema>(&version, &usage.into())
    }

    fn get_progress(&self, version: Version) -> Result<Option<StateSnapshotProgress>> {
        Ok(self
            .state_kv_db
            .metadata_db()
            .get::<DbMetadataSchema>(&DbMetadataKey::StateSnapshotRestoreProgress(version))?
            .map(|v| v.expect_state_snapshot_progress()))
    }
}
