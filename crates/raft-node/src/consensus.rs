use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::proto::v1::AppendEntriesRequest;
use common::proto::v1::RequestVoteRequest;
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
            // 1. Randomize timeout for this "tick".
            let timeout = {
                let mut rng = rand::rng();
                Duration::from_millis(rng.random_range(
                    config.raft.election_timeout_min_ms..config.raft.election_timeout_max_ms,
                ))
            };

            // 2. Extract the heartbeat signal if we are a Follower.
            let heartbeat_signal = {
                let guard = state.read().await;
                match &*guard {
                    RaftNodeState::Follower(node) => Some(node.state().heartbeat_signal().clone()),
                    RaftNodeState::Candidate(_) => None, // Candidates use standard timeout
                    RaftNodeState::Leader(_) => return,  // Leaders don't need the timer
                    RaftNodeState::Poisoned => {
                        error!("Node is poisoned. Election timer stopping.");
                        return;
                    }
                }
            };

            // 3. Reactive Wait: Either the timeout expires OR a heartbeat arrives.
            if let Some(signal) = heartbeat_signal {
                tokio::select! {
                    _ = sleep(timeout) => {
                        // Timeout reached. Verify it hasn't been reset just before the lock.
                        let mut state_guard = state.write().await;
                        if let RaftNodeState::Follower(node) = &*state_guard {
                            let elapsed = node.state().last_heartbeat().elapsed();
                            if elapsed >= timeout {
                                info!(
                                    "Election timeout reached ({:?}). Transitioning to Candidate for term {}",
                                    elapsed,
                                    node.current_term() + 1
                                );
                                state_guard.transition(|old| match old {
                                    RaftNodeState::Follower(n) => RaftNodeState::Candidate(n.into_candidate()),
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
                        }
                    }
                    _ = signal.notified() => {
                        // Heartbeat received! We restart the loop and pick a new timeout.
                        continue;
                    }
                }
            } else {
                // We are a Candidate. Standard sleep without signal reactivity.
                sleep(timeout).await;
                let mut state_guard = state.write().await;
                if let RaftNodeState::Candidate(node) = &*state_guard {
                    info!(
                        "Election timeout reached as Candidate. Restarting election for term {}",
                        node.current_term() + 1
                    );
                    state_guard.transition(|old| match old {
                        RaftNodeState::Candidate(n) => {
                            RaftNodeState::Candidate(n.into_restarted_candidate())
                        }
                        other => other,
                    });

                    let state_clone = state.clone();
                    let peer_manager_clone = peer_manager.clone();
                    let config_clone = config.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            initiate_election(config_clone, state_clone, peer_manager_clone).await
                        {
                            error!("Failed to initiate election: {}", e);
                        }
                    });
                }
            }
        }
    });
}

