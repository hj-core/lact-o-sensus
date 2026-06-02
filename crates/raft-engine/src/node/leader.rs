use std::collections::HashMap;
use std::collections::HashSet;

use common::proto::v1::raft::LogEntry;
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::errors::NodeError;
use tracing::instrument;

use crate::tick::Tick;
use crate::tick::TickDuration;

use super::NodeState;
use super::RaftNode;
use super::TickAction;

/// Authoritative role responsible for mutation ingestion and log replication.
///
/// The Leader manages the cluster's logical timeline by replicating log
/// entries to Followers and advancing the commit index once a quorum has
/// acknowledged reception (§5.3).
#[derive(Debug)]
pub struct Leader {
    pub(super) next_index: HashMap<NodeId, LogIndex>,
    pub(super) match_index: HashMap<NodeId, LogIndex>,
    pub(super) last_heartbeat: Tick,

    /// The epoch of the latest heartbeat round initiated (§8).
    pub(super) current_read_epoch: u64,
    /// The highest epoch acknowledged by a majority (§8).
    pub(super) confirmed_read_epoch: u64,
    /// Peers who have acknowledged the current_read_epoch.
    pub(super) heartbeat_acks: HashSet<NodeId>,
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
        self.log_store
            .append_entries(vec![entry])
            .map_err(NodeError::from)?;
        Ok(index)
    }
}
