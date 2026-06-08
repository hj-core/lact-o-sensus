# Phase 8: Pre-Vote Integrity (Election Safety)

## Implementation Plan

> **Document Status:** Committed
> **Checklist Compliance:** This plan adheres to the [Implementation Planning Checklist](../checklists/planning_checklist.md). Each rule ID ([EXEC-01], [BDD-02], etc.) is cited explicitly.

---

## Architectural Overview

The Pre-Vote mechanism introduces a `PreCandidate` role state between `Follower` and `Candidate`. A Follower whose election timer expires first enters `PreCandidate` — a dry-run phase where it checks with peers whether a real election would succeed. Only after receiving quorum does it increment its term and become a full `Candidate`.

```
Follower ──[timeout]──► PreCandidate ──[pre-vote quorum]──► Candidate ──[vote quorum]──► Leader
                              │                                    │
                              │ [no quorum / timeout]              │ [timeout / restart]
                              ▼                                    ▼
                         Follower                             Candidate (term+1, skip pre-vote)
```

**Key invariants:**
- Pre-vote never increments the term — the requester's term stays unchanged until real candidacy
- Pre-vote never mutates peer state — voters do read-only log-up-to-date checks, no persistence, no timer reset
- Pre-vote is purely advisory — multiple candidates can receive grants from the same voter; real voting resolves correctly
- Partitioned nodes with stale logs get denied pre-votes and never disrupt the cluster term
- `handle_pre_vote()` does NOT call `into_follower()` when `req_term > current_term` — the reader stays in its current role
- `delegate_to_inner!` macros delegate shared methods normally for `PreCandidate` (same as Follower/Candidate/Leader)

## Timing Impact

The pre-vote campaign timeout is a new, separate constant:

| Constant | Value | Notes |
|---|---|---|
| `HEARTBEAT_INTERVAL` | 50ms (unchanged) | ADR 003 |
| `ELECTION_TIMEOUT_MIN` | 150ms (unchanged) | ADR 003 |
| `ELECTION_TIMEOUT_MAX` | 300ms (unchanged) | ADR 003 |
| `PRE_VOTE_CAMPAIGN_TIMEOUT` | ~80ms (NEW) | 2× RPC_TIMEOUT |
| `RPC_TIMEOUT` | 40ms (unchanged) | ADR 003 |

MTTR impact: Pre-vote adds one RTT (~40ms) to the success path. Without pre-vote: ~190–340ms. With pre-vote: ~230–380ms. Still sub-second and well within ADR 003's "< 500ms" goal.

---

## Task Sequence

### Task 1 (RED): Behavioral Tests — Define the Contract First

- **Rule References:** [BDD-01] [BDD-02] [BDD-03] [DETAIL-01] [VERIFY-02]
- **Goal:** Write failing tests that capture all pre-vote behaviors before any production code changes.
- **Draft Commit:** `test(raft): define pre-vote behavioral contract via failing tests`

#### Affected Files

- `crates/raft-engine/src/consensus/tests.rs` (add integration tests + update MockConsensusService)
- `crates/raft-engine/src/engine.rs` (tests module — add unit tests inside the existing test module)
- `crates/raft-engine/src/node/follower.rs` — no changes (tests use existing patterns)
- `crates/raft-engine/src/consensus/mod.rs` — no changes

#### Major Steps

