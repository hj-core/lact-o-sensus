//! State Machine (FSM) and Compaction operations for the Raft engine.
//!
//! This module isolates the "State Materialization" layer, managing the
//! application of committed log entries to the business logic and
//! advancing logical horizons after snapshot restoration (ADR 011).

#[cfg(test)]
use common::raft_api::StateMachine;
use common::types::LogIndex;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tracing::debug;
use tracing::info;
use tracing::instrument;

use super::NodeState;
use super::RaftNode;

impl<R: NodeState> RaftNode<R> {
    /// Updates the commit index without triggering FSM application.
    ///
    /// FSM application is deferred to the background applier loop to prevent
    /// heartbeat starvation (ADR 009/RAFT-01).
    #[instrument(
        name = "advance_commit_index",
        target = "raft::replication",
        skip_all,
        fields(index = %index)
    )]
    pub fn advance_last_committed(&mut self, index: LogIndex) -> Result<(), NodeError> {
        self.update_commit_index_only(index)?;
        Ok(())
    }

    /// Persists and updates the commit index without triggering FSM
    /// application. Used by the Freeze-Apply mechanism (ADR 011).
    pub fn update_commit_index_only(&mut self, index: LogIndex) -> Result<(), NodeError> {
        if index < self.last_committed {
            debug!(
                target: ClinicalTarget::RaftReplication.as_str(),
                "Ignoring stale last_committed update: {} < current {}",
                index, self.last_committed
            );
            return Ok(());
        }

        let last_idx = self.last_log_index()?;
        if index > last_idx {
            return Err(NodeError::Protocol(format!(
                "Attempted to commit index {} but last_log_index is {}",
                index, last_idx
            )));
        }

        if index > self.last_committed {
            // Persist last_committed BEFORE applying to FSM to ensure safety
            // across crashes.
            self.log_store
                .save_last_committed(index)
                .map_err(NodeError::from)?;

            self.last_committed = index;

            info!(
                target: ClinicalTarget::RaftReplication.as_str(),
                index = %index,
                "Commit Index Advanced (Logical Only)"
            );
        }
        Ok(())
    }

    /// Advances both the commit index and the volatile application cache
    /// to a specific horizon after a successful snapshot installation.
    ///
    /// Effectively "jumps" the logical state forward to match the semantic
    /// reality of the restored State Machine.
    pub fn advance_horizon_after_snapshot(&mut self, index: LogIndex) -> Result<(), NodeError> {
        // 1. Advance commit index (and persist to log storage)
        self.update_commit_index_only(index)?;

        // 2. Sync volatile cache
        self.last_applied = index;

        info!(
            target: ClinicalTarget::RaftCompaction.as_str(),
            index = %index,
            "Logical horizon advanced to match snapshot."
        );

        Ok(())
    }

    /// Orchestrates the sequential application of committed log entries to the
    /// State Machine.
    ///
    /// NOTE: This method is only used by tests. Production code now uses the
    /// background applier loop via `shell.apply_committed()`.
    #[cfg(test)]
    pub(super) fn apply_to_state_machine<F: StateMachine>(
        &mut self,
        fsm: &F,
    ) -> Result<(), NodeError> {
        use tracing::error;

        // Safety Barrier: Ensure FSM hasn't regressed or moved ahead of log.
        let fsm_last = fsm.last_applied_index().map_err(|e| e.into())?;
        if fsm_last > self.last_committed {
            return Err(NodeError::Protocol(format!(
                "FSM index {} is ahead of last_committed {}. Possible log regression.",
                fsm_last, self.last_committed
            )));
        }

        while self.last_applied < self.last_committed {
            let apply_idx = (self.last_applied + 1)?;
            let entry = self.log_store.read_entry(apply_idx)?.ok_or_else(|| {
                NodeError::Protocol(format!(
                    "Committed entry {} missing from log during apply",
                    apply_idx
                ))
            })?;

            if let Err(e) = fsm.apply(apply_idx, &entry.data) {
                error!(
                    target: ClinicalTarget::ClinicalFsm.as_str(),
                    index = %apply_idx,
                    error = %e,
                    "State machine failed to apply index. Triggering Halt Mandate."
                );
                return Err(e.into());
            }

            debug!(
                target: ClinicalTarget::ClinicalFsm.as_str(),
                index = %apply_idx,
                "Physical Mutation Resolved"
            );

            self.last_applied = apply_idx;
        }
        Ok(())
    }
}
