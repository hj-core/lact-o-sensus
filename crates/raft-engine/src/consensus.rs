use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::AppendEntriesResponse;
use common::proto::v1::raft::RequestVoteRequest;
use common::proto::v1::raft::RequestVoteResponse;
use common::types::LogIndex;
use common::types::NodeId;
use common::types::Term;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rand::RngExt;
use tokio::time::sleep;
use tonic::Request;
use tonic::Status;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::config::Config;
use crate::engine::LogicalNode;
use crate::peer::PeerManager;
use crate::shell::ConsensusShell;

// --- Semantic vocabulary for the consensus engine ---

/// Type alias for internal gRPC results to avoid conflict with anyhow::Result.
type RpcResult<T> = std::result::Result<T, Status>;

/// Encapsulates the result of a single vote request.
#[derive(Debug)]
struct VoteOutcome {
    peer_id: NodeId,
    response: RequestVoteResponse,
}

/// Encapsulates the result of a single replication RPC and the metadata
/// needed for reconciliation.
#[derive(Debug)]
struct ReplicationOutcome {
    peer_id: NodeId,
    sent_prev_index: LogIndex,
    sent_entries_len: u64,
    response: AppendEntriesResponse,
}

/// Instruction for the main election timer loop.
#[derive(Debug, PartialEq)]
enum TimerAction {
    /// No significant event; restart the loop with a new timeout.
    Restart,
    /// Transition to Candidate and initiate a new election.
    StartElection,
}

/// Instruction for the vote tallying loop.
#[derive(Debug, PartialEq)]
enum VoteProcessingResult {
    /// Quorum has been reached and node has transitioned to Leader.
    QuorumReached,
    /// Node has been demoted to Follower due to a higher term.
    Demoted,
    /// Election continues; quorum not yet reached.
    Continue,
}

/// Instruction for the replication loop.
#[derive(Debug, PartialEq)]
enum ReplicationResult {
    /// Node has been demoted to Follower due to a higher term.
    Demoted,
    /// Replication continues for other peers.
    Continue,
}

// --- Background Task Orchestrators ---

/// Spawns a background task that manages randomized election timeouts.
pub fn spawn_election_timer(
    config: Arc<Config>,
    state: Arc<ConsensusShell>,
    peer_manager: Arc<PeerManager>,
) {
    tokio::spawn(async move {
        loop {
            // 1. Generate randomized timeout.
            let timeout = calculate_election_timeout(&config);

            // 2. Sleep for the full timeout duration.
            sleep(timeout).await;

            // 3. Evaluation: Check state and evaluate if election is needed.
            let action = {
                let guard = state.read().await;
                match &*guard {
                    LogicalNode::Follower(_) => handle_follower_tick(&guard, timeout),
                    LogicalNode::Candidate(_) => handle_candidate_tick(&guard),
                    LogicalNode::Leader(_) => handle_leader_tick(&guard),
                    LogicalNode::Poisoned => {
                        error!("Node is poisoned. Election timer stopping (ADR 009).");
                        return;
                    }
                }
            };

            // 4. Execution: If evaluation triggered an election, transition and campaign.
            if action == TimerAction::StartElection {
                initiate_transition_to_candidate(
                    config.clone(),
                    state.clone(),
                    peer_manager.clone(),
                )
                .await;
            }
        }
    });
}

/// Transitions the node to Candidate and spawns the election RPC task.
async fn initiate_transition_to_candidate(
    config: Arc<Config>,
    state: Arc<ConsensusShell>,
    peer_manager: Arc<PeerManager>,
) {
    let term = {
        let mut guard = state.write().await;
        let is_follower_or_candidate = matches!(
            &*guard,
            LogicalNode::Follower(_) | LogicalNode::Candidate(_)
        );
        if !is_follower_or_candidate {
            return;
        }
        let t = guard.current_term();
        guard.into_candidate();
        t + 1
    };

    info!(
        "Election timeout reached. Transitioned to Candidate for term {}",
        term
    );

    let state_clone = state.clone();
    let peer_manager_clone = peer_manager.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = initiate_election(config_clone, state_clone, peer_manager_clone).await {
            error!("Failed to initiate election: {}", e);
        }
    });
}

