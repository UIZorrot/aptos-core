// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
use crate::config::persistable_config::PersistableConfig;
use crate::{
    config::{
        config_sanitizer::ConfigSanitizer, Error, IdentityBlob, LoggerConfig, NodeConfig, RoleType,
        SecureBackend, WaypointConfig,
    },
    keys::ConfigKey,
};
use aptos_crypto::{bls12381, Uniform};
use aptos_types::{chain_id::ChainId, network_address::NetworkAddress, waypoint::Waypoint, PeerId};
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyRulesConfig {
    pub backend: SecureBackend,
    pub logger: LoggerConfig,
    pub service: SafetyRulesService,
    pub test: Option<SafetyRulesTestConfig>,
    // Read/Write/Connect networking operation timeout in milliseconds.
    pub network_timeout_ms: u64,
    pub enable_cached_safety_data: bool,
    pub initial_safety_rules_config: InitialSafetyRulesConfig,
}

impl Default for SafetyRulesConfig {
    fn default() -> Self {
        Self {
            backend: SecureBackend::InMemoryStorage,
            logger: LoggerConfig::default(),
            service: SafetyRulesService::Local,
            test: None,
            // Default value of 30 seconds for a timeout
            network_timeout_ms: 30_000,
            enable_cached_safety_data: true,
            initial_safety_rules_config: InitialSafetyRulesConfig::None,
        }
    }
}

impl SafetyRulesConfig {
    pub fn set_data_dir(&mut self, data_dir: PathBuf) {
        if let SecureBackend::OnDiskStorage(backend) = &mut self.backend {
            backend.set_data_dir(data_dir);
        } else if let SecureBackend::RocksDbStorage(backend) = &mut self.backend {
            backend.set_data_dir(data_dir);
        }
    }

    #[cfg(test)]
    /// Returns the default safety rules config for a validator (only used by tests)
    pub fn get_default_config() -> Self {
        let contents = include_str!("test_data/safety_rules.yaml");
        SafetyRulesConfig::parse_serialized_config(contents).unwrap_or_else(|error| {
            panic!(
                "Failed to parse default safety rules config! Error: {}",
                error
            )
        })
    }
}

impl ConfigSanitizer for SafetyRulesConfig {
    /// Validate and process the safety rules config according to the given node role and chain ID
    fn sanitize(
        node_config: &mut NodeConfig,
        node_role: RoleType,
        chain_id: ChainId,
    ) -> Result<(), Error> {
        let sanitizer_name = Self::get_sanitizer_name();
        let safety_rules_config = &node_config.consensus.safety_rules;

        // If the node is not a validator, there's nothing to be done
        if !node_role.is_validator() {
            return Ok(());
        }

        // Verify that the secure backend is appropriate for mainnet validators
        if chain_id.is_mainnet()? && node_role.is_validator() {
            if safety_rules_config.backend.is_github() {
                return Err(Error::ConfigSanitizerFailed(
                    sanitizer_name,
                    "The secure backend should not be set to GitHub in mainnet!".to_string(),
                ));
            } else if safety_rules_config.backend.is_in_memory() {
                return Err(Error::ConfigSanitizerFailed(
                    sanitizer_name,
                    "The secure backend should not be set to in memory storage in mainnet!"
                        .to_string(),
                ));
            }
        }

        // Verify that the safety rules service is set to local for optimal performance
        if chain_id.is_mainnet()? && !safety_rules_config.service.is_local() {
            return Err(Error::ConfigSanitizerFailed(
                sanitizer_name,
                format!("The safety rules service should be set to local in mainnet for optimal performance! Given config: {:?}", &safety_rules_config.service)
            ));
        }

        // Verify that the safety rules test config is not enabled in mainnet
        if chain_id.is_mainnet()? && safety_rules_config.test.is_some() {
            return Err(Error::ConfigSanitizerFailed(
                sanitizer_name,
                "The safety rules test config should not be used in mainnet!".to_string(),
            ));
        }

        // Verify that the initial safety rules config is set for validators
        if node_role.is_validator() {
            if let InitialSafetyRulesConfig::None = safety_rules_config.initial_safety_rules_config
            {
                return Err(Error::ConfigSanitizerFailed(
                    sanitizer_name,
                    "The initial safety rules config must be set for validators!".to_string(),
                ));
            }
        }

        Ok(())
    }
}

// TODO: Find a cleaner way so WaypointConfig isn't duplicated
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialSafetyRulesConfig {
    FromFile {
        identity_blob_path: PathBuf,
        waypoint: WaypointConfig,
    },
    None,
}

impl InitialSafetyRulesConfig {
    pub fn from_file(identity_blob_path: PathBuf, waypoint: WaypointConfig) -> Self {
        Self::FromFile {
            identity_blob_path,
            waypoint,
        }
    }

    pub fn waypoint(&self) -> Waypoint {
        match self {
            InitialSafetyRulesConfig::FromFile { waypoint, .. } => waypoint.waypoint(),
            InitialSafetyRulesConfig::None => panic!("Must have a waypoint"),
        }
    }

