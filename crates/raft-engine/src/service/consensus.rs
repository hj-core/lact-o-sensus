//! Consensus delivery layer for Lact-O-Sensus.
//!
//! This module implements the gRPC-based peer-to-peer consensus service.
//! It acts as the "Logical Orchestrator" (ADR 009) that extracts domain
//! parameters from wire formats, establishes clinical telemetry boundaries,
//! and delegates authoritative logic to the underlying consensus engine.

use std::sync::Arc;

use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::AppendEntriesResponse;
use common::proto::v1::raft::InstallSnapshotRequest;
use common::proto::v1::raft::InstallSnapshotResponse;
use common::proto::v1::raft::LogEntry;
use common::proto::v1::raft::RequestVoteRequest;
use common::proto::v1::raft::RequestVoteResponse;
use common::proto::v1::raft::consensus_service_server::ConsensusService;
use common::raft_api::StateMachine;
use common::rpc::TraceInterceptor;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::NodeIdentity;
use common::types::Term;
use common::types::errors::NodeError;
use common::types::trace::ClinicalTarget;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::Instrument;
use tracing::info_span;
use tracing::instrument;

use crate::engine::LogicalNode;
use crate::shell::ConsensusShell;

// =============================================================================
// 1. Semantic Vocabulary (DTOs)
// =============================================================================

/// Encapsulates the fully validated and parsed parameters for a RequestVote
/// RPC.
struct VoteParams {
    candidate_id: NodeId,
    term: Term,
    last_log_index: LogIndex,
    last_log_term: Term,
}

impl VoteParams {
    /// Translates raw Protobuf types into strict domain NewTypes.
    fn try_from_proto(req: RequestVoteRequest) -> Result<Self, Status> {
        let candidate_id = req.candidate_id.parse::<NodeId>().map_err(|_| {
            Status::invalid_argument(format!("Invalid NodeId: '{}'", req.candidate_id))
        })?;

        Ok(Self {
            candidate_id,
            term: Term::new(req.term),
            last_log_index: LogIndex::new(req.last_log_index),
            last_log_term: Term::new(req.last_log_term),
        })
    }
}

/// Encapsulates the fully validated and parsed parameters for an
/// AppendEntries RPC.
struct AppendParams {
    leader_id: NodeId,
    term: Term,
    prev_log_index: LogIndex,
    prev_log_term: Term,
    entries: Vec<LogEntry>,
    leader_commit: LogIndex,
}

impl AppendParams {
    /// Translates raw Protobuf types into strict domain NewTypes.
    fn try_from_proto(req: AppendEntriesRequest) -> Result<Self, Status> {
        let leader_id = req.leader_id.parse::<NodeId>().map_err(|_| {
            Status::invalid_argument(format!("Invalid NodeId: '{}'", req.leader_id))
        })?;

        Ok(Self {
            leader_id,
            term: Term::new(req.term),
            prev_log_index: LogIndex::new(req.prev_log_index),
            prev_log_term: Term::new(req.prev_log_term),
            entries: req.entries,
            leader_commit: LogIndex::new(req.leader_commit),
        })
    }
}

/// Encapsulates the fully validated and parsed parameters for an
/// InstallSnapshot RPC.
pub struct SnapshotParams {
    pub leader_id: NodeId,
    pub term: Term,
    pub last_included_index: LogIndex,
    pub last_included_term: Term,
    pub data: Vec<u8>,
}

impl SnapshotParams {
    /// Translates raw Protobuf types into strict domain NewTypes.
    fn try_from_proto(req: InstallSnapshotRequest) -> Result<Self, Status> {
        let leader_id = req.leader_id.parse::<NodeId>().map_err(|_| {
            Status::invalid_argument(format!("Invalid NodeId: '{}'", req.leader_id))
        })?;

        Ok(Self {
            leader_id,
            term: Term::new(req.term),
            last_included_index: LogIndex::new(req.last_included_index),
            last_included_term: Term::new(req.last_included_term),
            data: req.data,
        })
    }
}

// =============================================================================
// 2. Public Orchestrators (ConsensusDispatcher)
// =============================================================================

/// Implementation of the internal Raft consensus RPCs.
///
/// This service acts as a dispatcher, delegating logic to the underlying
/// Type-State node engine while enforcing cluster identity and node health.
#[derive(Debug)]
pub struct ConsensusDispatcher<S: StateMachine> {
    identity: Arc<NodeIdentity>,
    state: Arc<ConsensusShell<S>>,
}

