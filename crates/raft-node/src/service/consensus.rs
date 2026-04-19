use std::sync::Arc;

use common::proto::v1::AppendEntriesRequest;
use common::proto::v1::AppendEntriesResponse;
use common::proto::v1::RequestVoteRequest;
use common::proto::v1::RequestVoteResponse;
use common::proto::v1::consensus_service_server::ConsensusService;
use common::types::NodeId;
use tokio::sync::RwLock;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;

use crate::identity::NodeIdentity;
use crate::node::RaftNodeState;
use crate::service::common::ServiceState;

/// Implementation of the internal Raft consensus RPCs.
///
/// This service acts as a dispatcher, delegating logic to the underlying
/// Type-State node engine while enforcing cluster identity and node health.
#[derive(Debug)]
pub struct ConsensusDispatcher {
    identity: Arc<NodeIdentity>,
    state: Arc<RwLock<RaftNodeState>>,
}

impl ConsensusDispatcher {
    pub fn new(identity: Arc<NodeIdentity>, state: Arc<RwLock<RaftNodeState>>) -> Self {
        Self { identity, state }
    }
}

impl ServiceState for ConsensusDispatcher {
    fn identity_arc(&self) -> &Arc<NodeIdentity> {
        &self.identity
    }
}

#[tonic::async_trait]
impl ConsensusService for ConsensusDispatcher {
    async fn request_vote(
        &self,
        request: Request<RequestVoteRequest>,
    ) -> Result<Response<RequestVoteResponse>, Status> {
        let req = request.into_inner();
        self.verify_identity(&req.cluster_id)?;

        let candidate_id = req
            .candidate_id
            .parse::<NodeId>()
            .map_err(|_| self.invalid_node_id_status(&req.candidate_id))?;

        let span = info_span!("request_vote", term = req.term, candidate = %candidate_id);
        let _enter = span.enter();

        let mut state_guard = self.state.write().await;
        self.verify_node_integrity(&state_guard)?;

        // 1. If term > currentTerm: set currentTerm = term, transition to follower
        //    (§5.1)
        let current_term = state_guard
            .current_term()
            .map_err(|_| self.poisoned_status())?;

        if req.term > current_term {
            info!(
                "Received higher term ({}) from candidate {}. Transitioning to Follower.",
                req.term, candidate_id
            );
            state_guard.transition(|old| old.into_follower(req.term, None));
        }

        // Re-acquire current state after potential transition
        let mut vote_granted = false;
        match &mut *state_guard {
            RaftNodeState::Follower(node) => {
                // 2. If votedFor is null or candidateId, and candidate’s log is at
                // least as up-to-date as receiver’s log, grant vote (§5.2, §5.4)
                if req.term >= node.current_term()
                    && (node.voted_for().is_none() || node.voted_for() == Some(candidate_id))
                {
                    let local_last_term = node.last_log_term();
                    let local_last_index = node.last_log_index();

                    // Raft §5.4:
                    // If the logs have last entries with different terms, then the log with the
                    // later term is more up-to-date. If the logs end with the same term, then
                    // whichever log is longer is more up-to-date.
                    let is_up_to_date = if req.last_log_term != local_last_term {
                        req.last_log_term > local_last_term
                    } else {
                        req.last_log_index >= local_last_index
                    };

                    if is_up_to_date {
                        vote_granted = true;
                        node.vote_for(candidate_id);
                        info!(
                            "Granting vote to candidate {} for term {}",
                            candidate_id, req.term
                        );
                    } else {
                        debug!(
                            "Rejecting vote for candidate {}: candidate's log is not up-to-date \
                             (local: index {}, term {}; remote: index {}, term {})",
                            candidate_id,
                            local_last_index,
                            local_last_term,
                            req.last_log_index,
                            req.last_log_term
                        );
                    }
                }
            }
            RaftNodeState::Candidate(_) | RaftNodeState::Leader(_) => {
                // If term is equal, we already voted for ourselves or are
                // leading. If term was higher, we transitioned
                // to Follower above.
            }
            RaftNodeState::Poisoned => return Err(self.poisoned_status()),
        };

        Ok(Response::new(RequestVoteResponse {
            cluster_id: self.cluster_id_as_str().to_string(),
            term: state_guard
                .current_term()
                .map_err(|_| self.poisoned_status())?,
            vote_granted,
        }))
    }

    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let req = request.into_inner();
        self.verify_identity(&req.cluster_id)?;