    pub fn identity_blob(&self) -> IdentityBlob {
        match self {
            InitialSafetyRulesConfig::FromFile {
                identity_blob_path, ..
            } => IdentityBlob::from_file(identity_blob_path).unwrap(),
            InitialSafetyRulesConfig::None => panic!("Must have an identity blob"),
        }
    }
}

/// Defines how safety rules should be executed
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum SafetyRulesService {
    /// This runs safety rules in the same thread as event processor
    Local,
    /// This is the production, separate service approach
    Process(RemoteService),
    /// This runs safety rules in the same thread as event processor but data is passed through the
    /// light weight RPC (serializer)
    Serializer,
    /// This creates a separate thread to run safety rules, it is similar to a fork / exec style
    Thread,
}

impl SafetyRulesService {
    /// Returns true iff the service is local
    fn is_local(&self) -> bool {
        matches!(self, SafetyRulesService::Local)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteService {
    pub server_address: NetworkAddress,
}

impl RemoteService {
    pub fn server_address(&self) -> SocketAddr {
        self.server_address
            .to_socket_addrs()
            .expect("server_address invalid")
            .next()
            .expect("server_address invalid")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SafetyRulesTestConfig {
    pub author: PeerId,
    pub consensus_key: Option<ConfigKey<bls12381::PrivateKey>>,
    pub waypoint: Option<Waypoint>,
}

impl SafetyRulesTestConfig {
    pub fn new(author: PeerId) -> Self {
        Self {
            author,
            consensus_key: None,
            waypoint: None,
        }
    }

    pub fn consensus_key(&mut self, key: bls12381::PrivateKey) {
        self.consensus_key = Some(ConfigKey::new(key));
    }

    pub fn random_consensus_key(&mut self, rng: &mut StdRng) {
        let privkey = bls12381::PrivateKey::generate(rng);
        self.consensus_key = Some(ConfigKey::<bls12381::PrivateKey>::new(privkey));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConsensusConfig;

    #[test]
    fn test_sanitize_invalid_backend_for_mainnet() {
        // Create a node config with an invalid backend for mainnet
        let mut node_config = NodeConfig {
            consensus: ConsensusConfig {
                safety_rules: SafetyRulesConfig {
                    backend: SecureBackend::InMemoryStorage,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        // Verify that the config sanitizer fails
        let error =
            SafetyRulesConfig::sanitize(&mut node_config, RoleType::Validator, ChainId::mainnet())
                .unwrap_err();
        assert!(matches!(error, Error::ConfigSanitizerFailed(_, _)));
    }

    #[test]
    fn test_sanitize_backend_for_mainnet_fullnodes() {
        // Create a node config with an invalid backend for mainnet validators
        let mut node_config = NodeConfig {
            consensus: ConsensusConfig {
                safety_rules: SafetyRulesConfig {
                    backend: SecureBackend::InMemoryStorage,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        // Verify that the config sanitizer passes because the node is a fullnode
        SafetyRulesConfig::sanitize(&mut node_config, RoleType::FullNode, ChainId::mainnet())
            .unwrap();
    }

    #[test]
    fn test_sanitize_invalid_service_for_mainnet() {
        // Create a node config with a non-local service
        let mut node_config = NodeConfig {
            consensus: ConsensusConfig {
                safety_rules: SafetyRulesConfig {
                    service: SafetyRulesService::Serializer,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        // Verify that the config sanitizer fails
        let error =
            SafetyRulesConfig::sanitize(&mut node_config, RoleType::Validator, ChainId::mainnet())
                .unwrap_err();
        assert!(matches!(error, Error::ConfigSanitizerFailed(_, _)));
    }

    #[test]
    fn test_sanitize_test_config_on_mainnet() {
        // Create a node config with a test config
        let mut node_config = NodeConfig {
            consensus: ConsensusConfig {
                safety_rules: SafetyRulesConfig {
                    test: Some(SafetyRulesTestConfig::new(PeerId::random())),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        // Verify that the config sanitizer fails
        let error =
            SafetyRulesConfig::sanitize(&mut node_config, RoleType::Validator, ChainId::mainnet())
                .unwrap_err();
        assert!(matches!(error, Error::ConfigSanitizerFailed(_, _)));
    }

    #[test]
    fn test_sanitize_missing_initial_safety_rules() {
        // Create a node config with a test config
        let mut node_config = NodeConfig {
            consensus: ConsensusConfig {
                safety_rules: SafetyRulesConfig {
                    test: Some(SafetyRulesTestConfig::new(PeerId::random())),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        // Verify that the config sanitizer fails
        let error =
            SafetyRulesConfig::sanitize(&mut node_config, RoleType::Validator, ChainId::mainnet())
                .unwrap_err();
        assert!(matches!(error, Error::ConfigSanitizerFailed(_, _)));
    }
}
