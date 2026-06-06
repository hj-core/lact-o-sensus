//! Clinical verification utilities for the Node module.
//!
//! Provides shared mock implementations and setup helpers to ensure
//! consistent behavioral verification across role-specific and shared
//! test suites.

use std::sync::Arc;
use std::sync::Mutex;

use common::raft_api::StateMachine;
use common::types::ClusterId;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::errors::FsmError;
use common::types::trace::TraceId;

use crate::node::Candidate;
use crate::node::Follower;
use crate::node::Leader;
use crate::node::RaftNode;
use crate::storage::MemoryStorage;
use crate::tick::Tick;
use crate::tick::TickDuration;

#[derive(Debug, Default)]
pub struct MockFsm {
    pub applied_indices: Mutex<Vec<LogIndex>>,
    pub applied_data: Mutex<Vec<Vec<u8>>>,
}

impl StateMachine for MockFsm {
    type Error = FsmError;

    fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
        Ok(LogIndex::ZERO)
    }

    fn apply(&self, index: LogIndex, data: &[u8]) -> Result<(), Self::Error> {
        self.applied_indices
            .lock()
            .expect("Clinical Invariant: Mutex must be lockable")
            .push(index);
        self.applied_data
            .lock()
            .expect("Clinical Invariant: Mutex must be lockable")
            .push(data.to_vec());
        Ok(())
    }

    fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
        Ok(vec![])
    }

    fn install_snapshot(
        &self,
        _last_included_index: LogIndex,
        _data: &[u8],
        _trace_id: TraceId,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub fn test_identity(id: u64) -> Arc<NodeIdentity> {
    Arc::new(NodeIdentity::new(
        ClusterId::try_new("test-cluster").unwrap(),
        NodeId::try_new(id).unwrap(),
    ))
}

pub fn setup_node_as_follower(log_store: Arc<MemoryStorage>) -> RaftNode<Follower> {
    RaftNode::try_new(
        test_identity(1),
        log_store,
        Tick::new(0),
        TickDuration::new(100),
    )
    .unwrap()
}

pub fn setup_node_as_candidate(log_store: Arc<MemoryStorage>) -> RaftNode<Candidate> {
    setup_node_as_follower(log_store)
        .try_into_candidate(Tick::new(0), TickDuration::new(150))
        .unwrap()
}

pub fn setup_node_as_leader(log_store: Arc<MemoryStorage>) -> RaftNode<Leader> {
    setup_node_as_candidate(log_store)
        .try_into_leader(vec![], Tick::new(0))
        .unwrap()
}