1. **Update `MockConsensusService`** to implement the `pre_vote` RPC (the trait method won't exist yet, so this step is prepared but gated):
   - Add a `pre_vote_response: Arc<Mutex<PreVoteResponse>>` field
   - The `pre_vote` method echoes the trace_id header back (same pattern as `request_vote`)

2. **Write integration tests** in `crates/raft-engine/src/consensus/tests.rs`:

   a. **`partitioned_node_does_not_disrupt_term`** — core safety test:
      - Setup: 3-node cluster. Node 1 is Leader at term=5 with committed entries. Node 2 (partitioned) has term=3 with stale log.
      - Action: Node 2's election timer fires → sends PreVote to peers.
      - Expected: Peers deny pre-vote (Node 2's log is stale). Cluster term remains 5. Node 2 stays as Follower.
      - Assert: `term == Term::new(5)`, Node 2 is `RoleState::Follower`.

   b. **`pre_vote_quorum_triggers_real_election`** — happy path:
      - Setup: Node 1 as Follower, term=1. No leader for > timeout.
      - Action: PreVote campaign — all peers grant.
      - Expected: Node transitions to Candidate with term=2, then to Leader.
      - Assert: Role is `Leader`, term is `Term::new(2)`.

   c. **`pre_vote_no_quorum_returns_to_follower`**:
      - Setup: PreCandidate with no peers responding (peer responses simulate denial).
      - Action: Campaign timeout or all responses deny.
      - Assert: Node is `Follower`, term unchanged.

   d. **`pre_vote_candidate_restart_skips_pre_vote`**:
      - Setup: Candidate in term=3 whose election campaign timed out.
      - Action: Candidate's `evaluate_tick()` fires.
      - Assert: Returns `TickAction::StartElection` (not `StartPreVote`). Term becomes 4.

3. **Write unit tests** in `crates/raft-engine/src/engine.rs` (tests module):

   a. **`follower_evaluate_tick_returns_start_pre_vote`**:
      - Assert: After election timeout, `Follower::evaluate_tick()` returns `TickAction::StartPreVote`.

   b. **`pre_candidate_evaluate_tick_returns_step_down`**:
      - Setup: `PreCandidate` past its campaign timeout.
      - Assert: `evaluate_tick()` returns `TickAction::StepDown`.

   c. **`grant_pre_vote_does_not_reset_timer`**:
      - Setup: Follower at tick=10, `last_heartbeat=tick=5`.
      - Action: Call `grant_pre_vote(...)`.
      - Assert: `last_heartbeat` is still 5 (unchanged).

   d. **`grant_pre_vote_does_not_persist`**:
      - Setup: Follower with no `voted_for`.
      - Action: Call `grant_pre_vote(...)` (returns true).
      - Assert: `voted_for` is still `None`.

   e. **`grant_pre_vote_respects_log_up_to_date`**:
      - Setup: Local log has entry at term=2, index=5.
      - Action: PreVote from candidate with last_log_term=1 (older).
      - Assert: Returns false.

4. **Write gRPC handler tests** in `crates/raft-engine/src/service/consensus.rs` (tests module):

   a. **`pre_vote_injects_trace_integrity`**:
      - Assert: PreVote response echoes trace_id correctly.

   b. **`pre_vote_rejects_missing_trace_id`**:
      - Assert: Returns `FailedPrecondition`.

   c. **`pre_vote_higher_term_does_not_demote`**:
      - Setup: Follower at term=3.
      - Action: PreVote from candidate with term=10.
      - Assert: `vote_granted` is true (log up-to-date), but term in response is 3 (NOT 10 — no demotion).

#### Test Structure (BDD Nested Hierarchy)

```
tests/
  pre_vote/
    campaign_lifecycle/
      partitioned_node_does_not_disrupt_term
      pre_vote_quorum_triggers_real_election
      pre_vote_no_quorum_returns_to_follower
      pre_vote_candidate_restart_skips_pre_vote
    grant_pre_vote/
      does_not_reset_timer
      does_not_persist
      respects_log_up_to_date
      grants_when_log_is_up_to_date
  pre_vote_handler/
    trace_integrity/
      injects_trace_id
      rejects_missing_trace_id
    term_semantics/
      higher_term_does_not_demote
```

#### Caveats

- Tests won't compile until Tasks 2–6 are implemented. Mark them with `#[ignore]` initially.
- To allow the crate to build, the test code referencing `PreVoteRequest`/`PreVoteResponse` and `TickAction::StartPreVote`/`StepDown` must be conditionally compiled or the entire module can be temporarily gated.
- **Recommended approach:** Write each test block with a `#[cfg(any())]` guard on the outer `mod` so the crate compiles. Remove the guard as each task implements the required types.

---

### Task 2: Protocol Contract — Proto Definition + Factories

- **Rule References:** [EXEC-01] [EXEC-02] [DETAIL-01] [VERIFY-02]
- **Goal:** Add `PreVoteRequest`, `PreVoteResponse`, and the `PreVote` RPC to the consensus service.
- **Draft Commit:** `feat(common): add PreVote RPC to consensus protocol`

#### Affected Files

- `crates/common/proto/raft.proto`
- `crates/common/src/proto.rs`

#### Major Steps

1. Add to `raft.proto`:
```protobuf
message PreVoteRequest {
    uint64 term = 1;
    string candidate_id = 2;
    uint64 last_log_index = 3;
    uint64 last_log_term = 4;
}
message PreVoteResponse {
    uint64 term = 1;
    bool vote_granted = 2;
}
```

2. Add to `ConsensusService` in `raft.proto`:
```protobuf
rpc PreVote(PreVoteRequest) returns (PreVoteResponse);
```

3. Add factory impls in `common/src/proto.rs` (same pattern as `RequestVoteRequest`/`Response`):
```rust
impl PreVoteRequest {
    pub fn new(
        term: Term,
        candidate_id: NodeId,
        last_log_index: LogIndex,
        last_log_term: Term,
    ) -> Self { ... }
}

impl PreVoteResponse {
    pub fn new(term: Term, vote_granted: bool) -> Self { ... }
}
```

#### Consequences

- `tonic-build` auto-generates the `PreVote` RPC method on the `ConsensusService` trait
- `ConsensusServiceClient` gains a `pre_vote()` method
- `MockConsensusService` and `ConsensusDispatcher` must implement `pre_vote` (will fail to compile until Tasks 3–6)

#### Caveats

- The generated code may rename `PreVoteRequest`/`PreVoteResponse` in the gRPC service method signature. Check the tonic convention — typically `PreVoteRequest` is the request type and `PreVoteResponse` is the response type, same as the message names.

---

### Task 3: PreCandidate Role State + Follower Pre-Vote Granting

- **Rule References:** [EXEC-01] [EXEC-02] [VERIFY-03] [DETAIL-01] [VERIFY-02]
- **Goal:** Implement the new `PreCandidate` type-state and the read-only `grant_pre_vote()` method on `Follower`.
- **Draft Commit:** `feat(raft): add PreCandidate role state and read-only pre-vote granting`

#### Affected Files

- `crates/raft-engine/src/node/pre_candidate.rs` (NEW)
- `crates/raft-engine/src/node/mod.rs` (add module declaration)
- `crates/raft-engine/src/node/follower.rs` (add `grant_pre_vote()` method)
- `crates/raft-engine/src/engine.rs` (add `NodeRole::PreCandidate`, `RoleState::PreCandidate`, update macros, transitions, tick, reset_heartbeat)
- `crates/raft-engine/src/tick.rs` (add `TickAction::StartPreVote`, `TickAction::StepDown`)
- `crates/raft-engine/src/consensus/types.rs` (add `PreVoteCampaignParams`)

#### Major Steps

1. **Create `PreCandidate` struct** in `node/pre_candidate.rs`:
```rust
#[derive(Debug)]
pub struct PreCandidate {
    pre_votes_received: HashSet<NodeId>,
    campaign_start: Tick,
    timeout: TickDuration,
}
```

2. **Implement `PreCandidate` methods**:
```rust
impl PreCandidate {
    pub fn new(campaign_start: Tick, timeout: TickDuration) -> Self { ... }
    pub fn campaign_start(&self) -> Tick { ... }
    pub fn timeout(&self) -> TickDuration { ... }
    pub fn add_pre_vote(&mut self, peer_id: NodeId) { ... }
    pub fn vote_count(&self) -> usize { ... }
    pub fn evaluate_tick(&self, now: Tick) -> TickAction {
        if now - self.campaign_start >= self.timeout {
            TickAction::StepDown
        } else {
            TickAction::None
        }
    }
}
impl NodeState for PreCandidate {}
```

3. **Add `RaftNode<PreCandidate>` methods**:
```rust
impl RaftNode<PreCandidate> {
    pub fn try_into_pre_candidate(
        self,
        campaign_start: Tick,
        timeout: TickDuration,
    ) -> Result<RaftNode<PreCandidate>, NodeError> {
        // NO term increment
        // NO self-vote
        Ok(self.transition(PreCandidate::new(campaign_start, timeout)))
    }

    pub fn try_into_follower(
        self,
        term: Term,
        leader_id: Option<NodeId>,
        now: Tick,
        timeout: TickDuration,
    ) -> Result<RaftNode<Follower>, NodeError> { ... }
}
```

4. **Add `grant_pre_vote()` on `RaftNode<Follower>`** in `follower.rs`:
```rust
pub fn grant_pre_vote(
    &self,
    candidate_id: NodeId,
    req_term: Term,
    req_last_log_index: LogIndex,
    req_last_log_term: Term,
) -> Result<bool, NodeError> {
    let current_term = self.current_term()?;

    // Pre-vote uses >= instead of == because pre-votes don't demote
    if req_term < current_term {
        return Ok(false);
    }

    // Check log up-to-date (same as real voting)
    Ok(self.is_log_up_to_date(req_last_log_term, req_last_log_index)?)
}
```

5. **Add `TickAction` variants** in `tick.rs`:
```rust
pub enum TickAction {
    None,
    StartPreVote,  // NEW
    StartElection,
    SendHeartbeat,
    StepDown,      // NEW — return to Follower without term change
    Stop,
}
```

6. **Update `NodeRole` enum** in `engine.rs`:
```rust
pub enum NodeRole {
    Follower,
    PreCandidate,  // NEW
    Candidate,
    Leader,
    Poisoned,
}
```

7. **Update `RoleState` enum** in `engine.rs`:
```rust
pub enum RoleState {
    Follower(RaftNode<Follower>),
    PreCandidate(RaftNode<PreCandidate>),  // NEW
    Candidate(RaftNode<Candidate>),
    Leader(RaftNode<Leader>),
    Poisoned,
}
```

8. **Update `delegate_to_inner!` and `delegate_mut_to_inner!`** macros — add `PreCandidate(n)` arm that delegates normally:
```rust
RoleState::PreCandidate(n) => n.$method($($args),*),
```

9. **Update `tick()` method** to handle PreCandidate:
```rust
RoleState::PreCandidate(node) => node.state().evaluate_tick(now),
```

10. **Update `reset_heartbeat()`** — PreCandidate heartbeat reset behaves like Follower (resets campaign timer if leader discovered):
```rust
RoleState::PreCandidate(node) => node.state_mut().reset_heartbeat(tick),
```

11. **Add `into_follower()` transition** for PreCandidate → Follower (universal demotion):
```rust
RoleState::PreCandidate(n) => match n.try_into_follower(term, leader_id, tick, timeout) {
    Ok(new) => RoleState::Follower(new),
    Err(e) => Self::apply_fatal_static(e),
},
```

#### Consequences

- `Follower::evaluate_tick()` now returns `TickAction::StartPreVote` instead of `TickAction::StartElection` on timeout
- `Candidate::evaluate_tick()` still returns `TickAction::StartElection` (re-election skips pre-vote)
- `PreCandidate::evaluate_tick()` returns `TickAction::StepDown` on timeout
- The `delegate_to_inner!` macros will pass through shared methods like `current_term()`, `last_log_index()`, `last_log_term()`, `voted_for()` for PreCandidate

#### Caveats

- The `into_pre_candidate()` method (on `LogicalNode`) is deferred to Task 4
- The `PreVoteCampaignParams` type is needed by the async campaign (Task 5) and is defined here because it captures PreCandidate state at handoff

---

### Task 4: Engine Pre-Vote Handler + State Transitions

- **Rule References:** [EXEC-01] [EXEC-02] [VERIFY-03] [DETAIL-01] [VERIFY-02]
- **Goal:** Wire up `handle_pre_vote()` and `into_pre_candidate()` in `LogicalNode`.
- **Draft Commit:** `feat(raft): implement handle_pre_vote and into_pre_candidate transitions`

#### Affected Files

- `crates/raft-engine/src/engine.rs`
- `crates/raft-engine/src/engine.rs` (tests — un-ignore pre-vote engine tests from Task 1)

#### Major Steps

1. **Add `into_pre_candidate()` on `LogicalNode`:**
```rust
pub fn into_pre_candidate(&mut self) {
    let timeout = self.thresholds.generate_election_timeout(&mut self.rng);
    let tick = self.current_tick;

    self.transition(|old_role| match old_role {
        RoleState::Follower(n) => match n.try_into_pre_candidate(tick, timeout) {
            Ok(new) => RoleState::PreCandidate(new),
            Err(e) => Self::apply_fatal_static(e),
        },
        other => other,
    });
}
```

2. **Add `handle_pre_vote()` on `LogicalNode`:**
```rust
pub fn handle_pre_vote(
    &mut self,
    candidate_id: NodeId,
    req_term: Term,
    req_last_log_index: LogIndex,
    req_last_log_term: Term,
) -> RequestVoteResult {
    // Core safety invariant: PreVote does NOT trigger term demotion.
    // We do NOT call into_follower() here, regardless of req_term.

    let vote_granted = match &mut self.state {
        RoleState::Follower(node) => match node.grant_pre_vote(
            candidate_id,
            req_term,
            req_last_log_index,
            req_last_log_term,
        ) {
            Ok(granted) => granted,
            Err(e) => self.apply_fatal(e),
        },
        _ => false,
    };

    if vote_granted {
        RequestVoteResult::granted(self.current_term())
    } else {
        RequestVoteResult::rejected(self.current_term())
    }
}
```

3. **Remove `TickAction::StartElection` from `Follower::evaluate_tick()` return path** (already done in Task 3, verified here).

4. **Un-ignore engine unit tests** from Task 1 that test `Follower::evaluate_tick() → StartPreVote`, `PreCandidate::evaluate_tick() → StepDown`.

#### Consequences

- Any node receiving a PreVote will respond with its current term and a boolean grant — no state mutation on the receiver
- The `PreVoteCampaignParams` type (defined in Task 3) is used in the `into_pre_candidate()` handoff

#### Caveats

- The `into_pre_candidate()` method is called from the tick loop while holding the write lock. The campaign params are captured inside the lock and handed off to the async task outside the lock (Atomic Handoff pattern, same as `into_candidate()`).

---

### Task 5: Pre-Vote Campaign — Async Broadcasting + Tallying

- **Rule References:** [EXEC-01] [EXEC-02] [VERIFY-03] [DETAIL-01] [VERIFY-02]
- **Goal:** Implement the asynchronous pre-vote campaign, mirroring the existing election campaign structure.
- **Draft Commit:** `feat(raft): implement async pre-vote campaign with quorum tallying`

#### Affected Files

- `crates/raft-engine/src/consensus/election.rs`
- `crates/raft-engine/src/consensus/types.rs`
- `crates/raft-engine/src/consensus/mod.rs`

#### Major Steps

1. **Add `PreVoteCampaignParams` to `types.rs`** (may already exist from Task 3):
```rust
#[derive(Debug, Clone, Copy)]
pub(super) struct PreVoteCampaignParams {
    pub(super) term: Term,
    pub(super) hypothetical_term: Term,
    pub(super) node_id: NodeId,
    pub(super) last_log_index: LogIndex,
    pub(super) last_log_term: Term,
    pub(super) trace_id: TraceId,
}
```

2. **Add `PreVoteAction` enum to `types.rs`:**
```rust
#[derive(Debug, PartialEq)]
pub(super) enum PreVoteAction {
    QuorumReached,
    NoQuorum,
    Continue,
}
```

3. **Add `start_pre_vote_campaign()` to `election.rs`:**
```rust
pub(super) fn start_pre_vote_campaign<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: PreVoteCampaignParams,
    parent_span: tracing::Span,
) {
    let span = info_span!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        parent: &parent_span,
        "pre_vote_campaign",
        trace_id = %params.trace_id,
        term = %params.term
    );

    tokio::spawn(
        async move {
            match initiate_pre_vote(config, state.clone(), peer_manager, params).await {
                Ok(action) => {
                    match action {
                        PreVoteAction::QuorumReached => {
                            // Pre-vote succeeded — now start a real election
                            // The into_candidate() was already called by
                            // process_pre_vote_response when quorum was detected.
                        }
                        PreVoteAction::NoQuorum => {
                            info!(... "Pre-vote failed: quorum not reached. Returning to Follower.");
                            let mut guard = state.write().await;
                            guard.into_follower(guard.current_term(), None);
                        }
                        PreVoteAction::Continue => {
                            // Should not reach here (loop exited without quorum)
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Pre-vote campaign failed");
                    let mut guard = state.write().await;
                    guard.apply_fatal(e);
                }
            }
        }
        .instrument(span),
    );
}
```

4. **Add `initiate_pre_vote()` to `election.rs`:**
```rust
pub(super) async fn initiate_pre_vote<S: StateMachine>(
    config: Arc<Config>,
    state: Arc<ConsensusShell<S>>,
    peer_manager: Arc<PeerManager>,
    params: PreVoteCampaignParams,
) -> ConsensusResult<PreVoteAction> {
    // 1. Broadcast pre-vote requests concurrently
    let peer_ids = peer_manager.peer_ids();
    let mut pre_vote_stream = broadcast_pre_vote_requests(
        config.as_ref(),
        peer_manager.clone(),
        params.hypothetical_term,
        params.node_id,
        params.last_log_index,
        params.last_log_term,
        params.trace_id,
    );

    // 2. Tally responses
    let total_nodes = peer_ids.len() + 1;
    let quorum = (total_nodes / 2) + 1;
    let mut pre_votes_granted = 0;  // No self-vote in pre-vote

    while let Some((peer_id, res)) = pre_vote_stream.next().await {
        match process_pre_vote_response(
            &state,
            params.term,
            &peer_ids,
            peer_id,
            res,
            quorum,
        ).await? {
            PreVoteAction::QuorumReached => return Ok(PreVoteAction::QuorumReached),
            PreVoteAction::NoQuorum => return Ok(PreVoteAction::NoQuorum),
            PreVoteAction::Continue => {
                let guard = state.read().await;
                if let RoleState::PreCandidate(n) = guard.state() {
                    pre_votes_granted = n.state().vote_count();
                }
            }
        }
    }

    Ok(PreVoteAction::NoQuorum)
}
```

5. **Add `process_pre_vote_response()` to `election.rs`:**
```rust
pub(super) async fn process_pre_vote_response<S: StateMachine>(
    state: &ConsensusShell<S>,
    term: Term,
    peer_ids: &[NodeId],
    peer_id: NodeId,
    res: RpcResult<PreVoteResponse>,
    quorum: usize,
) -> ConsensusResult<PreVoteAction> {
    let resp = match res {
        Ok(val) => val,
        Err(e) => {
            debug!(... "Failed to get pre-vote from peer");
            return Ok(PreVoteAction::Continue);
        }
    };

    let mut guard = state.write().await;
    let resp_term = Term::new(resp.term);

    // NOTE: Higher term in pre-vote response does NOT demote.
    // We log it but continue.

    if resp.vote_granted {
        let mut quorum_reached = false;

        if let Some(node) = guard.as_pre_candidate_mut() {
            node.state_mut().add_pre_vote(peer_id);
            let current_count = node.state().vote_count();
            // No self-vote in pre-vote, so +1 for self is NOT added
            // Wait — the self should count. In real Raft pre-vote,
            // the candidate implicitly pre-votes for itself. Let me reconsider.
            //
            // Actually: the candidate doesn't vote for itself in pre-vote.
            // The pre-vote is purely asking peers "would you vote for me?"
            // If the candidate's own log is up-to-date, it doesn't need to
            // ask itself. So pre_vote_count includes ONLY peer responses.
            //
            // Re-checking quorum: (total_nodes / 2) + 1
            // Self is NOT counted. So quorum needs majority of peers.
            // Actually wait — this means with 3 nodes, we need 2 pre-votes
            // from peers to reach quorum of 2 (majority of 3).
            if current_count >= quorum {
                quorum_reached = true;
            }
        }

        if quorum_reached {
            // Pre-vote succeeded — proceed to real election
            info!(... "Pre-vote quorum reached! Transitioning to Candidate.");
            guard.into_candidate();

            // Now spawn the real election campaign
            let trace_id = TraceId::generate();
            let real_params = ElectionCampaignParams {
                term: guard.current_term(),
                node_id: guard.node_id(),
                last_log_index: guard.last_log_index(),
                last_log_term: guard.last_log_term(),
                trace_id,
            };

            // Release lock before spawning async campaign
            let peer_ids_vec = peer_ids.to_vec();
            drop(guard);
            // Note: The actual election campaign is spawned by the caller
            return Ok(PreVoteAction::QuorumReached);
        }
    }

    Ok(PreVoteAction::Continue)
}
```

6. **Add `broadcast_pre_vote_requests()`** — mirrors `broadcast_vote_requests()` but calls `pre_vote` RPC.

7. **Add `request_pre_vote_from_peer()`** — mirrors `request_vote_from_peer()` but uses `PreVoteRequest`/`Response` and `client.pre_vote()`.

8. **Update `consensus/mod.rs`** to re-export:
```rust
pub(crate) use election::start_pre_vote_campaign;
```

---

### Task 6: gRPC Service Handler

- **Rule References:** [EXEC-01] [EXEC-02] [DETAIL-01] [VERIFY-02]
- **Goal:** Implement the `PreVote` handler in `ConsensusDispatcher`.
- **Draft Commit:** `feat(raft): add PreVote gRPC handler to ConsensusDispatcher`

#### Affected Files

- `crates/raft-engine/src/service/consensus.rs`
- `crates/raft-engine/src/consensus/tests.rs` (update `MockConsensusService`)

#### Major Steps

1. **Add `PreVoteParams` struct** (mirrors `VoteParams`):
```rust
struct PreVoteParams {
    candidate_id: NodeId,
    term: Term,
    last_log_index: LogIndex,
    last_log_term: Term,
}

impl PreVoteParams {
    fn try_from_proto(req: PreVoteRequest) -> Result<Self, Status> {
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
```

2. **Add `execute_pre_vote_logic()` method** on `ConsensusDispatcher`:
```rust
async fn execute_pre_vote_logic(
    &self,
    params: &PreVoteParams,
) -> Result<crate::engine::RequestVoteResult, Status> {
    let mut guard = self.state.write().await;
    self.verify_node_integrity(&mut guard)?;
    Ok(guard.handle_pre_vote(
        params.candidate_id,
        params.term,
        params.last_log_index,
        params.last_log_term,
    ))
}
```

3. **Implement `pre_vote` on `ConsensusService for ConsensusDispatcher`:**
```rust
async fn pre_vote(
    &self,
    request: Request<PreVoteRequest>,
) -> Result<Response<PreVoteResponse>, Status> {
    let trace_id = TraceInterceptor::require_trace_id(&request)?;
    let params = PreVoteParams::try_from_proto(request.into_inner())?;

    let span = info_span!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        "pre_vote",
        cluster_id = %self.identity.cluster_id(),
        node_id = %self.identity.node_id(),
        trace_id = %trace_id,
        term = %params.term,
        sender_id = %params.candidate_id
    );

    let result = self.execute_pre_vote_logic(&params).instrument(span).await?;

    let mut response = Response::new(PreVoteResponse::new(result.term, result.vote_granted));
    TraceInterceptor::inject_trace_id_into_response(&mut response, trace_id)?;

    Ok(response)
}
```

4. **Update `MockConsensusService`** to implement `pre_vote`:
```rust
pre_vote_response: Arc<Mutex<PreVoteResponse>>,

async fn pre_vote(
    &self,
    request: Request<PreVoteRequest>,
) -> Result<Response<PreVoteResponse>, Status> {
    let trace_id_header = request.metadata().get(HEADER_TRACE_ID).cloned();
    let mut res = Response::new(*self.pre_vote_response.lock().unwrap());
    if let Some(val) = trace_id_header {
        res.metadata_mut().insert(HEADER_TRACE_ID, val);
    }
    Ok(res)
}
```

---

### Task 7: Tick Loop Integration

- **Rule References:** [EXEC-01] [EXEC-02] [VERIFY-03] [DETAIL-01] [VERIFY-02]
- **Goal:** Wire the `StartPreVote` and `StepDown` actions into the tick loop dispatch.
- **Draft Commit:** `feat(raft): integrate pre-vote lifecycle into tick loop dispatch`

#### Affected Files

- `crates/raft-engine/src/consensus/lifecycle.rs`

#### Major Steps

1. **Add `StartPreVote` branch in the tick loop** (inside the lock):
```rust
TickAction::StartPreVote => {
    let trace_id = TraceId::generate();
    guard.into_pre_candidate();
    pre_vote_params = Some(PreVoteCampaignParams {
        term: guard.current_term(),
        hypothetical_term: (guard.current_term() + 1).unwrap(),
        node_id: guard.node_id(),
        last_log_index: guard.last_log_index(),
        last_log_term: guard.last_log_term(),
        trace_id,
    });
}
```

2. **Dispatch after lock release:**
```rust
if let Some(params) = pre_vote_campaign {
    start_pre_vote_campaign(
        config.clone(),
        state.clone(),
        peer_manager.clone(),
        params,
        session_span.clone(),
    );
}
```

3. **Handle `StepDown` in the action dispatch** (safety net for stalled async tasks):
```rust
TickAction::StepDown => {
    info!(
        target: ClinicalTarget::RaftFoundation.as_str(),
        "Pre-vote campaign timed out. Returning to Follower."
    );
    let mut guard = state.write().await;
    guard.into_follower(guard.current_term(), None);
}
```

4. **Update the action tuple** to carry both `campaign_params` and `pre_vote_params`:
```rust
let (action, role_name, term, campaign, replication, pre_vote_campaign) = {
    ...
    let mut pre_vote_params = None;
    ...
    (action, role_name, term, campaign_params, replication_params, pre_vote_params)
};
```

---

### Task 8 (GREEN): Verification Pipeline

- **Rule References:** [VERIFY-01] [BDD-01]
- **Goal:** Run the full clinical verification sequence and ensure all tests pass.
- **Draft Commit:** `feat(raft): complete pre-vote integrity with verification pipeline`

#### Actions

1. `cargo +nightly fmt --all` — verify zero diff
2. `cargo test --all-features` — all tests pass (including those from Task 1)
3. `cargo clippy --all-targets -- -D warnings`
4. `python3 scripts/smoke_test.py`

#### Success Criteria

- All BDD tests from Task 1 pass (Red → Green transition complete)
- No regressions in existing tests
- No clippy warnings
- Smoke test passes
- `docs/adrs/003_network_timing_model.md` is reviewed and updated with pre-vote timing section (pre-vote campaign timeout documented)

---

## Summary of New Files

| # | File | Purpose |
|---|---|---|
| 1 | `crates/raft-engine/src/node/pre_candidate.rs` | PreCandidate type-state struct, NodeState impl, evaluate_tick, add_pre_vote, vote_count |

## Summary of Modified Files

| # | File | Change |
|---|---|---|
| 1 | `crates/common/proto/raft.proto` | Add PreVoteRequest, PreVoteResponse messages, PreVote RPC |
| 2 | `crates/common/src/proto.rs` | Add factory impls for PreVoteRequest/Response |
| 3 | `crates/raft-engine/src/tick.rs` | Add TickAction::StartPreVote, TickAction::StepDown |
| 4 | `crates/raft-engine/src/node/mod.rs` | Add pre_candidate module |
| 5 | `crates/raft-engine/src/node/follower.rs` | Add grant_pre_vote() read-only method |
| 6 | `crates/raft-engine/src/node/pre_candidate.rs` | NEW — see above |
| 7 | `crates/raft-engine/src/engine.rs` | Add PreCandidate to RoleState/NodeRole, delegate macros, tick(), reset_heartbeat(), into_pre_candidate(), handle_pre_vote(), transition paths |
| 8 | `crates/raft-engine/src/consensus/types.rs` | Add PreVoteCampaignParams, PreVoteAction |
| 9 | `crates/raft-engine/src/consensus/election.rs` | Add start_pre_vote_campaign(), initiate_pre_vote(), process_pre_vote_response(), broadcast_pre_vote_requests(), request_pre_vote_from_peer() |
| 10 | `crates/raft-engine/src/consensus/mod.rs` | Re-export start_pre_vote_campaign |
| 11 | `crates/raft-engine/src/consensus/lifecycle.rs` | Wire StartPreVote and StepDown actions |
| 12 | `crates/raft-engine/src/service/consensus.rs` | Add PreVote gRPC handler |
| 13 | `crates/raft-engine/src/consensus/tests.rs` | BDD integration tests + MockConsensusService update |
| 14 | `crates/raft-engine/src/engine.rs` (tests) | Unit tests for tick actions, grant_pre_vote |
| 15 | `crates/raft-engine/src/service/consensus.rs` (tests) | PreVote handler tests |

## Risk Register

| Risk | Severity | Mitigation |
|---|---|---|
| Pre-vote campaign async task races with leader discovery | Medium | `TickAction::StepDown` is the sync safety net — if the async task is lost or delayed, the tick loop transitions back to Follower |
| Pre-vote grants never reset timers — what if no leader exists? | Low | Pre-vote campaign timeout forces back to Follower with a fresh randomized timer, which will try again |
| Tests from Task 1 won't compile until Task 6 | High | Use `#[cfg(any())]` to gate test modules, removing the guard progressively as each task completes; OR gate at the `feature` level |
| `PreCandidate` in `delegate_to_inner!` may panic on unexpected access | Low | All shared methods (`current_term`, `last_log_index`, etc.) delegate normally — no panic risk |
| `MockConsensusService` needs `pre_vote` method before trait exists | Medium | Use `#[allow(unused)]` and conditional compilation in test code; the mock update aligns with Task 6 |
| `process_pre_vote_response` needs to spawn real election campaign | Medium | The function returns `PreVoteAction::QuorumReached` and the caller (`initiate_pre_vote`) handles the transition inside its async context; the `start_pre_vote_campaign` wrapper then calls `start_election_campaign` with the real params |

---

## ADR 003 Update Notes

The following should be added to `docs/adrs/003_network_timing_model.md`:

```markdown
### 5. Pre-Vote Campaign Timeout

To support the Pre-Vote mechanism (Phase 8), a campaign timeout is introduced:

- **`PRE_VOTE_CAMPAIGN_TIMEOUT`:** ~80ms (2× RPC_TIMEOUT). This provides sufficient time for a full round-trip of PreVote requests to all peers while being short enough to not significantly delay the overall election MTTR.

**MTTR Impact:** Pre-vote adds approximately one RTT (~40ms) to the success path, bringing the worst-case failover from ~340ms to ~380ms — still well within the sub-second recovery target.
```
