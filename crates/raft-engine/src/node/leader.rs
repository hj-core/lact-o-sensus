//! Leader role implementation for the Raft engine.
//!
//! This module defines the authoritative behavior of nodes, including
//! mutation ingestion, log replication (§5.3), and Read Index protocol
//! management for linearizable queries (§8).

use std::collections::HashMap;
use std::collections::HashSet;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::errors::NodeError;
use tracing::instrument;

use super::NodeState;
use super::RaftNode;
use super::TickAction;
use crate::tick::Tick;
use crate::tick::TickDuration;

/// Authoritative role responsible for mutation ingestion and log replication.
///
/// The Leader manages the cluster's logical timeline by replicating log
/// entries to Followers and advancing the commit index once a quorum has
/// acknowledged reception (§5.3).
#[derive(Debug)]
pub struct Leader {
    next_index: HashMap<NodeId, LogIndex>,
    match_index: HashMap<NodeId, LogIndex>,
    last_heartbeat: Tick,

    /// The epoch of the latest heartbeat round initiated (§8).
    current_read_epoch: u64,
    /// The highest epoch acknowledged by a majority (§8).
    confirmed_read_epoch: u64,
    /// Peers who have acknowledged the current_read_epoch.
    heartbeat_acks: HashSet<NodeId>,
}

impl Leader {
    pub fn new(
        peer_ids: Vec<NodeId>,
        last_log_index: LogIndex,
        last_heartbeat: Tick,
    ) -> Result<Self, NodeError> {
        let mut next_index = HashMap::new();
        let mut match_index = HashMap::new();

        for peer_id in peer_ids {
            next_index.insert(peer_id, (last_log_index + 1)?);
            match_index.insert(peer_id, LogIndex::ZERO);
        }

        Ok(Self {
            next_index,
            match_index,
            last_heartbeat,
            current_read_epoch: 0,
            confirmed_read_epoch: 0,
            heartbeat_acks: HashSet::new(),
        })
    }

    pub fn last_heartbeat(&self) -> Tick {
        self.last_heartbeat
    }

    pub fn reset_heartbeat(&mut self, current_tick: Tick) {
        self.last_heartbeat = current_tick;
    }

    pub fn next_index(&self) -> &HashMap<NodeId, LogIndex> {
        &self.next_index
    }

    pub fn next_index_mut(&mut self) -> &mut HashMap<NodeId, LogIndex> {
        &mut self.next_index
    }

    pub fn match_index(&self) -> &HashMap<NodeId, LogIndex> {
        &self.match_index
    }

    pub fn match_index_mut(&mut self) -> &mut HashMap<NodeId, LogIndex> {
        &mut self.match_index
    }

    pub fn current_read_epoch(&self) -> u64 {
        self.current_read_epoch
    }

    pub fn confirmed_read_epoch(&self) -> u64 {
        self.confirmed_read_epoch
    }

    pub fn evaluate_tick(&self, now: Tick, threshold: TickDuration) -> TickAction {
        if now - self.last_heartbeat >= threshold {
            TickAction::SendHeartbeat
        } else {
            TickAction::None
        }
    }

    /// Prepares a new read probe epoch (§8).
    ///
    /// If the current epoch has already reached quorum (or is otherwise
    /// finished), increments the epoch to ensure the next round-trip proves
    /// authority *after* this call. Returns the target epoch to wait for.
    pub fn prepare_read_probe(&mut self, self_id: NodeId) -> u64 {
        if self.confirmed_read_epoch == self.current_read_epoch {
            self.current_read_epoch += 1;
            self.heartbeat_acks.clear();
            self.heartbeat_acks.insert(self_id);
        }
        self.current_read_epoch
    }

    /// Records an acknowledgment for the current heartbeat epoch (§8).
    ///
    /// If a majority is reached, advances the `confirmed_read_epoch`.
    pub fn acknowledge_heartbeat(&mut self, peer_id: NodeId, quorum_size: usize) {
        self.heartbeat_acks.insert(peer_id);
        if self.heartbeat_acks.len() >= quorum_size {
            self.confirmed_read_epoch = self.current_read_epoch;
        }
    }
}

impl NodeState for Leader {}