        let leader_id = req
            .leader_id
            .parse::<NodeId>()
            .map_err(|_| self.invalid_node_id_status(&req.leader_id))?;

        let span = info_span!("append_entries", term = req.term, leader = %leader_id);
        let _enter = span.enter();

        let mut state_guard = self.state.write().await;
        self.verify_node_integrity(&state_guard)?;

        let current_term = state_guard
            .current_term()
            .map_err(|_| self.poisoned_status())?;

        // 1. Reply false if term < currentTerm (§5.1)
        if req.term < current_term {
            debug!(
                "Rejecting AppendEntries from {}: term {} is older than currentTerm {}",
                leader_id, req.term, current_term
            );
            return Ok(Response::new(AppendEntriesResponse {
                cluster_id: self.cluster_id_as_str().to_string(),
                term: current_term,
                success: false,
                last_log_index: 0,
            }));
        }

        // 2. Term-based state transitions
        if req.term > current_term {
            // §5.1: If term > currentTerm, transition to follower
            info!(
                "Received higher term ({}) from leader {}. Demoting to Follower.",
                req.term, leader_id
            );
            state_guard.transition(|old| old.into_follower(req.term, Some(leader_id)));
        } else if req.term == current_term {
            match &mut *state_guard {
                RaftNodeState::Candidate(_) => {
                    // §5.2: If Candidate receives AppendEntries from a leader of the SAME term,
                    // it recognizes the leader as legitimate and returns to follower state.
                    info!(
                        "Candidate recognizing leader {} for term {}. Returning to Follower.",
                        leader_id, req.term
                    );
                    state_guard.transition(|old| old.into_follower(req.term, Some(leader_id)));
                }
                RaftNodeState::Leader(_) => {
                    // FATAL INVARIANT VIOLATION: Two leaders for the same term!
                    // We prioritize Safety over Liveness. Detecting another leader for our own term
                    // implies a failure in the consensus logic or persistence layer. We must halt
                    // to prevent data corruption.
                    let msg = format!(
                        "CRITICAL SAFETY VIOLATION: Rival leader {} detected for term {}. Halting \
                         node to prevent state corruption.",
                        leader_id, req.term
                    );
                    error!("{}", msg);
                    panic!("{}", msg);
                }
                RaftNodeState::Follower(node) => {
                    node.state_mut().set_leader_id(Some(leader_id));
                }
                _ => {}
            }
        }

        // 3. Log Consistency Check (§5.3)
        // Note: We perform this only if the node is in a stable Follower state.
        // If we just transitioned from Candidate/Leader, we are now a Follower.
        state_guard.reset_heartbeat();

        let (success, last_log_index) = match &mut *state_guard {
            RaftNodeState::Follower(node) => {
                let mut success = true;

                // 2. Reply false if log doesn’t contain an entry at prevLogIndex whose term
                //    matches prevLogTerm (§5.3)
                if req.prev_log_index > 0 {
                    let local_term = node.get_term_at(req.prev_log_index);
                    if local_term != req.prev_log_term {
                        debug!(
                            "Rejecting AppendEntries: prevLogIndex {} has term mismatch (local \
                             {}, remote {})",
                            req.prev_log_index, local_term, req.prev_log_term
                        );
                        success = false;
                    }
                }

                if success {
                    // 3. If an existing entry conflicts with a new one (same index but different
                    //    terms), delete the existing entry and all that follow it (§5.3)
                    for entry in &req.entries {
                        let local_term = node.get_term_at(entry.index);
                        if local_term != 0 && local_term != entry.term {
                            info!(
                                "Log conflict detected at index {}. Truncating log.",
                                entry.index
                            );
                            let truncate_at = (entry.index - 1) as usize;
                            node.log_mut().truncate(truncate_at);
                            break;
                        }
                    }

                    // 4. Append any new entries not already in the log
                    for entry in req.entries {
                        let last_idx = node.last_log_index();
                        if entry.index > last_idx {
                            if entry.index != last_idx + 1 {
                                // This should be caught by the prevLogIndex check, but we are
                                // defensive.
                                error!(
                                    "CRITICAL: Non-contiguous log append attempted. index={}, \
                                     last={}",
                                    entry.index, last_idx
                                );
                                success = false;
                                break;
                            }
                            node.log_mut().push(entry);
                        }
                    }
                }

                if success {
                    // 5. If leaderCommit > commitIndex, set commitIndex = min(leaderCommit, index
                    //    of last new entry)
                    if req.leader_commit > node.commit_index() {
                        let last_new_idx = node.last_log_index();
                        let new_commit = std::cmp::min(req.leader_commit, last_new_idx);
                        node.set_commit_index(new_commit);
                        debug!("Updated commit_index to {}", new_commit);
                    }
                }

                (success, node.last_log_index())
            }
            _ => (false, 0), // Should have demoted above
        };

