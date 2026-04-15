use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::proto::v1::AppendEntriesRequest;
use common::proto::v1::AppendEntriesResponse;
use common::proto::v1::RequestVoteRequest;
use common::proto::v1::RequestVoteResponse;
use futures::StreamExt;
use futures::future::join_all;
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
    let (term, cluster_id, node_id) = {
        let guard = state.read().await;
        let cid: Arc<str> = Arc::from(guard.cluster_id()?.as_str());
        let nid: Arc<str> = Arc::from(guard.node_id()?.to_string().as_str());
        (guard.current_term()?, cid, nid)
    };

    info!("Campaigning for leadership in term {}...", term);

    // 2. Request votes from all peers concurrently
    let peer_ids = peer_manager.peer_ids();
    let vote_requests = peer_ids.into_iter().map(|peer_id| {
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
                        last_log_index: 0, // TODO: Phase 5 - Real log state
                        last_log_term: 0,
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
    let total_nodes = peer_manager.peer_ids().len() + 1;
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
                        guard.transition(|old| match old {
                            RaftNodeState::Candidate(n) if n.current_term() == term => {
                                RaftNodeState::Leader(n.into_leader())
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

/// Spawns a background task that periodically sends heartbeats if the node is a
/// Leader.
pub fn spawn_heartbeat_task(
    config: Arc<Config>,
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) {
    let interval = Duration::from_millis(config.raft.heartbeat_interval_ms);
    tokio::spawn(async move {
        loop {
            sleep(interval).await;

            let is_leader = matches!(&*state.read().await, RaftNodeState::Leader(_));
            if is_leader {
                let state_clone = state.clone();
                let peer_manager_clone = peer_manager.clone();
                let config_clone = config.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        send_heartbeats(config_clone, state_clone, peer_manager_clone).await
                    {
                        error!("Failed to send heartbeats: {}", e);
                    }
                });
            }
        }
    });
}

/// Sends AppendEntries RPCs (heartbeats) to all peers.
async fn send_heartbeats(
    config: Arc<Config>,
    state: Arc<RwLock<RaftNodeState>>,
    peer_manager: Arc<PeerManager>,
) -> Result<()> {
    // 1. Gather heartbeat parameters
    let (term, cluster_id_arc, node_id_str) = {
        let guard = state.read().await;
        let cid: Arc<str> = Arc::from(guard.cluster_id()?.as_str());
        let nid = guard.node_id()?.to_string();
        (guard.current_term()?, cid, nid)
    };

    // 2. Send heartbeats to all peers concurrently
    let peer_ids = peer_manager.peer_ids();
    let heartbeat_requests = peer_ids.into_iter().map(|peer_id| {
        let peer_manager = peer_manager.clone();
        let cluster_id = cluster_id_arc.clone();
        let node_id = node_id_str.clone();
        let rpc_timeout = config.raft.rpc_timeout();

        async move {
            match peer_manager.get_client(peer_id) {
                Ok(mut client) => {
                    let mut request = Request::new(AppendEntriesRequest {
                        cluster_id: cluster_id.to_string(),
                        term,
                        leader_id: node_id,
                        prev_log_index: 0, // TODO: Phase 5 - Real log state
                        prev_log_term: 0,
                        entries: Vec::new(),
                        leader_commit: 0,
                    });
                    request.set_timeout(rpc_timeout);

                    match client.append_entries(request).await {
                        Ok(resp) => {
                            let resp = resp.into_inner();
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

    let results: Vec<Result<AppendEntriesResponse, Status>> = join_all(heartbeat_requests).await;

    // 3. Handle responses (primarily term updates)
    for res in results {
        match res {
            Ok(resp) => {
                if resp.term > term {
                    info!(
                        "Found higher term ({}) in heartbeat response. Demoting to Follower.",
                        resp.term
                    );
                    let mut guard = state.write().await;
                    guard.transition(|old| old.into_follower(resp.term, None));
                    return Ok(());
                }
            }
            Err(e) => {
                debug!("Heartbeat failed for a peer: {}", e);
            }
        }
    }

    Ok(())
}
