use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::proto::v1::raft::AppendEntriesRequest;
use common::proto::v1::raft::RequestVoteRequest;
use common::types::LogIndex;
use common::types::Term;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rand::RngExt;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tonic::Request;
use tonic::Status;
use tracing::debug;
use tracing::error;
use tracing::info;

use crate::config::Config;
use crate::node::Leader;
use crate::node::RaftNode;
use crate::node::RaftNodeState;
use crate::peer::PeerManager;

/// Spawns a background task that manages randomized election timeouts.
pub fn spawn_election_timer(
    config: Arc<Config>,
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) {
    tokio::spawn(async move {
        loop {
            // 1. Generate randomized timeout and identify signal.
            let timeout = calculate_election_timeout(&config);

            let heartbeat_signal = {
                let guard = state.read().await;
                match &*guard {
                    RaftNodeState::Follower(node) => Some(node.state().heartbeat_signal().clone()),
                    RaftNodeState::Candidate(_) | RaftNodeState::Leader(_) => None,
                    RaftNodeState::Poisoned => {
                        panic!("HALT: Node is poisoned (ADR 001)");
                    }
                }
            };

            // 2. Wait: Race timeout vs. heartbeat signal (if Follower).
            let action = if let Some(signal) = heartbeat_signal {
                tokio::select! {
                    _ = sleep(timeout) => {
                        let guard = state.read().await;
                        handle_follower_tick(&*guard, timeout)
                    }
                    _ = signal.notified() => {
                        TimerAction::Restart
                    }
                }
            } else {
                sleep(timeout).await;
                let guard = state.read().await;
                match &*guard {
                    RaftNodeState::Candidate(_) => handle_candidate_tick(&*guard),
                    RaftNodeState::Leader(_) => handle_leader_tick(&*guard),
                    RaftNodeState::Poisoned => panic!("HALT: Node is poisoned (ADR 001)"),
                    _ => TimerAction::Restart,
                }
            };

            // 3. Execution: If evaluation triggered an election, transition and campaign.
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
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) {
    let mut state_guard = state.write().await;
    let new_term = match &*state_guard {
        RaftNodeState::Follower(n) => n.current_term() + 1,
        RaftNodeState::Candidate(n) => n.current_term() + 1,
        _ => return, // Only Followers and Candidates can start elections
    };

    info!(
        "Election timeout reached. Transitioning to Candidate for term {}",
        new_term
    );

    state_guard.transition(|old| match old {
        RaftNodeState::Follower(n) => RaftNodeState::Candidate(n.into_candidate()),
        RaftNodeState::Candidate(n) => RaftNodeState::Candidate(n.into_restarted_candidate()),
        other => other,
    });

    let state_clone = state.clone();
    let peer_manager_clone = peer_manager.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = initiate_election(config_clone, state_clone, peer_manager_clone).await {
            error!("Failed to initiate election: {}", e);
        }
    });
}

/// Evaluates the Follower's election timer.
fn handle_follower_tick(state: &RaftNodeState, timeout: Duration) -> TimerAction {
    match state {
        RaftNodeState::Follower(node) => {
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
fn handle_candidate_tick(state: &RaftNodeState) -> TimerAction {
    match state {
        RaftNodeState::Candidate(_) => TimerAction::StartElection,
        _ => TimerAction::Restart,
    }
}

/// Evaluates the Leader's election timer.
fn handle_leader_tick(state: &RaftNodeState) -> TimerAction {
    match state {
        RaftNodeState::Leader(_) => TimerAction::Restart,
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

/// Instruction for the main election timer loop.
#[derive(Debug, PartialEq)]
enum TimerAction {
    /// No significant event; restart the loop with a new timeout.
    Restart,
    /// Transition to Candidate and initiate a new election.
    StartElection,
}

/// Initiates a Leader Election by requesting votes from all peers.
async fn initiate_election(
    config: Arc<Config>,
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) -> Result<()> {
    // 1. Gather election parameters from the current state.
    let (term, node_id, last_log_index, last_log_term) = {
        let guard = state.read().await;
        let nid = guard.node_id()?;

        let (last_idx, last_term) = match &*guard {
            RaftNodeState::Candidate(n) => (n.last_log_index(), n.last_log_term()),
            _ => (LogIndex::ZERO, Term::ZERO), // Should be Candidate
        };

        (guard.current_term()?, nid, last_idx, last_term)
    };

    info!(
        "Campaigning for leadership in term {} (last_log: {}/{})",
        term, last_log_index, last_log_term
    );

    // 2. Request votes from all peers concurrently
    let peer_ids = peer_manager.peer_ids();
    let vote_requests = peer_ids.clone().into_iter().map(|peer_id| {
        let peer_manager = peer_manager.clone();
        let node_id = node_id;
        let rpc_timeout = config.raft.rpc_timeout();

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
                        Ok(resp) => {
                            let resp = resp.into_inner();
                            Ok((peer_id, resp))
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(Status::internal(e.to_string())),
            }
        }
    });

    let mut vote_stream = vote_requests.collect::<FuturesUnordered<_>>();

    // 3. Tally votes and handle term updates
    let mut votes_granted = 1; // Start with 1 (self-vote)
    let total_nodes = peer_ids.len() + 1;
    let quorum = (total_nodes / 2) + 1;

    while let Some(res) = vote_stream.next().await {
        match res {
            Ok((peer_id, resp)) => {
                let resp_term = Term::new(resp.term);
                if resp_term > term {
                    info!(
                        "Found higher term ({}) during election. Demoting to Follower.",
                        resp_term
                    );
                    let mut guard = state.write().await;
                    guard.transition(|old| old.into_follower(resp_term, None));
                    return Ok(());
                }

                if resp.vote_granted {
                    let mut guard = state.write().await;
                    let mut is_leader = false;

                    guard.transition(|old| match old {
                        RaftNodeState::Candidate(mut n) if n.current_term() == term => {
                            n.state_mut().add_vote(peer_id);
                            // Synchronize local tally from the formal state machine
                            votes_granted = n.state().vote_count();

                            if votes_granted >= quorum {
                                is_leader = true;
                                RaftNodeState::Leader(n.into_leader(peer_ids.clone()))
                            } else {
                                RaftNodeState::Candidate(n)
                            }
                        }
                        other => other,
                    });

                    if is_leader {
                        info!(
                            "Quorum reached early ({} votes)! Transitioning to Leader for term {}.",
                            votes_granted, term
                        );
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                debug!("Failed to get vote from peer: {}", e);
            }
        }
    }

    // Loop finished without reaching quorum or being demoted.
    let still_candidate = {
        let guard = state.read().await;
        matches!(&*guard, RaftNodeState::Candidate(n) if n.current_term() == term)
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
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) {
    let interval = config.raft.heartbeat_interval();
    tokio::spawn(async move {
        loop {
            sleep(interval).await;

            let node_state = state.read().await;
            match &*node_state {
                RaftNodeState::Leader(_) => {
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
                RaftNodeState::Poisoned => {
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
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) -> Result<()> {
    // 1. Gather global replication parameters.
    let (term, node_id, commit_index) = {
        let guard = state.read().await;
        let nid = guard.node_id()?;
        (guard.current_term()?, nid, guard.commit_index()?)
    };

    let peer_ids = peer_manager.peer_ids();
    let rpc_timeout = config.raft.rpc_timeout();

    // 2. Prepare and send AppendEntries concurrently to all peers.
    let replication_requests = peer_ids.into_iter().map(|peer_id| {
        let state = state.clone();
        let peer_manager = peer_manager.clone();
        let node_id = node_id;

        async move {
            // a. Prepare the request for this specific peer
            let request = {
                let guard = state.read().await;
                match &*guard {
                    RaftNodeState::Leader(node) => {
                        let next_idx = *node
                            .state()
                            .next_index()
                            .get(&peer_id)
                            .unwrap_or(&LogIndex::new(1));
                        let last_log_idx = node.last_log_index();

                        let prev_log_index = next_idx - 1;
                        let prev_log_term = node.get_term_at(prev_log_index);

                        // Collect entries starting from next_idx
                        let entries = if last_log_idx >= next_idx {
                            // Logs are 1-indexed, so index 1 is at Vec index 0.
                            node.log()[(next_idx.value() as usize - 1)..].to_vec()
                        } else {
                            Vec::new()
                        };

                        AppendEntriesRequest::new(
                            term,
                            node_id,
                            prev_log_index,
                            prev_log_term,
                            entries,
                            commit_index,
                        )
                    }
                    _ => return Ok(None), // No longer leader
                }
            };

            // Capture metadata before the request is consumed by the tonic::Request
            let sent_prev_idx = LogIndex::new(request.prev_log_index);
            let sent_entries_len = request.entries.len() as u64;

            // b. Execute the RPC
            match peer_manager.get_client(peer_id) {
                Ok(mut client) => {
                    let mut req = Request::new(request);
                    req.set_timeout(rpc_timeout);

                    match client.append_entries(req).await {
                        Ok(resp) => {
                            let resp = resp.into_inner();
                            // Return peer_id and minimal metadata to avoid cloning log data
                            Ok(Some((peer_id, sent_prev_idx, sent_entries_len, resp)))
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(Status::internal(e.to_string())),
            }
        }
    });

    let mut response_stream = replication_requests.collect::<FuturesUnordered<_>>();

    // 3. Process responses as they arrive (Opportunistic demotion & index updates).
    while let Some(res) = response_stream.next().await {
        match res {
            Ok(Some((peer_id, sent_prev_idx, sent_entries_len, resp))) => {
                let mut guard = state.write().await;

                let resp_term = Term::new(resp.term);
                // §5.1: If term > currentTerm, demote immediately
                if resp_term > term {
                    info!(
                        "Found higher term ({}) from peer {}. Demoting to Follower.",
                        resp_term, peer_id
                    );
                    guard.transition(|old| old.into_follower(resp_term, None));
                    return Ok(());
                }

                if let RaftNodeState::Leader(node) = &mut *guard {
                    if resp.success {
                        // Update nextIndex and matchIndex for follower (§5.3)
                        let new_match = sent_prev_idx + sent_entries_len;
                        let new_next = new_match + 1;

                        // Monotonicity check: only update if we are moving forward
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
                        }

                        // Check for new commit point (§5.3, §5.4)
                        update_leader_commit_index(node).await;
                    } else {
                        // If AppendEntries fails because of log inconsistency:
                        // decrement nextIndex and retry (§5.3)
                        let current_next = *node
                            .state()
                            .next_index()
                            .get(&peer_id)
                            .unwrap_or(&LogIndex::new(1));

                        // Optimization: jump back based on peer's actual log state
                        let last_log_index = LogIndex::new(resp.last_log_index);
                        let new_next = if last_log_index > LogIndex::ZERO {
                            std::cmp::min(current_next, last_log_index + 1)
                        } else {
                            (current_next - 1).max(LogIndex::new(1))
                        };

                        node.state_mut().next_index_mut().insert(peer_id, new_next);
                        debug!(
                            "Peer {} rejected AppendEntries (log mismatch). Retrying with \
                             next_index={}",
                            peer_id, new_next
                        );
                    }
                }
            }
            Ok(None) => {} // Node was demoted during task preparation
            Err(e) => {
                debug!("Replication RPC failed for a peer: {}", e);
            }
        }
    }

    Ok(())
}

/// Helper to update the Leader's commit index based on a quorum of peer
/// match_indices.
async fn update_leader_commit_index(node: &mut RaftNode<Leader>) {
    let last_idx = node.last_log_index();
    let current_term = node.current_term();
    let mut match_indices: Vec<LogIndex> = node.state().match_index().values().cloned().collect();
    match_indices.push(last_idx); // Include self
    match_indices.sort_unstable();

    // The index that is replicated on a majority of nodes.
    // For 3 nodes, index 1 (middle element of sorted [idx1, idx2, idx3]).
    let quorum_idx = match_indices[(match_indices.len() - 1) / 2];

    if quorum_idx > node.commit_index() && node.get_term_at(quorum_idx) == current_term {
        info!("Quorum reached for log index {}. Committing.", quorum_idx);
        node.set_commit_index(quorum_idx).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use std::collections::HashMap;

        use common::types::ClusterId;
        use common::types::NodeId;

        use super::*;
        use crate::fsm::StateMachine;
        use crate::identity::NodeIdentity;
        use crate::node::Follower;
        use crate::node::RaftNode;

        #[derive(Debug, Default)]
        struct MockFsm;
        #[tonic::async_trait]
        impl StateMachine for MockFsm {
            async fn apply(&self, _index: LogIndex, _data: &[u8]) -> Result<(), Status> {
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

        async fn setup() -> (Arc<Config>, Arc<RwLock<RaftNodeState>>, Arc<PeerManager>) {
            let config = mock_config(50, 100);
            let id = mock_identity();
            let fsm = Arc::new(MockFsm::default());
            let state = Arc::new(RwLock::new(RaftNodeState::Follower(
                RaftNode::<Follower>::new(id.clone(), fsm),
            )));
            let peer_manager = Arc::new(PeerManager::new(id, &HashMap::new()).unwrap());
            (config, state, peer_manager)
        }

        #[tokio::test]
        async fn follower_transitions_to_candidate_on_timeout() {
            let (config, state, peer_manager) = setup().await;
            spawn_election_timer(config, state.clone(), peer_manager);

            // Wait for timeout (50-100ms) + buffer
            sleep(Duration::from_millis(250)).await;

            let guard = state.read().await;
            assert!(matches!(&*guard, RaftNodeState::Candidate(_)));
        }

        #[tokio::test]
        async fn candidate_restarts_election_on_timeout() {
            let (config, state, peer_manager) = setup().await;

            // Start as Candidate
            {
                let mut guard = state.write().await;
                guard.transition(|old| match old {
                    RaftNodeState::Follower(n) => RaftNodeState::Candidate(n.into_candidate()),
                    other => other,
                });
            }
            let initial_term = state.read().await.current_term().unwrap();

            spawn_election_timer(config, state.clone(), peer_manager);

            // Wait for timeout
            sleep(Duration::from_millis(250)).await;

            let guard = state.read().await;
            assert!(matches!(&*guard, RaftNodeState::Candidate(_)));
            assert!(
                guard.current_term().unwrap() > initial_term,
                "Candidate should have incremented term due to timeout"
            );
        }

        #[tokio::test]
        async fn leader_timer_remains_active() {
            let (config, state, peer_manager) = setup().await;

            // Start as Leader
            {
                let mut guard = state.write().await;
                guard.transition(|old| match old {
                    RaftNodeState::Follower(n) => {
                        RaftNodeState::Leader(n.into_candidate().into_leader(vec![]))
                    }
                    other => other,
                });
            }

            spawn_election_timer(config, state.clone(), peer_manager);

            // Wait a bit while Leader to ensure the loop hits the Leader case
            sleep(Duration::from_millis(50)).await;

            // Demote to Follower
            {
                let mut guard = state.write().await;
                let term = guard.current_term().unwrap();
                guard.transition(|old| old.into_follower(term, None));
            }

            // If the timer task returned when it was Leader (the bug), it won't trigger
            // now. Wait for timeout
            sleep(Duration::from_millis(300)).await;

            let guard = state.read().await;
            assert!(
                matches!(&*guard, RaftNodeState::Candidate(_)),
                "Timer should have stayed active and triggered election after demotion \
                 (Reproduction of Leader Bug)"
            );
        }

        #[tokio::test]
        async fn poisoned_state_triggers_halt() {
            let (config, state, peer_manager) = setup().await;

            // Start as poisoned
            {
                let mut guard = state.write().await;
                *guard = RaftNodeState::Poisoned;
            }

            // Wrap in tokio::spawn to capture panic
            let handle = tokio::spawn(async move {
                // We need to call the inner loop logic or a version that we can join.
                // For now, let's just use spawn_election_timer and see if it halts.
                spawn_election_timer(config, state, peer_manager);
                // The timer is spawned in another task inside
                // spawn_election_timer. This makes it hard to
                // join the specific timer loop.
            });

            handle.await.unwrap();
            // This test is currently a placeholder for the logic.
            // The actual implementation will use panic! which we'll verify in
            // Commit 2.
        }
    }
}
