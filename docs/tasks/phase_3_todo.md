# Phase 3 Task List: The Consensus Heart

## Step 0: Documentation

- [x] Save this plan as a task list to `docs/tasks/phase_3_todo.md` for progress tracking.

## Step 1: State Machine Expansion (`node.rs`)

- [x] Expand the base `RaftNode` to include standard Raft persistent state (`voted_for`).
- [x] Expand the volatile state structs:
  - [x] `Follower`: Track `leader_id` and maintain the election timer state.
  - [x] `Candidate`: Track `votes_received` (using a `HashSet` of `NodeId`).
  - [x] `Leader`: Track `next_index` and `match_index` for each peer (skeletal).
- [x] Implement safe state transition methods on the `RaftNodeState` enum:
  - [x] `to_candidate` (via `into_candidate`)
  - [x] `to_leader` (via `into_leader`)
  - [x] `to_follower` (via `into_follower`)
- [x] Ensure `std::mem::replace` is used for atomic, memory-safe transitions within the `RwLock` (via `transition` helper).

## Step 2: Randomized Election Timeouts (ADR 003)

- [x] Implement `tokio::spawn` background task for randomized election timeout (150ms - 300ms).
- [x] Trigger transition to Candidate on timeout.
- [x] Ensure timer resets on valid heartbeats.

## Step 3: Leader Campaign and Voting (`consensus.rs`)

- [x] Implement `RequestVote` RPC logic in `ConsensusDispatcher`.
  - [x] Term validation.
  - [x] `voted_for` validation.
- [x] Implement Candidate campaign loop.
  - [x] Concurrent `RequestVote` calls to all peers.
  - [x] Majority vote handling.

## Step 4: Heartbeat Mechanism and Leadership Maintenance

- [x] Implement `tokio::spawn` heartbeat task for Leaders (50ms).
- [x] Update `AppendEntries` handler to act as heartbeat receiver.
  - [x] Reset election timer.
  - [x] Update `leader_id`.
  - [x] Transition Candidate/Leader back to Follower on receipt of valid heartbeat (with defensive demotion for rival leaders).

## Step 5: Verification & Testing

- [x] Add BDD unit tests for voting logic and term increments.
- [x] Add BDD unit tests for state transitions.
- [x] Create `scripts/smoke_test.py` to verify leader election and failover.
- [x] Verify MTTR < 500ms for leadership re-election logic (demonstrated in chaos test).