/// Initiates a Leader Election by requesting votes from all peers.
async fn initiate_election(
    config: Arc<Config>,
    state: Arc<ConsensusShell>,
    peer_manager: Arc<PeerManager>,
) -> Result<()> {
    // 1. Gather election parameters from the current state.
    let (term, node_id, last_log_index, last_log_term) = {
        let mut guard = state.write().await;
        (
            guard.current_term(),
            guard.node_id(),
            guard.last_log_index(),
            guard.last_log_term(),
        )
    };

    info!(
        "Campaigning for leadership in term {} (last_log: {}/{})",
        term, last_log_index, last_log_term
    );

    // 2. Request votes from all peers concurrently
    let peer_ids = peer_manager.peer_ids();
    let mut vote_stream = broadcast_vote_requests(
        &config,
        peer_manager.clone(),
        term,
        node_id,
        last_log_index,
        last_log_term,
    );

    // 3. Tally votes and handle term updates
    let mut votes_granted = 1; // Start with 1 (self-vote)
    let total_nodes = peer_ids.len() + 1;
    let quorum = (total_nodes / 2) + 1;

    while let Some(res) = vote_stream.next().await {
        match process_vote_response(&state, term, &peer_ids, res).await? {
            VoteProcessingResult::QuorumReached => return Ok(()),
            VoteProcessingResult::Demoted => return Ok(()),
            VoteProcessingResult::Continue => {
                // Fetch the current vote count from the formal state machine to ensure
                // consistency with the loop's local tally.
                let guard = state.read().await;
                if let LogicalNode::Candidate(n) = &*guard {
                    votes_granted = n.state().vote_count();
                }
            }
        }
    }

    // Loop finished without reaching quorum or being demoted.
    let still_candidate = {
        let guard = state.read().await;
        let current = guard.try_current_term().unwrap_or(Term::ZERO);
        matches!(&*guard, LogicalNode::Candidate(_) if current == term)
    };

    if still_candidate {
        info!(
            "Election failed for term {}: only {}/{} votes granted.",
            term, votes_granted, quorum
        );
    }

    Ok(())
}

/// Spawns a background task that periodically sends heartbeats or replicates
/// logs if the node is a Leader.
pub fn spawn_heartbeat_task(
    config: Arc<Config>,
    state: Arc<ConsensusShell>,
    peer_manager: Arc<PeerManager>,
) {
    let interval = config.raft.heartbeat_interval();
    tokio::spawn(async move {
        loop {
            sleep(interval).await;

            let node_state = state.read().await;
            match &*node_state {
                LogicalNode::Leader(_) => {
                    let state_clone = state.clone();
                    let peer_manager_clone = peer_manager.clone();
                    let config_clone = config.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            replicate_to_peers(config_clone, state_clone, peer_manager_clone).await
                        {
                            error!("Failed to replicate to peers: {}", e);
                        }
                    });
                }
                LogicalNode::Poisoned => {
                    error!("Node is poisoned. Heartbeat task stopping.");
                    return;
                }
                _ => {
                    // Not a leader, but the loop continues in case we become
                    // one
                }
            }
        }
    });
}

/// Sends AppendEntries RPCs to all peers, including log entries if they are
/// behind.
async fn replicate_to_peers(
    config: Arc<Config>,
    state: Arc<ConsensusShell>,
    peer_manager: Arc<PeerManager>,
) -> Result<()> {
    // 1. Gather global replication parameters.
    let (term, node_id, commit_index) = {
        let mut guard = state.write().await;
        (guard.current_term(), guard.node_id(), guard.commit_index())
    };

    // 2. Prepare and send AppendEntries concurrently to all peers.
    let mut response_stream = broadcast_append_entries(
        &config,
        peer_manager.clone(),
        state.clone(),
        term,
        node_id,
        commit_index,
    );

    // 3. Process responses as they arrive (Opportunistic demotion & index updates).
    while let Some(res) = response_stream.next().await {
        if process_append_entries_response(&state, term, res).await? == ReplicationResult::Demoted {
            return Ok(());
        }
    }

    Ok(())
}