/// Initiates a Leader Election by requesting votes from all peers.
async fn initiate_election(
    config: Arc<Config>,
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) -> Result<()> {
    // 1. Gather election parameters from the current state.
    // Identity is immutable, so we can pre-allocate Arc<str> once outside the
    // loop for zero-allocation sharing across RPC tasks.
    let (term, cluster_id, node_id, last_log_index, last_log_term) = {
        let guard = state.read().await;
        let cid: Arc<str> = Arc::from(guard.cluster_id()?.as_str());
        let nid: Arc<str> = Arc::from(guard.node_id()?.to_string().as_str());

        let (last_idx, last_term) = match &*guard {
            RaftNodeState::Candidate(n) => (n.last_log_index(), n.last_log_term()),
            _ => (0, 0), // Should be Candidate
        };

        (guard.current_term()?, cid, nid, last_idx, last_term)
    };

    info!(
        "Campaigning for leadership in term {} (last_log: {}/{})",
        term, last_log_index, last_log_term
    );

    // 2. Request votes from all peers concurrently
    let peer_ids = peer_manager.peer_ids();
    let vote_requests = peer_ids.clone().into_iter().map(|peer_id| {
        let peer_manager = peer_manager.clone();
        let cluster_id = cluster_id.clone();
        let node_id = node_id.clone();
        let rpc_timeout = config.raft.rpc_timeout();

        async move {
            match peer_manager.get_client(peer_id) {
                Ok(mut client) => {
                    let mut request = Request::new(RequestVoteRequest {
                        cluster_id: cluster_id.to_string(),
                        term,
                        candidate_id: node_id.to_string(),
                        last_log_index,
                        last_log_term,
                    });
                    request.set_timeout(rpc_timeout);

                    match client.request_vote(request).await {
                        Ok(resp) => {
                            let resp = resp.into_inner();
                            // ADR 004 / Security: Verify cluster identity in response
                            if resp.cluster_id != *cluster_id {
                                return Err(unauthorized_response_status());
                            }
                            Ok(resp)
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
    let mut votes_granted = 1; // Vote for self
    let total_nodes = peer_ids.len() + 1;
    let quorum = (total_nodes / 2) + 1;

    while let Some(res) = vote_stream.next().await {
        match res {
            Ok(resp) => {
                if resp.term > term {
                    info!(
                        "Found higher term ({}) during election. Demoting to Follower.",
                        resp.term
                    );
                    let mut guard = state.write().await;
                    guard.transition(|old| old.into_follower(resp.term, None));
                    return Ok(());
                }
                if resp.vote_granted {
                    votes_granted += 1;
                    if votes_granted >= quorum {
                        info!(
                            "Quorum reached early ({} votes)! Transitioning to Leader for term {}.",
                            votes_granted, term
                        );
                        let mut guard = state.write().await;
                        let peer_ids_clone = peer_ids.clone();
                        guard.transition(|old| match old {
                            RaftNodeState::Candidate(n) if n.current_term() == term => {
                                RaftNodeState::Leader(n.into_leader(peer_ids_clone))
                            }
                            other => other,
                        });
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                debug!("Failed to get vote from peer: {}", e);
            }
        }
    }

    info!(
        "Election finalized for term {}: {}/{} votes granted.",
        term, votes_granted, quorum
    );

    Ok(())
}

/// Returns a standard gRPC PermissionDenied status for responses from
/// unauthorized clusters.
fn unauthorized_response_status() -> Status {
    Status::permission_denied("Received response from unauthorized cluster")
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
    // Use Arc<str> for zero-allocation identity sharing across concurrent RPC
    // tasks.
    let (term, cluster_id, node_id, commit_index) = {
        let guard = state.read().await;
        let cid: Arc<str> = Arc::from(guard.cluster_id()?.as_str());
        let nid: Arc<str> = Arc::from(guard.node_id()?.to_string().as_str());
        (guard.current_term()?, cid, nid, guard.commit_index()?)
    };

    let peer_ids = peer_manager.peer_ids();
    let rpc_timeout = config.raft.rpc_timeout();

    // 2. Prepare and send AppendEntries concurrently to all peers.
    let replication_requests = peer_ids.into_iter().map(|peer_id| {
        let state = state.clone();
        let peer_manager = peer_manager.clone();
        let cluster_id = cluster_id.clone();
        let node_id = node_id.clone();

        async move {
            // a. Prepare the request for this specific peer
            let request = {
                let guard = state.read().await;
                match &*guard {
                    RaftNodeState::Leader(node) => {
                        let next_idx = *node.state().next_index().get(&peer_id).unwrap_or(&1);
                        let last_log_idx = node.last_log_index();

                        let prev_log_index = next_idx - 1;
                        let prev_log_term = node.get_term_at(prev_log_index);

                        // Collect entries starting from next_idx
                        let entries = if last_log_idx >= next_idx {
                            // Logs are 1-indexed, so index 1 is at Vec index 0.
                            node.log()[(next_idx as usize - 1)..].to_vec()
                        } else {
                            Vec::new()
                        };

                        AppendEntriesRequest {
                            cluster_id: cluster_id.to_string(),
                            term,
                            leader_id: node_id.to_string(),
                            prev_log_index,
                            prev_log_term,
                            entries,
                            leader_commit: commit_index,
                        }
                    }
                    _ => return Ok(None), // No longer leader
                }
            };

            // Capture metadata before the request is consumed by the tonic::Request
            let sent_prev_idx = request.prev_log_index;
            let sent_entries_len = request.entries.len() as u64;

            // b. Execute the RPC
            match peer_manager.get_client(peer_id) {
                Ok(mut client) => {
                    let mut req = Request::new(request);
                    req.set_timeout(rpc_timeout);

                    match client.append_entries(req).await {
                        Ok(resp) => {
                            let resp = resp.into_inner();
                            // ADR 004 / Security: Verify cluster identity
                            if resp.cluster_id != *cluster_id {
                                return Err(unauthorized_response_status());
                            }
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

                // §5.1: If term > currentTerm, demote immediately
                if resp.term > term {
                    info!(
                        "Found higher term ({}) from peer {}. Demoting to Follower.",
                        resp.term, peer_id
                    );
                    guard.transition(|old| old.into_follower(resp.term, None));
                    return Ok(());
                }

                if let RaftNodeState::Leader(node) = &mut *guard {
                    if resp.success {
                        // Update nextIndex and matchIndex for follower (§5.3)
                        let new_match = sent_prev_idx + sent_entries_len;
                        let new_next = new_match + 1;

                        // Monotonicity check: only update if we are moving forward
                        let current_match = *node.state().match_index().get(&peer_id).unwrap_or(&0);
                        if new_match > current_match {
                            node.state_mut().next_index_mut().insert(peer_id, new_next);
                            node.state_mut()
                                .match_index_mut()
                                .insert(peer_id, new_match);
                        }

                        // Check for new commit point (§5.3, §5.4)
                        update_leader_commit_index(node);
                    } else {
                        // If AppendEntries fails because of log inconsistency:
                        // decrement nextIndex and retry (§5.3)
                        let current_next = *node.state().next_index().get(&peer_id).unwrap_or(&1);

                        // Optimization: jump back based on peer's actual log state
                        let new_next = if resp.last_log_index > 0 {
                            std::cmp::min(current_next, resp.last_log_index + 1)
                        } else {
                            current_next.saturating_sub(1).max(1)
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
fn update_leader_commit_index(node: &mut RaftNode<Leader>) {
    let last_idx = node.last_log_index();
    let current_term = node.current_term();
    let mut match_indices: Vec<u64> = node.state().match_index().values().cloned().collect();
    match_indices.push(last_idx); // Include self
    match_indices.sort_unstable();

    // The index that is replicated on a majority of nodes.
    // For 3 nodes, index 1 (middle element of sorted [idx1, idx2, idx3]).
    let quorum_idx = match_indices[(match_indices.len() - 1) / 2];

    if quorum_idx > node.commit_index() && node.get_term_at(quorum_idx) == current_term {
        info!("Quorum reached for log index {}. Committing.", quorum_idx);
        node.set_commit_index(quorum_idx);
    }
}