        Ok(Response::new(AppendEntriesResponse {
            cluster_id: self.cluster_id_as_str().to_string(),
            term: state_guard.current_term().unwrap_or(req.term),
            success,
            last_log_index,
        }))
    }
}

#[cfg(test)]
mod tests {
    use common::types::ClusterId;

    use super::*;
    use crate::node::Follower;
    use crate::node::RaftNode;

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    fn mock_dispatcher() -> ConsensusDispatcher {
        let id = mock_identity();
        let node = RaftNodeState::Follower(RaftNode::<Follower>::new(id.clone()));
        ConsensusDispatcher::new(id, Arc::new(RwLock::new(node)))
    }

    mod identity_guard {
        use super::*;

        #[tokio::test]
        async fn returns_err_when_cluster_id_mismatches() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(RequestVoteRequest {
                cluster_id: "wrong-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let result = dispatcher.request_vote(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }

    mod integrity_check {
        use super::*;

        #[tokio::test]
        async fn returns_err_when_poisoned() {
            let dispatcher = mock_dispatcher();

            // Force the node into a poisoned state for testing
            {
                let mut state_guard = dispatcher.state.write().await;
                *state_guard = RaftNodeState::Poisoned;
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let result = dispatcher.request_vote(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
        }

        #[tokio::test]
        async fn returns_err_when_identity_mismatches() {
            let id = mock_identity();
            // Create a node with a DIFFERENT identity (different node_id)
            let wrong_id = Arc::new(NodeIdentity::new(id.cluster_id().clone(), NodeId::new(99)));
            let node = RaftNodeState::Follower(RaftNode::<Follower>::new(wrong_id));

            let dispatcher = ConsensusDispatcher::new(id, Arc::new(RwLock::new(node)));

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let result = dispatcher.request_vote(req).await;
            assert!(result.is_err());
            // Should return Internal due to identity divergence
            assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
        }
    }

    mod request_vote {
        use super::*;

        #[tokio::test]
        async fn grants_vote_when_term_is_higher_and_not_voted() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Follower starts at term 0, so 1 is higher
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
            assert_eq!(response.term, 1);
        }

        #[tokio::test]
        async fn grants_vote_when_term_is_already_current_and_not_voted() {
            let dispatcher = mock_dispatcher();
            // Pre-initialize to term 1
            {
                let mut state = dispatcher.state.write().await;
                state.transition(|old| old.into_follower(1, None));
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Same as current term
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
            assert_eq!(response.term, 1);
        }

        #[tokio::test]
        async fn rejects_vote_when_term_is_older() {
            let dispatcher = mock_dispatcher();

            // First, update node to term 2
            {
                let mut state = dispatcher.state.write().await;
                state.transition(|old| old.into_follower(2, None));
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Older term
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
            assert_eq!(response.term, 2);
        }

        #[tokio::test]
        async fn rejects_vote_when_candidate_log_is_shorter_same_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 2 entries in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    node.log_mut().push(common::proto::v1::LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    });
                    node.log_mut().push(common::proto::v1::LogEntry {
                        index: 2,
                        term: 1,
                        data: vec![],
                    });
                }
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 1, // Shorter than local (2)
                last_log_term: 1,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
        }

        #[tokio::test]
        async fn rejects_vote_when_candidate_log_has_older_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 1 entry in term 2
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    node.log_mut().push(common::proto::v1::LogEntry {
                        index: 1,
                        term: 2,
                        data: vec![],
                    });
                }
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 2,
                candidate_id: "2".to_string(),
                last_log_index: 10, // Longer, but...
                last_log_term: 1,   // ...older term
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, false);
        }

