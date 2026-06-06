//! Shared test utilities for the Raft engine.
//!
//! Provides mock state machine implementations and helper constructors
//! used across multiple test modules.

use std::collections::HashMap;
use std::sync::Arc;

use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::errors::FsmError;
use common::types::trace::TraceId;
use rand::SeedableRng;

use crate::config::Config;
use crate::engine::LogicalNode;
use crate::shell::ConsensusShell;
use crate::storage::MemoryStorage;
use crate::tick::TickDuration;
use crate::tick::TickThresholds;

#[derive(Debug, Default)]
pub struct MockFsm;

impl StateMachine for MockFsm {
    type Error = FsmError;

    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
        Ok(LogIndex::ZERO)
    }

    fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
        Ok(vec![])
    }

    fn install_snapshot(
        &self,
        _index: LogIndex,
        _data: &[u8],
        _trace_id: TraceId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub fn test_config() -> Config {
    Config {
        cluster_id: ClusterId::try_new("test").unwrap(),
        node_id: NodeId::try_new(1).unwrap(),
        listen_addr: "127.0.0.1:8080".parse().unwrap(),
        data_dir: "data".into(),
        peers: HashMap::new(),
        raft: crate::config::RaftConfig::default(),
        policy: crate::config::PolicyConfig::default(),
    }
}

pub fn mock_shell() -> Arc<ConsensusShell<MockFsm>> {
    let id = Arc::new(NodeIdentity::new(
        ClusterId::try_new("test").unwrap(),
        NodeId::try_new(1).unwrap(),
    ));
    let storage = Arc::new(MemoryStorage::new());
    let thresholds = TickThresholds {
        heartbeat_interval: TickDuration::new(10),
        min_election: TickDuration::new(15),
        max_election: TickDuration::new(30),
    };
    let rng = rand::rngs::StdRng::seed_from_u64(1);
    let node = LogicalNode::try_new(id, Arc::new(MockFsm), storage, thresholds, rng).unwrap();
    Arc::new(ConsensusShell::new(node))
}
