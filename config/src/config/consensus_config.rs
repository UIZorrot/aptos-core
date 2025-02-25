// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::config::{
    config_sanitizer::ConfigSanitizer, Error, NodeConfig, QuorumStoreConfig, RoleType,
    SafetyRulesConfig,
};
use aptos_types::chain_id::ChainId;
use cfg_if::cfg_if;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) const MAX_SENDING_BLOCK_TXNS_QUORUM_STORE_OVERRIDE: u64 = 4000;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsensusConfig {
    // length of inbound queue of messages
    pub max_network_channel_size: usize,
    // Use getters to read the correct value with/without quorum store.
    pub max_sending_block_txns: u64,
    pub max_sending_block_txns_quorum_store_override: u64,
    pub max_sending_block_bytes: u64,
    pub max_sending_block_bytes_quorum_store_override: u64,
    pub max_receiving_block_txns: u64,
    pub max_receiving_block_txns_quorum_store_override: u64,
    pub max_receiving_block_bytes: u64,
    pub max_receiving_block_bytes_quorum_store_override: u64,
    pub max_pruned_blocks_in_mem: usize,
    // Timeout for consensus to get an ack from mempool for executed transactions (in milliseconds)
    pub mempool_executed_txn_timeout_ms: u64,
    // Timeout for consensus to pull transactions from mempool and get a response (in milliseconds)
    pub mempool_txn_pull_timeout_ms: u64,
    pub round_initial_timeout_ms: u64,
    pub round_timeout_backoff_exponent_base: f64,
    pub round_timeout_backoff_max_exponent: usize,
    pub safety_rules: SafetyRulesConfig,
    // Only sync committed transactions but not vote for any pending blocks. This is useful when
    // validators coordinate on the latest version to apply a manual transaction.
    pub sync_only: bool,
    pub channel_size: usize,
    pub quorum_store_pull_timeout_ms: u64,
    // Decides how long the leader waits before proposing empty block if there's no txns in mempool
    pub quorum_store_poll_time_ms: u64,
    // Whether to create partial blocks when few transactions exist, or empty blocks when there is
    // pending ordering, or to wait for quorum_store_poll_count * 30ms to collect transactions for a block
    //
    // It is more efficient to execute larger blocks, as it creates less overhead. On the other hand
    // waiting increases latency (unless we are under high load that added waiting latency
    // is compensated by faster execution time). So we want to balance the two, by waiting only
    // when we are saturating the execution pipeline:
    // - if there are more pending blocks then usual in the execution pipeline,
    //   block is going to wait there anyways, so we can wait to create a bigger/more efificent block
    // - in case our node is faster than others, and we don't have many pending blocks,
    //   but we still see very large recent (pending) blocks, we know that there is demand
    //   and others are creating large blocks, so we can wait as well.
    pub wait_for_full_blocks_above_pending_blocks: usize,
    pub wait_for_full_blocks_above_recent_fill_threshold: f32,
    pub intra_consensus_channel_buffer_size: usize,
    pub quorum_store_configs: QuorumStoreConfig,
    pub vote_back_pressure_limit: u64,
    pub pipeline_backpressure: Vec<PipelineBackpressureValues>,
    // Used to decide if backoff is needed.
    // must match one of the CHAIN_HEALTH_WINDOW_SIZES values.
    pub window_for_chain_health: usize,
    pub chain_health_backoff: Vec<ChainHealthBackoffValues>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PipelineBackpressureValues {
    pub back_pressure_pipeline_latency_limit_ms: u64,
    pub max_sending_block_txns_override: u64,
    pub max_sending_block_bytes_override: u64,
    // If there is backpressure, giving some more breathing room to go through the backlog,
    // and making sure rounds don't go extremely fast (even if they are smaller blocks)
    // Set to a small enough value, so it is unlikely to affect proposer being able to finish the round in time.
    // If we want to dynamically increase it beyond quorum_store_poll_time,
    // we need to adjust timeouts other nodes use for the backpressured round.
    pub backpressure_proposal_delay_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ChainHealthBackoffValues {
    pub backoff_if_below_participating_voting_power_percentage: usize,

    pub max_sending_block_txns_override: u64,
    pub max_sending_block_bytes_override: u64,
}

impl Default for ConsensusConfig {
    fn default() -> ConsensusConfig {
        ConsensusConfig {
            max_network_channel_size: 1024,
            max_sending_block_txns: 2500,
            max_sending_block_txns_quorum_store_override:
                MAX_SENDING_BLOCK_TXNS_QUORUM_STORE_OVERRIDE,
            // defaulting to under 0.5s to broadcast the proposal to 100 validators
            // over 1gbps link
            max_sending_block_bytes: 600 * 1024, // 600 KB
            max_sending_block_bytes_quorum_store_override: 5 * 1024 * 1024, // 5MB
            max_receiving_block_txns: 10000,
            max_receiving_block_txns_quorum_store_override: 2
                * MAX_SENDING_BLOCK_TXNS_QUORUM_STORE_OVERRIDE,
            max_receiving_block_bytes: 3 * 1024 * 1024, // 3MB
            max_receiving_block_bytes_quorum_store_override: 6 * 1024 * 1024, // 6MB
            max_pruned_blocks_in_mem: 100,
            mempool_executed_txn_timeout_ms: 1000,
            mempool_txn_pull_timeout_ms: 1000,
            round_initial_timeout_ms: 1500,
            // 1.2^6 ~= 3
            // Timeout goes from initial_timeout to initial_timeout*3 in 6 steps
            round_timeout_backoff_exponent_base: 1.2,
            round_timeout_backoff_max_exponent: 6,
            safety_rules: SafetyRulesConfig::default(),
            sync_only: false,
            channel_size: 30, // hard-coded
            quorum_store_pull_timeout_ms: 400,
            quorum_store_poll_time_ms: 300,
            // disable wait_for_full until fully tested
            // We never go above 20-30 pending blocks, so this disables it
            wait_for_full_blocks_above_pending_blocks: 100,
            // Max is 1, so 1.1 disables it.
            wait_for_full_blocks_above_recent_fill_threshold: 1.1,
            intra_consensus_channel_buffer_size: 10,
            quorum_store_configs: QuorumStoreConfig::default(),

            // Voting backpressure is only used as a backup, to make sure pending rounds don't
            // increase uncontrollably, and we know when to go to state sync.
            vote_back_pressure_limit: 30,
            pipeline_backpressure: vec![
                PipelineBackpressureValues {
                    back_pressure_pipeline_latency_limit_ms: 1000,
                    max_sending_block_txns_override: 10000,
                    max_sending_block_bytes_override: 5000 * 1024,
                    backpressure_proposal_delay_ms: 100,
                },
                PipelineBackpressureValues {
                    back_pressure_pipeline_latency_limit_ms: 1500,
                    max_sending_block_txns_override: 10000,
                    max_sending_block_bytes_override: 5000 * 1024,
                    backpressure_proposal_delay_ms: 200,
                },
                PipelineBackpressureValues {
                    back_pressure_pipeline_latency_limit_ms: 2000,
                    max_sending_block_txns_override: 10000,
                    max_sending_block_bytes_override: 5000 * 1024,
                    backpressure_proposal_delay_ms: 300,
                },
                PipelineBackpressureValues {
                    back_pressure_pipeline_latency_limit_ms: 2500,
                    max_sending_block_txns_override: 2000,
                    max_sending_block_bytes_override: 500 * 1024,
                    backpressure_proposal_delay_ms: 300,
                },
                PipelineBackpressureValues {
                    back_pressure_pipeline_latency_limit_ms: 4000,
                    max_sending_block_txns_override: 500,
                    max_sending_block_bytes_override: 100 * 1024,
                    backpressure_proposal_delay_ms: 300,
                },
                // PipelineBackpressureValues {
                //     back_pressure_pipeline_latency_limit_ms: 3500,
                //     // in practice, latencies make it such that 2-4 blocks/s is max,
                //     // meaning that most aggressively we limit to ~600-1200 TPS
                //     // For transactions that are more expensive than that, we should
                //     // instead rely on max gas per block to limit latency
                //     max_sending_block_txns_override: 300,
                //     // stop reducing size, so 100k transactions can still go through
                //     max_sending_block_bytes_override: 100 * 1024,
                //     backpressure_proposal_delay_ms: 300,
                // },
            ],
            window_for_chain_health: 100,
            chain_health_backoff: vec![
                ChainHealthBackoffValues {
                    backoff_if_below_participating_voting_power_percentage: 80,
                    max_sending_block_txns_override: 2000,
                    max_sending_block_bytes_override: 500 * 1024,
                },
                ChainHealthBackoffValues {
                    backoff_if_below_participating_voting_power_percentage: 77,
                    max_sending_block_txns_override: 1000,
                    max_sending_block_bytes_override: 250 * 1024,
                },
                ChainHealthBackoffValues {
                    backoff_if_below_participating_voting_power_percentage: 75,
                    max_sending_block_txns_override: 400,
                    max_sending_block_bytes_override: 100 * 1024,
                },
                ChainHealthBackoffValues {
                    backoff_if_below_participating_voting_power_percentage: 72,
                    max_sending_block_txns_override: 200,
                    // stop reducing size, so 100k transactions can still go through
                    max_sending_block_bytes_override: 100 * 1024,
                },
                ChainHealthBackoffValues {
                    backoff_if_below_participating_voting_power_percentage: 69,
                    // in practice, latencies make it such that 2-4 blocks/s is max,
                    // meaning that most aggressively we limit to ~200-400 TPS
                    max_sending_block_txns_override: 100,
                    max_sending_block_bytes_override: 100 * 1024,
                },
            ],
        }
    }
}

impl ConsensusConfig {
    pub fn set_data_dir(&mut self, data_dir: PathBuf) {
        self.safety_rules.set_data_dir(data_dir);
    }

    pub fn max_sending_block_txns(&self, quorum_store_enabled: bool) -> u64 {
        if quorum_store_enabled {
            self.max_sending_block_txns_quorum_store_override
        } else {
            self.max_sending_block_txns
        }
    }

    pub fn max_sending_block_bytes(&self, quorum_store_enabled: bool) -> u64 {
        if quorum_store_enabled {
            self.max_sending_block_bytes_quorum_store_override
        } else {
            self.max_sending_block_bytes
        }
    }

    pub fn max_receiving_block_txns(&self, quorum_store_enabled: bool) -> u64 {
        if quorum_store_enabled {
            self.max_receiving_block_txns_quorum_store_override
        } else {
            self.max_receiving_block_txns
        }
    }

    pub fn max_receiving_block_bytes(&self, quorum_store_enabled: bool) -> u64 {
        if quorum_store_enabled {
            self.max_receiving_block_bytes_quorum_store_override
        } else {
            self.max_receiving_block_bytes
        }
    }
}

impl ConfigSanitizer for ConsensusConfig {
    /// Validate and process the consensus config according to the given node role and chain ID
    fn sanitize(
        node_config: &mut NodeConfig,
        node_role: RoleType,
        chain_id: ChainId,
    ) -> Result<(), Error> {
        // Verify that the safety rules config is valid
        let sanitizer_name = Self::get_sanitizer_name();
        SafetyRulesConfig::sanitize(node_config, node_role, chain_id)?;

        // Verify that the consensus-only feature is not enabled in mainnet
        if chain_id.is_mainnet()? && is_consensus_only_perf_test_enabled() {
            return Err(Error::ConfigSanitizerFailed(
                sanitizer_name,
                "consensus-only-perf-test should not be enabled in mainnet!".to_string(),
            ));
        }

        Ok(())
    }
}

/// Returns true iff consensus-only-perf-test is enabled
fn is_consensus_only_perf_test_enabled() -> bool {
    cfg_if! {
        if #[cfg(feature = "consensus-only-perf-test")] {
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_config_serialization() {
        let config = ConsensusConfig::default();
        let s = serde_yaml::to_string(&config).unwrap();

        serde_yaml::from_str::<ConsensusConfig>(&s).unwrap();
    }
}