        #[tokio::test]
        async fn grants_vote_when_candidate_log_is_longer_same_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 1 entry in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    node.log_mut().push(common::proto::v1::LogEntry {
                        index: 1,
                        term: 1,
                        data: vec![],
                    });
                }
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 2, // Longer than local (1)
                last_log_term: 1,
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
        }

        #[tokio::test]
        async fn grants_vote_when_candidate_log_has_newer_term() {
            let dispatcher = mock_dispatcher();

            // Populate local log: 10 entries in term 1
            {
                let mut state = dispatcher.state.write().await;
                if let RaftNodeState::Follower(node) = &mut *state {
                    for i in 1..=10 {
                        node.log_mut().push(common::proto::v1::LogEntry {
                            index: i,
                            term: 1,
                            data: vec![],
                        });
                    }
                }
            }

            let req = Request::new(RequestVoteRequest {
                cluster_id: "test-cluster".to_string(),
                term: 2,
                candidate_id: "2".to_string(),
                last_log_index: 1, // Shorter, but...
                last_log_term: 2,  // ...newer term
            });

            let response = dispatcher.request_vote(req).await.unwrap().into_inner();
            assert_eq!(response.vote_granted, true);
        }
    }

    mod append_entries {
        use std::time::Duration;

        use super::*;

        #[tokio::test]
        async fn returns_success_when_term_is_current() {
            let dispatcher = mock_dispatcher();

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
                term: 0,
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert_eq!(response.success, true);
            assert_eq!(response.term, 0);
        }

        #[tokio::test]
        async fn rejects_when_term_is_older() {
            let dispatcher = mock_dispatcher();

            // Update node to term 2
            {
                let mut state = dispatcher.state.write().await;
                state.transition(|old| old.into_follower(2, None));
            }

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Older
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert_eq!(response.success, false);
            assert_eq!(response.term, 2);
        }

        #[tokio::test]
        async fn demotes_candidate_on_equal_term() {
            let id = mock_identity();
            // Start as Follower term 0, transition to Candidate term 1
            let follower = RaftNode::<Follower>::new(id.clone());
            let candidate = follower.into_candidate();
            let dispatcher = ConsensusDispatcher::new(
                id,
                Arc::new(RwLock::new(RaftNodeState::Candidate(candidate))),
            );

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Equal to candidate term
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            let response = dispatcher.append_entries(req).await.unwrap().into_inner();
            assert_eq!(response.success, true);

            let state_guard = dispatcher.state.read().await;
            assert!(matches!(&*state_guard, RaftNodeState::Follower(_)));
            assert_eq!(state_guard.current_term().unwrap(), 1);
        }

        #[tokio::test]
        #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
        async fn panics_on_rival_leader_same_term() {
            let id = mock_identity();
            // Start as Leader term 1
            let follower = RaftNode::<Follower>::new(id.clone());
            let candidate = follower.into_candidate();
            let leader = candidate.into_leader(Vec::new());
            let dispatcher =
                ConsensusDispatcher::new(id, Arc::new(RwLock::new(RaftNodeState::Leader(leader))));

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
                term: 1, // Rival leader for same term
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            // This should panic
            let _ = dispatcher.append_entries(req).await;
        }

        #[tokio::test]
        async fn resets_election_timer() {
            let dispatcher = mock_dispatcher();

            // 1. Get initial heartbeat time
            let initial_heartbeat = {
                let guard = dispatcher.state.read().await;
                if let RaftNodeState::Follower(node) = &*guard {
                    node.state().last_heartbeat()
                } else {
                    panic!("Should be follower");
                }
            };

            // Small sleep to ensure time moves forward
            tokio::time::sleep(Duration::from_millis(5)).await;

            let req = Request::new(AppendEntriesRequest {
                cluster_id: "test-cluster".to_string(),
                term: 0,
                leader_id: "2".to_string(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            });

            dispatcher.append_entries(req).await.unwrap();

            // 2. Verify heartbeat time was updated
            let updated_heartbeat = {
                let guard = dispatcher.state.read().await;
                if let RaftNodeState::Follower(node) = &*guard {
                    node.state().last_heartbeat()
                } else {
                    panic!("Should be follower");
                }
            };

            assert!(updated_heartbeat > initial_heartbeat);
        }
    }
}