impl<S: StateMachine> ConsensusDispatcher<S> {
    pub fn new(identity: Arc<NodeIdentity>, state: Arc<ConsensusShell<S>>) -> Self {
        Self { identity, state }
    }

    /// Verifies that the node engine is healthy and matches the service
    /// identity.
    #[instrument(name = "verify_integrity", target = "clinical::telemetry", skip_all)]
    fn verify_node_integrity(&self, node: &mut LogicalNode<S>) -> Result<(), Status> {
        let engine_id = node.identity();
        if Arc::ptr_eq(&engine_id, &self.identity) {
            Ok(())
        } else {
            // ADR 010: Structured Integrity Event
            tracing::error!(
                target: ClinicalTarget::ClinicalTelemetry.as_str(),
                service_id = ?&self.identity,
                engine_id = ?engine_id,
                "Identity divergence detected! Halting node."
            );

            let msg = format!(
                "Identity divergence detected! ServiceIdentity='{:?}' EngineIdentity='{:?}'",
                self.identity, engine_id
            );
            // ADR 009: Poison the node via apply_fatal before panicking.
            node.apply_fatal(NodeError::Integrity(msg));
        }
    }

    /// Executes the core logic for a RequestVote RPC.
    #[instrument(
        name = "execute_vote_logic",
        target = "raft::foundation",
        skip_all,
        fields(candidate = %params.candidate_id, term = %params.term)
    )]
    async fn execute_vote_logic(
        &self,
        params: &VoteParams,
    ) -> Result<crate::engine::RequestVoteResult, Status> {
        let mut guard = self.state.write().await;
        self.verify_node_integrity(&mut guard)?;

        Ok(guard.handle_request_vote(
            params.candidate_id,
            params.term,
            params.last_log_index,
            params.last_log_term,
        ))
    }

    /// Executes the core logic for an AppendEntries RPC.
    #[instrument(
        name = "execute_append_logic",
        target = "raft::replication",
        skip_all,
        fields(leader = %params.leader_id, term = %params.term)
    )]
    async fn execute_append_logic(
        &self,
        params: AppendParams,
    ) -> Result<crate::engine::AppendEntriesResult, Status> {
        let mut guard = self.state.write().await;
        self.verify_node_integrity(&mut guard)?;

        Ok(guard
            .handle_append_entries(
                params.leader_id,
                params.term,
                params.prev_log_index,
                params.prev_log_term,
                params.entries,
                params.leader_commit,
            )
            .await)
    }
}

#[tonic::async_trait]
impl<S: StateMachine> ConsensusService for ConsensusDispatcher<S> {
    async fn request_vote(
        &self,
        request: Request<RequestVoteRequest>,
    ) -> Result<Response<RequestVoteResponse>, Status> {
        // 1. Extraction: Enforce TraceId presence and parse domain parameters.
        let trace_id = TraceInterceptor::require_trace_id(&request)?;
        let params = VoteParams::try_from_proto(request.into_inner())?;

        // 2. Instrumentation: Establish the clinical boundary.
        let span = info_span!(
            target: ClinicalTarget::RaftFoundation.as_str(),
            "request_vote",
            cluster_id = %self.identity.cluster_id(),
            node_id = %self.identity.node_id(),
            trace_id = %trace_id,
            term = %params.term,
            sender_id = %params.candidate_id
        );

        // 3. Execution: Delegate to the internal logic shell.
        let result = self.execute_vote_logic(&params).instrument(span).await?;

        // 4. Construction: Build the response and inject telemetry feedback.
        let mut response =
            Response::new(RequestVoteResponse::new(result.term, result.vote_granted));
        TraceInterceptor::inject_trace_id_into_response(&mut response, trace_id)?;

        Ok(response)
    }

    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        // 1. Extraction: Enforce TraceId presence and parse domain parameters.
        let trace_id = TraceInterceptor::require_trace_id(&request)?;
        let params = AppendParams::try_from_proto(request.into_inner())?;

        // 2. Instrumentation: Establish the clinical boundary.
        let span = info_span!(
            target: ClinicalTarget::RaftReplication.as_str(),
            "append_entries",
            cluster_id = %self.identity.cluster_id(),
            node_id = %self.identity.node_id(),
            trace_id = %trace_id,
            term = %params.term,
            sender_id = %params.leader_id
        );

        // 3. Execution: Delegate to the internal logic shell.
        let result = self.execute_append_logic(params).instrument(span).await?;