// --- Specialized Sub-functions (Delegated Implementation) ---

/// Evaluates the Follower's election timer.
fn handle_follower_tick(state: &LogicalNode, timeout: Duration) -> TimerAction {
    match state {
        LogicalNode::Follower(node) => {
            let elapsed = node.state().last_heartbeat().elapsed();
            if elapsed >= timeout {
                TimerAction::StartElection
            } else {
                TimerAction::Restart
            }
        }
        _ => TimerAction::Restart,
    }
}

/// Evaluates the Candidate's election timer.
fn handle_candidate_tick(state: &LogicalNode) -> TimerAction {
    match state {
        LogicalNode::Candidate(_) => TimerAction::StartElection,
        _ => TimerAction::Restart,
    }
}

/// Evaluates the Leader's election timer.
fn handle_leader_tick(state: &LogicalNode) -> TimerAction {
    match state {
        LogicalNode::Leader(_) => TimerAction::Restart,
        _ => TimerAction::Restart,
    }
}

/// Calculates a randomized election timeout based on the configuration.
fn calculate_election_timeout(config: &Config) -> Duration {
    let mut rng = rand::rng();
    Duration::from_millis(
        rng.random_range(config.raft.election_timeout_min_ms..config.raft.election_timeout_max_ms),
    )
}