impl<S: StateMachine> RaftNode<Leader, S> {
    /// Appends a new command to the leader's log and returns the assigned log
    /// index.
    #[instrument(
        name = "proposal_ingestion",
        target = "raft::replication",
        skip_all,
        fields(command_len = command.len())
    )]
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex, NodeError> {
        let index = (self.last_log_index()? + 1)?;
        let entry = LogEntry::new(index, self.current_term()?, command);
        self.append_entries(vec![entry])?;
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use common::proto::v1::raft::LogEntry;
    use common::types::LogIndex;
    use common::types::Term;

    use super::*;
    use crate::node::test_utils::*;
    use crate::storage::LogStorage;
    use crate::storage::MemoryStorage;

    mod propose {
        use super::*;

        #[test]
        fn should_increment_log_length_and_use_current_term() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = setup_node_as_leader(fsm, log_store);
            let current_term = node.current_term().unwrap();

            let index = node.propose(vec![42]).unwrap();

            assert_eq!(index, LogIndex::new(1));
            assert_eq!(node.last_log_index().unwrap(), LogIndex::new(1));
            let entry = node.read_entries(index, index).unwrap().remove(0);
            assert_eq!(Term::new(entry.term), current_term);
            assert_eq!(entry.data, vec![42]);
        }

        #[test]
        fn should_return_error_when_storage_fails() {
            use common::types::errors::LogStorageError;

            #[derive(Debug, Default)]
            struct FailingAppendStorage;
            impl LogStorage for FailingAppendStorage {
                fn current_term(&self) -> Result<Term, LogStorageError> {
                    Ok(Term::new(1))
                }

                fn voted_for(&self) -> Result<Option<NodeId>, LogStorageError> {
                    Ok(None)
                }

                fn last_log_index(&self) -> Result<LogIndex, LogStorageError> {
                    Ok(LogIndex::ZERO)
                }

                fn last_log_term(&self) -> Result<Term, LogStorageError> {
                    Ok(Term::ZERO)
                }

                fn last_committed(&self) -> Result<LogIndex, LogStorageError> {
                    Ok(LogIndex::ZERO)
                }

                fn read_entry(&self, _: LogIndex) -> Result<Option<LogEntry>, LogStorageError> {
                    Ok(None)
                }

                fn read_entries(
                    &self,
                    _: LogIndex,
                    _: LogIndex,
                ) -> Result<Vec<LogEntry>, LogStorageError> {
                    Ok(vec![])
                }

                fn save_hard_state(
                    &self,
                    _: Term,
                    _: Option<NodeId>,
                ) -> Result<(), LogStorageError> {
                    Ok(())
                }

                fn save_last_committed(&self, _: LogIndex) -> Result<(), LogStorageError> {
                    Ok(())
                }

                fn append_entries(&self, _: Vec<LogEntry>) -> Result<(), LogStorageError> {
                    Err(LogStorageError::persistence("Simulated Append Failure"))
                }

                fn truncate_log(&self, _: LogIndex) -> Result<(), LogStorageError> {
                    Ok(())
                }

                fn truncate_log_front(&self, _: LogIndex) -> Result<(), LogStorageError> {
                    Ok(())
                }

                fn save_snapshot_metadata(
                    &self,
                    _: LogIndex,
                    _: Term,
                ) -> Result<(), LogStorageError> {
                    Ok(())
                }

                fn last_included_index(&self) -> Result<LogIndex, LogStorageError> {
                    Ok(LogIndex::ZERO)
                }

                fn last_included_term(&self) -> Result<Term, LogStorageError> {
                    Ok(Term::ZERO)
                }
            }

            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(FailingAppendStorage);
            let mut node = RaftNode {
                identity: test_identity(1),
                fsm,
                log_store,
                last_committed: LogIndex::ZERO,
                last_applied: LogIndex::ZERO,
                state: Leader::new(vec![], LogIndex::ZERO, Tick::new(0)).unwrap(),
            };

            let result = node.propose(vec![1]);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("Append Failure"));
        }
    }

    mod heartbeat_epochs {
        use super::*;

        #[test]
        fn should_advance_epoch_when_quorum_is_reached() {
            let fsm = Arc::new(MockFsm::default());
            let log_store = Arc::new(MemoryStorage::new());
            let mut node = setup_node_as_candidate(fsm, log_store)
                .try_into_leader(
                    vec![NodeId::try_new(2).unwrap(), NodeId::try_new(3).unwrap()],
                    Tick::new(0),
                )
                .unwrap();
            let self_id = node.node_id();

            // 1. Initial state
            assert_eq!(node.state().current_read_epoch(), 0);
            assert_eq!(node.state().confirmed_read_epoch(), 0);

            // 2. Start round 1 (prepare_read_probe increments to 1)
            let target = node.state_mut().prepare_read_probe(self_id);
            assert_eq!(target, 1);

            // 3. Acknowledge from peer 2 (1 peer + self = 2/3 quorum)
            node.state_mut()
                .acknowledge_heartbeat(NodeId::try_new(2).unwrap(), 2);
            assert_eq!(node.state().confirmed_read_epoch(), 1);

            // 4. Start round 2 (increments to 2)
            let target2 = node.state_mut().prepare_read_probe(self_id);
            assert_eq!(target2, 2);
            assert_eq!(node.state().confirmed_read_epoch(), 1);

            // 5. Reach quorum for round 2
            node.state_mut()
                .acknowledge_heartbeat(NodeId::try_new(3).unwrap(), 2);
            assert_eq!(node.state().confirmed_read_epoch(), 2);
        }
    }
}