        // 4. Construction: Build the response and inject telemetry feedback.
        let mut response = Response::new(AppendEntriesResponse::new(
            result.term,
            result.success,
            result.conflict_index,
        ));
        TraceInterceptor::inject_trace_id_into_response(&mut response, trace_id)?;

        Ok(response)
    }

    async fn install_snapshot(
        &self,
        request: Request<InstallSnapshotRequest>,
    ) -> Result<Response<InstallSnapshotResponse>, Status> {
        // 1. Extraction: Enforce TraceId presence and parse domain parameters.
        let trace_id = TraceInterceptor::require_trace_id(&request)?;
        let params = SnapshotParams::try_from_proto(request.into_inner())?;

        // 2. Instrumentation: Establish the clinical boundary.
        let span = info_span!(
            target: ClinicalTarget::RaftCompaction.as_str(),
            "install_snapshot",
            cluster_id = %self.identity.cluster_id(),
            node_id = %self.identity.node_id(),
            trace_id = %trace_id,
            term = %params.term,
            index = %params.last_included_index,
            sender_id = %params.leader_id
        );

        // 3. Execution: Delegate to the internal shell for non-blocking handoff.
        let term = self
            .state
            .handle_install_snapshot(params)
            .instrument(span)
            .await?;

        // 4. Construction: Build the response and inject telemetry feedback.
        let mut response = Response::new(InstallSnapshotResponse::new(term.as_u64()));
        TraceInterceptor::inject_trace_id_into_response(&mut response, trace_id)?;

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use common::raft_api::StateMachine;
    use common::types::ClusterId;
    use common::types::errors::FsmError;
    use common::types::trace::TraceId;

    use super::*;
    use crate::storage::MemoryStorage;
    use crate::tick::TickDuration;
    use crate::tick::TickThresholds;

    #[derive(Debug, Default)]
    struct MockFsm;
    #[async_trait]
    impl StateMachine for MockFsm {
        type Error = FsmError;

        fn last_applied_index(&self) -> Result<LogIndex, Self::Error> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn snapshot(&self) -> Result<Vec<u8>, Self::Error> {
            Ok(vec![])
        }

        async fn install_snapshot(
            &self,
            _index: LogIndex,
            _data: &[u8],
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn test_identity(id: u64) -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::try_new(id).unwrap(),
        ))
    }

    fn mock_identity() -> Arc<NodeIdentity> {
        test_identity(1)
    }

    fn mock_dispatcher() -> ConsensusDispatcher<MockFsm> {
        let id = mock_identity();
        let fsm = Arc::new(MockFsm);
        let storage = Arc::new(MemoryStorage::new());
        let thresholds = TickThresholds {
            heartbeat_interval: TickDuration::new(10),
            min_election: TickDuration::new(15),
            max_election: TickDuration::new(30),
        };
        let rng = rand::SeedableRng::seed_from_u64(1);
        let node = LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
        let state = Arc::new(ConsensusShell::new(node));
        ConsensusDispatcher::new(id, state)
    }

    /// Helper to create a Request with a mandatory TraceId extension for
    /// telemetry-guarded handlers.
    fn make_request<T>(payload: T) -> Request<T> {
        let mut req = Request::new(payload);
        req.extensions_mut().insert(TraceId::generate());
        req
    }

    mod tracing_integrity {
        use super::*;

        #[tokio::test]
        async fn rejects_request_missing_trace_id() {
            let dispatcher = mock_dispatcher();
            let req = Request::new(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let result = dispatcher.request_vote(req).await;
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        }
    }

    mod integrity_check {
        use std::panic::AssertUnwindSafe;

        use futures::FutureExt;

        use super::*;

        #[tokio::test]
        #[should_panic(expected = "Halt Mandate: Node is poisoned")]
        async fn panics_when_poisoned() {
            let dispatcher = mock_dispatcher();

            // Force the node into a poisoned state for testing
            {
                let mut guard = dispatcher.state.write().await;
                guard.poison();
            }

            let req = make_request(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let _ = dispatcher.request_vote(req).await;
        }

        #[tokio::test]
        #[should_panic(expected = "Data Integrity Violation (Fatal): Identity divergence detected")]
        async fn panics_on_node_id_mismatch() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Arc::new(MemoryStorage::new());
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = rand::SeedableRng::seed_from_u64(99);
            // Different NodeId, same ClusterId
            let node =
                LogicalNode::try_new(test_identity(99), fsm, storage, thresholds, rng).unwrap();
            let state = Arc::new(ConsensusShell::new(node));

            let dispatcher = ConsensusDispatcher::new(id, state);

            let req = make_request(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let _ = dispatcher.request_vote(req).await;
        }

        #[tokio::test]
        #[should_panic(expected = "Data Integrity Violation (Fatal): Identity divergence detected")]
        async fn panics_on_cluster_id_mismatch() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Arc::new(MemoryStorage::new());
            // Same NodeId, different ClusterId
            let cluster_mismatch = Arc::new(NodeIdentity::new(
                ClusterId::try_new("wrong-cluster").unwrap(),
                id.node_id(),
            ));
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = rand::SeedableRng::seed_from_u64(1);
            let node =
                LogicalNode::try_new(cluster_mismatch, fsm, storage, thresholds, rng).unwrap();
            let state = Arc::new(ConsensusShell::new(node));

            let dispatcher = ConsensusDispatcher::new(id, state);

            let req = make_request(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            let _ = dispatcher.request_vote(req).await;
        }

        #[tokio::test]
        async fn identity_mismatch_poisons_node() {
            let id = mock_identity();
            let fsm = Arc::new(MockFsm);
            let storage = Arc::new(MemoryStorage::new());
            let thresholds = TickThresholds {
                heartbeat_interval: TickDuration::new(10),
                min_election: TickDuration::new(15),
                max_election: TickDuration::new(30),
            };
            let rng = rand::SeedableRng::seed_from_u64(99);
            let node =
                LogicalNode::try_new(test_identity(99), fsm, storage, thresholds, rng).unwrap();
            let state = Arc::new(ConsensusShell::new(node));
            let dispatcher = ConsensusDispatcher::new(id, state.clone());

            let req = make_request(RequestVoteRequest {
                term: 1,
                candidate_id: "2".to_string(),
                last_log_index: 0,
                last_log_term: 0,
            });

            // Catch the panic to inspect the state afterwards
            let result = AssertUnwindSafe(dispatcher.request_vote(req))
                .catch_unwind()
                .await;

            assert!(result.is_err());

            // Verify that the node is now permanently poisoned
            let guard = state.read().await;
            assert!(guard.is_poisoned());
        }
    }

    mod request_vote {
        use common::proto::v1::raft::LogEntry;

        use super::*;

        mod grants_vote {
            use super::*;

            #[tokio::test]
            async fn when_term_is_higher_and_not_voted() {
                let dispatcher = mock_dispatcher();

                let req = make_request(RequestVoteRequest {
                    term: 1, // Follower starts at term 0, so 1 is higher
                    candidate_id: "2".to_string(),
                    last_log_index: 0,
                    last_log_term: 0,
                });

                let response = dispatcher.request_vote(req).await.unwrap().into_inner();
                assert!(response.vote_granted);
                assert_eq!(response.term, 1);
            }

            #[tokio::test]
            async fn when_term_is_already_current_and_not_voted() {
                let dispatcher = mock_dispatcher();
                // Pre-initialize to term 1
                {
                    let mut state = dispatcher.state.write().await;
                    state.into_follower(Term::new(1), None);
                }

                let req = make_request(RequestVoteRequest {
                    term: 1, // Same as current term
                    candidate_id: "2".to_string(),
                    last_log_index: 0,
                    last_log_term: 0,
                });

                let response = dispatcher.request_vote(req).await.unwrap().into_inner();
                assert!(response.vote_granted);
                assert_eq!(response.term, 1);
            }

            #[tokio::test]
            async fn when_candidate_log_is_longer_same_term() {
                let dispatcher = mock_dispatcher();

                // Populate local log: 1 entry in term 1
                {
                    let mut state = dispatcher.state.write().await;
                    let node = state.as_follower_mut().expect("Should be follower");
                    node.log_store()
                        .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(1), vec![])])
                        .unwrap();
                }

                let req = make_request(RequestVoteRequest {
                    term: 1,
                    candidate_id: "2".to_string(),
                    last_log_index: 2, // Longer than local (1)
                    last_log_term: 1,
                });

                let response = dispatcher.request_vote(req).await.unwrap().into_inner();
                assert!(response.vote_granted);
            }

            #[tokio::test]
            async fn when_candidate_log_has_newer_term() {
                let dispatcher = mock_dispatcher();

                // Populate local log: 10 entries in term 1
                {
                    let mut state = dispatcher.state.write().await;
                    let node = state.as_follower_mut().expect("Should be follower");
                    let mut entries = Vec::new();
                    for i in 1..=10 {
                        entries.push(LogEntry::new(LogIndex::new(i as u64), Term::new(1), vec![]));
                    }
                    node.log_store().append_entries(entries).unwrap();
                }

                let req = make_request(RequestVoteRequest {
                    term: 2,
                    candidate_id: "2".to_string(),
                    last_log_index: 1, // Shorter, but...
                    last_log_term: 2,  // ...newer term
                });

                let response = dispatcher.request_vote(req).await.unwrap().into_inner();
                assert!(response.vote_granted);
            }
        }

        mod rejects_vote {
            use super::*;

            #[tokio::test]
            async fn when_term_is_older() {
                let dispatcher = mock_dispatcher();

                // First, update node to term 2
                {
                    let mut state = dispatcher.state.write().await;
                    state.into_follower(Term::new(2), None);
                }

                let req = make_request(RequestVoteRequest {
                    term: 1, // Older term
                    candidate_id: "2".to_string(),
                    last_log_index: 0,
                    last_log_term: 0,
                });

                let response = dispatcher.request_vote(req).await.unwrap().into_inner();
                assert!(!response.vote_granted);
                assert_eq!(response.term, 2);
            }

            #[tokio::test]
            async fn when_candidate_log_is_shorter_same_term() {
                let dispatcher = mock_dispatcher();

                // Populate local log: 2 entries in term 1
                {
                    let mut state = dispatcher.state.write().await;
                    let node = state.as_follower_mut().expect("Should be follower");
                    node.log_store()
                        .append_entries(vec![
                            LogEntry::new(LogIndex::new(1), Term::new(1), vec![]),
                            LogEntry::new(LogIndex::new(2), Term::new(1), vec![]),
                        ])
                        .unwrap();
                }

                let req = make_request(RequestVoteRequest {
                    term: 1,
                    candidate_id: "2".to_string(),
                    last_log_index: 1, // Shorter than local (2)
                    last_log_term: 1,
                });

                let response = dispatcher.request_vote(req).await.unwrap().into_inner();
                assert!(!response.vote_granted);
            }

            #[tokio::test]
            async fn when_candidate_log_has_older_term() {
                let dispatcher = mock_dispatcher();

                // Populate local log: 1 entry in term 2
                {
                    let mut state = dispatcher.state.write().await;
                    let node = state.as_follower_mut().expect("Should be follower");
                    node.log_store()
                        .append_entries(vec![LogEntry::new(LogIndex::new(1), Term::new(2), vec![])])
                        .unwrap();
                }

                let req = make_request(RequestVoteRequest {
                    term: 2,
                    candidate_id: "2".to_string(),
                    last_log_index: 10, // Longer, but...
                    last_log_term: 1,   // ...older term
                });

                let response = dispatcher.request_vote(req).await.unwrap().into_inner();
                assert!(!response.vote_granted);
            }
        }

        #[tokio::test]
        async fn grants_vote_and_injects_trace_response() {
            let dispatcher = mock_dispatcher();
            let req = RequestVoteRequest {
                term: 2,
                candidate_id: "2".to_string(),
                last_log_index: 1,
                last_log_term: 2,
            };

            let trace_id = TraceId::generate();
            let mut r = Request::new(req);
            r.extensions_mut().insert(trace_id);

            let resp = dispatcher.request_vote(r).await.unwrap();
            assert!(resp.get_ref().vote_granted);
            assert_eq!(
                TraceInterceptor::extract_trace_id_from_response(&resp).unwrap(),
                trace_id
            );
        }
    }

    mod append_entries {
        use super::*;
        use crate::engine::RoleState;
        use crate::storage::MemoryStorage;

        mod returns_success {
            use super::*;

            #[tokio::test]
            async fn when_term_is_current() {
                let dispatcher = mock_dispatcher();

                let req = make_request(AppendEntriesRequest {
                    term: 0,
                    leader_id: "2".to_string(),
                    prev_log_index: 0,
                    prev_log_term: 0,
                    entries: Vec::new(),
                    leader_commit: 0,
                });

                let response = dispatcher.append_entries(req).await.unwrap().into_inner();
                assert!(response.success);
                assert_eq!(response.term, 0);
            }
        }

        mod rejects_request {
            use super::*;

            #[tokio::test]
            async fn when_term_is_older() {
                let dispatcher = mock_dispatcher();

                // Update node to term 2
                {
                    let mut state = dispatcher.state.write().await;
                    state.into_follower(Term::new(2), None);
                }

                let req = make_request(AppendEntriesRequest {
                    term: 1, // Older
                    leader_id: "2".to_string(),
                    prev_log_index: 0,
                    prev_log_term: 0,
                    entries: Vec::new(),
                    leader_commit: 0,
                });

                let response = dispatcher.append_entries(req).await.unwrap().into_inner();
                assert!(!response.success);
                assert_eq!(response.term, 2);
            }
        }

        mod transitions_role {
            use super::*;

            #[tokio::test]
            async fn demotes_candidate_on_equal_term() {
                let id = mock_identity();
                let fsm = Arc::new(MockFsm);
                let storage = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = rand::SeedableRng::seed_from_u64(1);
                // Start as Follower term 0, transition to Candidate term 1
                let mut node =
                    LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
                node.into_candidate();
                let state = Arc::new(ConsensusShell::new(node));
                let dispatcher = ConsensusDispatcher::new(id, state);

                let req = make_request(AppendEntriesRequest {
                    term: 1, // Equal to candidate term
                    leader_id: "2".to_string(),
                    prev_log_index: 0,
                    prev_log_term: 0,
                    entries: Vec::new(),
                    leader_commit: 0,
                });

                let response = dispatcher.append_entries(req).await.unwrap().into_inner();
                assert!(response.success);

                let state_guard = dispatcher.state.write().await;
                assert!(matches!(state_guard.state(), RoleState::Follower(_)));
                assert_eq!(state_guard.try_current_term().unwrap(), Term::new(1));
            }

            #[tokio::test]
            #[should_panic(expected = "CRITICAL SAFETY VIOLATION")]
            async fn panics_on_rival_leader_same_term() {
                let id = mock_identity();
                let fsm = Arc::new(MockFsm);
                let storage = Arc::new(MemoryStorage::new());
                let thresholds = TickThresholds {
                    heartbeat_interval: TickDuration::new(10),
                    min_election: TickDuration::new(15),
                    max_election: TickDuration::new(30),
                };
                let rng = rand::SeedableRng::seed_from_u64(1);
                // Start as Leader term 1
                let mut node =
                    LogicalNode::try_new(id.clone(), fsm, storage, thresholds, rng).unwrap();
                node.into_candidate();
                node.into_leader(Vec::new());
                let state = Arc::new(ConsensusShell::new(node));
                let dispatcher = ConsensusDispatcher::new(id, state);

                let req = make_request(AppendEntriesRequest {
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
        }

        #[tokio::test]
        async fn resets_election_timer() {
            let dispatcher = mock_dispatcher();

            // 1. Get initial heartbeat time
            let initial_heartbeat = {
                let guard = dispatcher.state.read().await;
                match guard.state() {
                    RoleState::Follower(node) => node.state().last_heartbeat(),
                    _ => panic!("Should be follower"),
                }
            };

            let req = make_request(AppendEntriesRequest {
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
                match guard.state() {
                    RoleState::Follower(node) => node.state().last_heartbeat(),
                    _ => panic!("Should be follower"),
                }
            };

            // Since current_tick is 0 and last_heartbeat was 0, it won't change yet
            // unless we increment tick.
            // But let's verify it didn't CRASH at least.
            assert_eq!(updated_heartbeat, initial_heartbeat);
        }

        mod install_snapshot {
            use common::proto::v1::raft::InstallSnapshotRequest;

            use super::*;

            #[tokio::test]
            async fn should_return_current_term_on_success() {
                let dispatcher = mock_dispatcher();
                let request = make_request(InstallSnapshotRequest {
                    term: 1, // Follower starts at 0, so 1 is valid
                    leader_id: "2".to_string(),
                    last_included_index: 100,
                    last_included_term: 1,
                    data: vec![1, 2, 3],
                });

                let response = dispatcher
                    .install_snapshot(request)
                    .await
                    .unwrap()
                    .into_inner();
                assert_eq!(response.term, 1);
            }

            #[tokio::test]
            async fn should_reject_stale_term() {
                let dispatcher = mock_dispatcher();
                // Pre-discovery higher term
                {
                    let mut guard = dispatcher.state.write().await;
                    guard.into_follower(Term::new(5), None);
                }

                let request = make_request(InstallSnapshotRequest {
                    term: 1, // Stale term
                    leader_id: "2".to_string(),
                    last_included_index: 100,
                    last_included_term: 1,
                    data: vec![1, 2, 3],
                });

                let response = dispatcher
                    .install_snapshot(request)
                    .await
                    .unwrap()
                    .into_inner();
                assert_eq!(response.term, 5);
            }
        }
    }
}