/// Issues concurrent RequestVote RPCs to all peers.
fn broadcast_vote_requests(
    config: &Config,
    peer_manager: Arc<PeerManager>,
    term: Term,
    node_id: NodeId,
    last_log_index: LogIndex,
    last_log_term: Term,
) -> FuturesUnordered<impl futures::Future<Output = RpcResult<VoteOutcome>>> {
    let peer_ids = peer_manager.peer_ids();
    let rpc_timeout = config.raft.rpc_timeout();

    peer_ids
        .into_iter()
        .map(|peer_id| {
            let peer_manager = peer_manager.clone();
            async move {
                match peer_manager.get_client(peer_id) {
                    Ok(mut client) => {
                        let mut request = Request::new(RequestVoteRequest::new(
                            term,
                            node_id,
                            last_log_index,
                            last_log_term,
                        ));
                        request.set_timeout(rpc_timeout);

                        match client.request_vote(request).await {
                            Ok(resp) => Ok(VoteOutcome {
                                peer_id,
                                response: resp.into_inner(),
                            }),
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(Status::internal(e.to_string())),
                }
            }
        })
        .collect()
}

/// Processes a single vote response and updates the node state.
async fn process_vote_response(
    state: &ConsensusShell,
    term: Term,
    peer_ids: &[NodeId],
    res: RpcResult<VoteOutcome>,
) -> Result<VoteProcessingResult> {
    let VoteOutcome {
        peer_id,
        response: resp,
    } = match res {
        Ok(val) => val,
        Err(e) => {
            debug!("Failed to get vote from peer: {}", e);
            return Ok(VoteProcessingResult::Continue);
        }
    };

    let mut guard = state.write().await;
    let resp_term = Term::new(resp.term);

    // 1. Term check and opportunistic demotion (§5.1)
    if resp_term > term {
        info!(
            "Found higher term ({}) during election. Demoting to Follower.",
            resp_term
        );
        guard.into_follower(resp_term, None);
        return Ok(VoteProcessingResult::Demoted);
    }

    // 2. Tally vote if granted and we are still campaigning for the same term
    if resp.vote_granted {
        let mut quorum_reached = false;
        let total_nodes = peer_ids.len() + 1;
        let quorum = (total_nodes / 2) + 1;

        if let LogicalNode::Candidate(node) = &mut *guard
            && node.current_term().unwrap_or(Term::ZERO) == term
        {
            node.state_mut().add_vote(peer_id);
            if node.state().vote_count() >= quorum {
                quorum_reached = true;
            }
        }

        if quorum_reached {
            info!(
                "Quorum reached early! Transitioning to Leader for term {}.",
                term
            );
            guard.into_leader(peer_ids.to_vec());
            return Ok(VoteProcessingResult::QuorumReached);
        }
    }

    Ok(VoteProcessingResult::Continue)
}

/// Issues concurrent AppendEntries RPCs to all peers.
fn broadcast_append_entries(
    config: &Config,
    peer_manager: Arc<PeerManager>,
    state: Arc<ConsensusShell>,
    term: Term,
    node_id: NodeId,
    commit_index: LogIndex,
) -> FuturesUnordered<impl futures::Future<Output = RpcResult<Option<ReplicationOutcome>>>> {
    let peer_ids = peer_manager.peer_ids();
    let rpc_timeout = config.raft.rpc_timeout();

    peer_ids
        .into_iter()
        .map(|peer_id| {
            let state = state.clone();
            let peer_manager = peer_manager.clone();
            async move {
                // Prepare the request for this specific peer
                let request = {
                    let mut guard = state.write().await;
                    match &mut *guard {
                        LogicalNode::Leader(_) => build_append_entries_request(
                            &mut guard,
                            peer_id,
                            term,
                            node_id,
                            commit_index,
                        ),
                        _ => return Ok(None),
                    }
                };

                let sent_prev_index = LogIndex::new(request.prev_log_index);
                let sent_entries_len = request.entries.len() as u64;

                match peer_manager.get_client(peer_id) {
                    Ok(mut client) => {
                        let mut req = Request::new(request);
                        req.set_timeout(rpc_timeout);

                        match client.append_entries(req).await {
                            Ok(resp) => Ok(Some(ReplicationOutcome {
                                peer_id,
                                sent_prev_index,
                                sent_entries_len,
                                response: resp.into_inner(),
                            })),
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(Status::internal(e.to_string())),
                }
            }
        })
        .collect()
}

/// Builds an AppendEntriesRequest for a specific peer based on its next_index.
fn build_append_entries_request(
    node: &mut LogicalNode,
    peer_id: NodeId,
    term: Term,
    node_id: NodeId,
    commit_index: LogIndex,
) -> AppendEntriesRequest {
    let next_idx = if let LogicalNode::Leader(n) = node {
        *n.state()
            .next_index()
            .get(&peer_id)
            .unwrap_or(&LogIndex::new(1))
    } else {
        LogIndex::new(1)
    };
    let last_log_idx = node.last_log_index();

    let prev_log_index = next_idx - 1;
    let prev_log_term = node.get_term_at(prev_log_index);

    let entries = node.read_entries(next_idx, last_log_idx);

    AppendEntriesRequest::new(
        term,
        node_id,
        prev_log_index,
        prev_log_term,
        entries,
        commit_index,
    )
}

/// Processes a single replication response and updates leader bookkeeping.
async fn process_append_entries_response(
    state: &ConsensusShell,
    term: Term,
    res: RpcResult<Option<ReplicationOutcome>>,
) -> Result<ReplicationResult> {
    let ReplicationOutcome {
        peer_id,
        sent_prev_index,
        sent_entries_len,
        response: resp,
    } = match res {
        Ok(Some(val)) => val,
        Ok(None) => return Ok(ReplicationResult::Continue),
        Err(e) => {
            debug!("Replication RPC failed for a peer: {}", e);
            return Ok(ReplicationResult::Continue);
        }
    };

    let mut guard = state.write().await;
    let resp_term = Term::new(resp.term);

    // 1. Term check and opportunistic demotion (§5.1)
    if resp_term > term {
        info!(
            "Found higher term ({}) from peer {}. Demoting to Follower.",
            resp_term, peer_id
        );
        guard.into_follower(resp_term, None);
        return Ok(ReplicationResult::Demoted);
    }

    // 2. Process replication success/failure if we are still the leader for this
    //    term
    let mut commit_index_updated = false;
    if let LogicalNode::Leader(node) = &mut *guard
        && node.current_term().unwrap_or(Term::ZERO) == term
    {
        if resp.success {
            let new_match = sent_prev_index + sent_entries_len;
            let new_next = new_match + 1;

            let current_match = *node
                .state()
                .match_index()
                .get(&peer_id)
                .unwrap_or(&LogIndex::ZERO);

            if new_match > current_match {
                node.state_mut().next_index_mut().insert(peer_id, new_next);
                node.state_mut()
                    .match_index_mut()
                    .insert(peer_id, new_match);
                commit_index_updated = true;
            }
        } else {
            let current_next = *node
                .state()
                .next_index()
                .get(&peer_id)
                .unwrap_or(&LogIndex::new(1));

            let last_log_index = LogIndex::new(resp.last_log_index);
            let new_next = if last_log_index > LogIndex::ZERO {
                std::cmp::min(current_next, last_log_index + 1)
            } else {
                (current_next - 1).max(LogIndex::new(1))
            };

            node.state_mut().next_index_mut().insert(peer_id, new_next);
            debug!(
                "Peer {} rejected AppendEntries (log mismatch). Retrying with next_index={}",
                peer_id, new_next
            );
        }
    }

    // 3. Opportunistically advance commit index if progress was made
    if commit_index_updated {
        update_leader_commit_index(&mut guard).await;
    }

    Ok(ReplicationResult::Continue)
}

/// Helper to update the Leader's commit index based on a quorum of peer
/// match_indices.
async fn update_leader_commit_index(node: &mut LogicalNode) {
    let last_idx = node.last_log_index();
    let current_term = node.current_term();
    let (median_idx, commit_idx) = if let LogicalNode::Leader(n) = node {
        let mut match_indices: Vec<LogIndex> = n.state().match_index().values().cloned().collect();
        match_indices.push(last_idx); // Include self
        match_indices.sort_unstable();

        // The index that is replicated on a majority of nodes.
        // For 3 nodes, index 1 (middle element of sorted [idx1, idx2, idx3]).
        let median = match_indices[(match_indices.len() - 1) / 2];
        (median, node.commit_index())
    } else {
        return;
    };

    if median_idx > commit_idx && node.get_term_at(median_idx) == current_term {
        info!("Quorum reached for log index {}. Committing.", median_idx);
        node.set_commit_index(median_idx).await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use common::raft_api::StateMachine;
    use common::types::ClusterId;
    use common::types::NodeId;
    use common::types::NodeIdentity;
    use tonic::async_trait;

    use super::*;
    use crate::engine::Follower;
    use crate::node::RaftNode;
    use crate::storage::MemoryStorage;

    #[derive(Debug, Default)]
    struct MockFsm;
    use common::types::errors::FsmError;
    #[async_trait]
    impl StateMachine for MockFsm {
        fn last_applied_index(&self) -> Result<LogIndex, FsmError> {
            Ok(LogIndex::ZERO)
        }

        async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), FsmError> {
            Ok(())
        }
    }

    fn mock_config(min_ms: u64, max_ms: u64) -> Arc<Config> {
        let toml_str = format!(
            r#"
            cluster_id = "test-cluster"
            node_id = 1
            listen_addr = "127.0.0.1:50051"
            data_dir = "data/node_1"
            peers = {{}}
            
            [raft]
            election_timeout_min_ms = {}
            election_timeout_max_ms = {}

            [policy]
            veto_addr = "http://127.0.0.1:50060"
            veto_timeout_ms = 1000
        "#,
            min_ms, max_ms
        );
        Arc::new(toml::from_str(&toml_str).unwrap())
    }

    fn mock_identity() -> Arc<NodeIdentity> {
        Arc::new(NodeIdentity::new(
            ClusterId::try_new("test-cluster").unwrap(),
            NodeId::new(1),
        ))
    }

    async fn setup() -> (Arc<Config>, Arc<ConsensusShell>, Arc<PeerManager>) {
        let config = mock_config(50, 100);
        let id = mock_identity();
        let fsm = Arc::new(MockFsm);
        let storage = Arc::new(MemoryStorage::new());
        let node = LogicalNode::Follower(RaftNode::<Follower>::new(id.clone(), fsm, storage));
        let state = Arc::new(ConsensusShell::new(node));
        let peer_manager = Arc::new(PeerManager::new(id, &HashMap::new()).unwrap());
        (config, state, peer_manager)
    }

    mod calculate_election_timeout {
        use super::*;

        #[test]
        fn returns_duration_within_configured_range() {
            let toml_str = r#"
                cluster_id = "test-cluster"
                node_id = 1
                listen_addr = "127.0.0.1:50051"
                data_dir = "data/node_1"
                peers = {}
                
                [raft]
                election_timeout_min_ms = 150
                election_timeout_max_ms = 300

                [policy]
                veto_addr = "http://127.0.0.1:50060"
                veto_timeout_ms = 1000
            "#;
            let config: Config = toml::from_str(toml_str).expect("Failed to parse mock config");

            for _ in 0..100 {
                let timeout = calculate_election_timeout(&config);
                assert!(timeout >= Duration::from_millis(150));
                assert!(timeout < Duration::from_millis(300));
            }
        }
    }

    mod spawn_election_timer {
        use super::*;

        #[tokio::test]
        async fn follower_transitions_to_candidate_on_timeout() {
            let (config, state, peer_manager) = setup().await;
            spawn_election_timer(config, state.clone(), peer_manager);

            // Wait for timeout (50-100ms) + buffer
            sleep(Duration::from_millis(250)).await;

            let guard = state.read().await;
            assert!(matches!(&*guard, LogicalNode::Candidate(_)));
        }

        #[tokio::test]
        async fn candidate_restarts_election_on_timeout() {
            let (config, state, peer_manager) = setup().await;

            // Start as Candidate
            {
                let mut guard = state.write().await;
                guard.into_candidate();
            }
            let initial_term = state.read().await.try_current_term().unwrap();

            spawn_election_timer(config, state.clone(), peer_manager);

            // Wait for timeout
            sleep(Duration::from_millis(250)).await;

            let guard = state.read().await;
            assert!(matches!(&*guard, LogicalNode::Candidate(_)));
            assert!(
                guard.try_current_term().unwrap() > initial_term,
                "Candidate should have incremented term due to timeout"
            );
        }

        #[tokio::test]
        async fn leader_timer_remains_active() {
            let (config, state, peer_manager) = setup().await;

            // Start as Leader
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![]);
            }

            spawn_election_timer(config, state.clone(), peer_manager);

            // Wait a bit while Leader to ensure the loop hits the Leader case
            sleep(Duration::from_millis(50)).await;

            // Demote to Follower
            {
                let mut guard = state.write().await;
                let term = guard.current_term();
                guard.into_follower(term, None);
            }

            // If the timer task returned when it was Leader (the bug), it won't trigger
            // now. Wait for timeout
            sleep(Duration::from_millis(300)).await;

            let guard = state.read().await;
            assert!(
                matches!(&*guard, LogicalNode::Candidate(_)),
                "Timer should have stayed active and triggered election after demotion \
                 (Reproduction of Leader Bug)"
            );
        }
    }

    mod process_vote_response {
        use super::*;

        #[tokio::test]
        async fn demotes_on_higher_term() {
            let (_, state, peer_manager) = setup().await;
            let term = Term::new(1);
            let peer_ids = peer_manager.peer_ids();

            let res = Ok(VoteOutcome {
                peer_id: NodeId::new(2),
                response: RequestVoteResponse::new(Term::new(2), false),
            });

            let result = process_vote_response(&state, term, &peer_ids, res)
                .await
                .unwrap();
            assert_eq!(result, VoteProcessingResult::Demoted);

            let guard = state.read().await;
            assert!(matches!(&*guard, LogicalNode::Follower(_)));
            assert_eq!(guard.try_current_term().unwrap(), Term::new(2));
        }

        #[tokio::test]
        async fn detects_quorum_reached() {
            let (_, state, _) = setup().await;
            let term = Term::new(1);
            let peer_ids = vec![NodeId::new(2), NodeId::new(3)]; // 3 nodes total

            // Transition to Candidate manually
            {
                let mut guard = state.write().await;
                guard.into_candidate();
            }

            let res = Ok(VoteOutcome {
                peer_id: NodeId::new(2),
                response: RequestVoteResponse::new(Term::new(1), true),
            });

            // 1 (self) + 1 (peer 2) = 2/3 (Quorum reached)
            let result = process_vote_response(&state, term, &peer_ids, res)
                .await
                .unwrap();
            assert_eq!(result, VoteProcessingResult::QuorumReached);

            let guard = state.read().await;
            assert!(matches!(&*guard, LogicalNode::Leader(_)));
        }
    }

    mod process_append_entries_response {
        use super::*;

        #[tokio::test]
        async fn advances_indices_on_success() {
            let (_, state, _) = setup().await;
            let term = Term::new(1);
            let peer_id = NodeId::new(2);

            // Transition to Leader manually
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![peer_id]);
            }

            let res = Ok(Some(ReplicationOutcome {
                peer_id,
                sent_prev_index: LogIndex::new(0),
                sent_entries_len: 1,
                response: AppendEntriesResponse::new(Term::new(1), true, LogIndex::new(0)),
            }));

            let result = process_append_entries_response(&state, term, res)
                .await
                .unwrap();
            assert_eq!(result, ReplicationResult::Continue);

            let guard = state.read().await;
            if let LogicalNode::Leader(node) = &*guard {
                assert_eq!(node.state().match_index().get(&peer_id).unwrap().value(), 1);
                assert_eq!(node.state().next_index().get(&peer_id).unwrap().value(), 2);
            } else {
                panic!("Should be leader");
            }
        }

        #[tokio::test]
        async fn optimizes_backoff_on_failure() {
            let (_, state, _) = setup().await;
            let term = Term::new(1);
            let peer_id = NodeId::new(2);

            // Transition to Leader manually
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![peer_id]);
            }

            // Manually set a high next_index to test optimization
            {
                let mut guard = state.write().await;
                if let LogicalNode::Leader(node) = &mut *guard {
                    node.state_mut()
                        .next_index_mut()
                        .insert(peer_id, LogIndex::new(11));
                }
            }

            let res = Ok(Some(ReplicationOutcome {
                peer_id,
                sent_prev_index: LogIndex::new(10),
                sent_entries_len: 0,
                response: AppendEntriesResponse::new(Term::new(1), false, LogIndex::new(5)), /* Peer is at index 5 */
            }));

            let _ = process_append_entries_response(&state, term, res)
                .await
                .unwrap();

            let guard = state.read().await;
            if let LogicalNode::Leader(node) = &*guard {
                // Should have jumped back to 5+1=6 instead of 10
                assert_eq!(node.state().next_index().get(&peer_id).unwrap().value(), 6);
            } else {
                panic!("Should be leader");
            }
        }
    }

    mod update_leader_commit_index {
        use super::*;

        #[tokio::test]
        async fn advances_commit_on_quorum() {
            let (_, state, _) = setup().await;
            let peer_id_2 = NodeId::new(2);
            let peer_id_3 = NodeId::new(3);

            // Transition to Leader manually and add dummy log entries
            {
                let mut guard = state.write().await;
                guard.into_candidate();
                guard.into_leader(vec![peer_id_2, peer_id_3]);
                if let LogicalNode::Leader(leader) = &mut *guard {
                    // Add 5 entries in current term
                    let entries: Vec<_> = (1..=5)
                        .map(|i| common::proto::v1::raft::LogEntry {
                            index: i,
                            term: 1,
                            data: vec![],
                        })
                        .collect();
                    leader.storage().append_entries(entries).unwrap();
                }
            }

            {
                let mut guard = state.write().await;
                if let LogicalNode::Leader(node) = &mut *guard {
                    // Node 1 (self) is at 5
                    // Node 2 is at 4
                    // Node 3 is at 1
                    node.state_mut()
                        .match_index_mut()
                        .insert(peer_id_2, LogIndex::new(4));
                    node.state_mut()
                        .match_index_mut()
                        .insert(peer_id_3, LogIndex::new(1));
                }
                update_leader_commit_index(&mut guard).await;
                assert_eq!(guard.commit_index().value(), 4);
            }
        }
    }
}
