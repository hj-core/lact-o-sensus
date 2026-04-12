use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::proto::v1::RequestVoteRequest;
use common::proto::v1::RequestVoteResponse;
use futures::future::join_all;
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
            // Determine timeout for this "tick". We don't hold the RNG across the await.
            let timeout = {
                let mut rng = rand::rng();
                Duration::from_millis(rng.random_range(
                    config.raft.election_timeout_min_ms..config.raft.election_timeout_max_ms,
                ))
            };

            sleep(timeout).await;

            let mut state_guard = state.write().await;
            match &*state_guard {
                RaftNodeState::Follower(node) => {
                    let elapsed = node.state().last_heartbeat().elapsed();
                    if elapsed >= timeout {
                        info!(
                            "Election timeout reached ({:?}). Transitioning to Candidate for term \
                             {}",
                            elapsed,
                            node.current_term() + 1
                        );
                        state_guard.transition(|old| match old {
                            RaftNodeState::Follower(n) => {
                                RaftNodeState::Candidate(n.into_candidate())
                            }
                            other => other,
                        });

                        // Initiate election in a separate task to avoid holding the lock
                        let state_clone = state.clone();
                        let peer_manager_clone = peer_manager.clone();
                        let config_clone = config.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                initiate_election(config_clone, state_clone, peer_manager_clone)
                                    .await
                            {
                                error!("Failed to initiate election: {}", e);
                            }
                        });
                    }
                }
                RaftNodeState::Candidate(node) => {
                    // Candidates also have an election timeout; if they don't
                    // win, they start a new term.
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
                RaftNodeState::Leader(_) => {
                    // Leaders don't have election timeouts
                }
                RaftNodeState::Poisoned => {
                    error!("Node is poisoned. Election timer stopping.");
                    break;
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
    // Identity is immutable, so we can pre-allocate strings once outside the
    // loop.
    let (term, cluster_id_arc, node_id_str) = {
        let guard = state.read().await;
        let cid: Arc<str> = Arc::from(guard.cluster_id()?.as_str()); // Arc<str> for cheap sharing
        let nid = guard.node_id()?.to_string();
        (guard.current_term()?, cid, nid)
    };

    info!("Campaigning for leadership in term {}...", term);

    // 2. Request votes from all peers concurrently
    let peer_ids = peer_manager.peer_ids();
    let vote_requests = peer_ids.into_iter().map(|peer_id| {
        let peer_manager = peer_manager.clone();
        let cluster_id = cluster_id_arc.clone();
        let node_id = node_id_str.clone();
        let rpc_timeout = config.raft.rpc_timeout();

        async move {
            match peer_manager.get_client(peer_id) {
                Ok(mut client) => {
                    let mut request = Request::new(RequestVoteRequest {
                        cluster_id: cluster_id.to_string(),
                        term,
                        candidate_id: node_id,
                        last_log_index: 0, // TODO: Phase 3 Step 3 - Real log state
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

    let results: Vec<Result<RequestVoteResponse, Status>> = join_all(vote_requests).await;

    // 3. Tally votes and handle term updates
    let mut votes_granted = 1; // Vote for self
    let total_nodes = peer_manager.peer_ids().len() + 1;
    let quorum = (total_nodes / 2) + 1;

    for res in results {
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
                }
            }
            Err(e) => {
                debug!("Failed to get vote from peer: {}", e);
            }
        }
    }

    info!(
        "Election tally for term {}: {}/{} votes granted.",
        term, votes_granted, quorum
    );

    // 4. If majority granted, become Leader
    if votes_granted >= quorum {
        info!("Quorum reached! Transitioning to Leader for term {}.", term);
        let mut guard = state.write().await;
        guard.transition(|old| match old {
            RaftNodeState::Candidate(n) if n.current_term() == term => {
                RaftNodeState::Leader(n.into_leader())
            }
            other => other, // Could have changed state during RPCs (e.g. saw higher term)
        });
    }

    Ok(())
}

/// Returns a standard gRPC PermissionDenied status for responses from
/// unauthorized clusters.
fn unauthorized_response_status() -> Status {
    Status::permission_denied("Received response from unauthorized cluster")
}
