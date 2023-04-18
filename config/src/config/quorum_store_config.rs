// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::config::{
    config_sanitizer::ConfigSanitizer, Error, NodeConfig, RoleType,
    MAX_SENDING_BLOCK_TXNS_QUORUM_STORE_OVERRIDE,
};
use aptos_global_constants::DEFAULT_BUCKETS;
use aptos_types::chain_id::ChainId;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuorumStoreBackPressureConfig {
    pub backlog_txn_limit_count: u64,
    pub backlog_per_validator_batch_limit_count: u64,
    pub decrease_duration_ms: u64,
    pub increase_duration_ms: u64,
    pub decrease_fraction: f64,
    pub dynamic_min_txn_per_s: u64,
    pub dynamic_max_txn_per_s: u64,
}

impl Default for QuorumStoreBackPressureConfig {
    fn default() -> QuorumStoreBackPressureConfig {
        QuorumStoreBackPressureConfig {
            // QS will be backpressured if the remaining total txns is more than this number
            backlog_txn_limit_count: MAX_SENDING_BLOCK_TXNS_QUORUM_STORE_OVERRIDE * 3,
            // QS will create batches at the max rate until this number is reached
            backlog_per_validator_batch_limit_count: 4,
            decrease_duration_ms: 1000,
            increase_duration_ms: 1000,
            decrease_fraction: 0.5,
            dynamic_min_txn_per_s: 160,
            dynamic_max_txn_per_s: 2000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuorumStoreConfig {
    pub channel_size: usize,
    pub proof_timeout_ms: usize,
    pub batch_generation_poll_interval_ms: usize,
    pub batch_generation_min_non_empty_interval_ms: usize,
    pub batch_generation_max_interval_ms: usize,
    pub sender_max_batch_txns: usize,
    pub sender_max_batch_bytes: usize,
    pub sender_max_num_batches: usize,
    pub sender_max_total_txns: usize,
    pub sender_max_total_bytes: usize,
    pub receiver_max_batch_txns: usize,
    pub receiver_max_batch_bytes: usize,
    pub receiver_max_num_batches: usize,
    pub receiver_max_total_txns: usize,
    pub receiver_max_total_bytes: usize,
    pub batch_request_num_peers: usize,
    pub batch_request_retry_limit: usize,
    pub batch_request_retry_interval_ms: usize,
    pub batch_request_rpc_timeout_ms: usize,
    /// Used when setting up the expiration time for the batch initation.
    pub batch_expiry_gap_when_init_usecs: u64,
    pub memory_quota: usize,
    pub db_quota: usize,
    pub batch_quota: usize,
    pub mempool_txn_pull_max_bytes: u64,
    pub back_pressure: QuorumStoreBackPressureConfig,
    pub num_workers_for_remote_batches: usize,
    pub batch_buckets: Vec<u64>,
}

impl Default for QuorumStoreConfig {
    fn default() -> QuorumStoreConfig {
        QuorumStoreConfig {
            channel_size: 1000,
            proof_timeout_ms: 10000,
            batch_generation_poll_interval_ms: 25,
            batch_generation_min_non_empty_interval_ms: 100,
            batch_generation_max_interval_ms: 250,
            sender_max_batch_txns: 250,
            sender_max_batch_bytes: 4 * 1024 * 1024,
            sender_max_num_batches: 20,
            sender_max_total_txns: 2000,
            sender_max_total_bytes: 4 * 1024 * 1024,
            receiver_max_batch_txns: 250,
            receiver_max_batch_bytes: 4 * 1024 * 1024,
            receiver_max_num_batches: 20,
            receiver_max_total_txns: 2000,
            receiver_max_total_bytes: 4 * 1024 * 1024,
            batch_request_num_peers: 3,
            batch_request_retry_limit: 10,
            batch_request_retry_interval_ms: 1000,
            batch_request_rpc_timeout_ms: 5000,
            batch_expiry_gap_when_init_usecs: Duration::from_secs(60).as_micros() as u64,
            memory_quota: 120_000_000,
            db_quota: 300_000_000,
            batch_quota: 300_000,
            mempool_txn_pull_max_bytes: 4 * 1024 * 1024,
            back_pressure: QuorumStoreBackPressureConfig::default(),
            // number of batch coordinators to handle QS batch messages, should be >= 1
            num_workers_for_remote_batches: 10,
            batch_buckets: DEFAULT_BUCKETS.to_vec(),
        }
    }
}

impl QuorumStoreConfig {
    fn sanitize_send_recv_batch_limits(
        sanitizer_name: &String,
        config: &QuorumStoreConfig,
    ) -> Result<(), Error> {
        let send_recv_pairs = [
            (
                config.sender_max_batch_txns,
                config.receiver_max_batch_txns,
                "txns",
            ),
            (
                config.sender_max_batch_bytes,
                config.receiver_max_batch_bytes,
                "bytes",
            ),
            (
                config.sender_max_total_txns,
                config.receiver_max_total_txns,
                "total_txns",
            ),
            (
                config.sender_max_total_bytes,
                config.receiver_max_total_bytes,
                "total_bytes",
            ),
        ];
        for (send, recv, label) in &send_recv_pairs {
            if *send > *recv {
                return Err(Error::ConfigSanitizerFailed(
                    sanitizer_name.clone(),
                    format!("Failed {}: {} > {}", label, *send, *recv),
                ));
            }
        }
        Ok(())
    }

    fn sanitize_batch_total_limits(
        sanitizer_name: &String,
        config: &QuorumStoreConfig,
    ) -> Result<(), Error> {
        let batch_total_pairs = [
            (
                config.sender_max_batch_txns,
                config.sender_max_total_txns,
                "send_txns",
            ),
            (
                config.sender_max_batch_bytes,
                config.sender_max_total_bytes,
                "send_bytes",
            ),
            (
                config.receiver_max_batch_txns,
                config.receiver_max_total_txns,
                "recv_txns",
            ),
            (
                config.receiver_max_batch_bytes,
                config.receiver_max_total_bytes,
                "recv_bytes",
            ),
        ];
        for (batch, total, label) in &batch_total_pairs {
            if *batch > *total {
                return Err(Error::ConfigSanitizerFailed(
                    sanitizer_name.clone(),
                    format!("Failed {}: {} > {}", label, *batch, *total),
                ));
            }
        }
        Ok(())
    }
}

impl ConfigSanitizer for QuorumStoreConfig {
    /// Validate and process the quorum store config according to the given node role and chain ID
    fn sanitize(
        node_config: &mut NodeConfig,
        _node_role: RoleType,
        _chain_id: ChainId,
    ) -> Result<(), Error> {
        let sanitizer_name = Self::get_sanitizer_name();

        Self::sanitize_send_recv_batch_limits(
            &sanitizer_name,
            &node_config.consensus.quorum_store_configs,
        )?;
        Self::sanitize_batch_total_limits(
            &sanitizer_name,
            &node_config.consensus.quorum_store_configs,
        )?;
        Ok(())
    }
}
