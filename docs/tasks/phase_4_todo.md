# Phase 4 Task List: Ingress & Mock Egress

## 🎯 Goal

Connect external actors (`client-cli` and `ai-veto`) to the `raft-node` consensus group and verify the full request lifecycle from user intent to committed consensus using mock policy logic.

---

## Step 0: Consensus Completion (In-Memory Log) [x]

- [x] Implement the in-memory Log storage (`Vec<LogEntry>`) in `RaftNode`.
- [x] Expand `AppendEntries` handler to process real entries (Index matching, Term validation, and Log conflict resolution).
- [x] Implement the Leader's replication loop (using `next_index` and `match_index`).
- [x] Acceptance: A Leader can successfully replicate a manual log entry to at least one Follower.

## Step 1: Initialize the AI Veto gRPC Server (Mock Mode) [x]

- [x] Implement `PolicyService` in `crates/ai-veto` using `tonic`.
- [x] Return deterministic "Mock Approval" for all requests.
- [x] Acceptance: `ai-veto` binary starts and responds to `EvaluateProposal` RPCs.

## Step 2: Implement Leader Egress Bridge (`raft-node`) [x]

- [x] Create `GrpcVetoRelay` in `crates/raft-node/src/service/veto.rs`.
- [x] Implement the `VetoRelay` trait to call the `ai-veto` gRPC service.
- [x] Acceptance: Leader successfully calls out to `ai-veto`.

## Step 3: Implement Leader Proposal Logic (`raft-node`)

- [ ] Update `IngressDispatcher` in `crates/raft-node/src/service/ingress.rs`.
- [ ] Implement `ProposeMutation` for the `Leader` state:
  - [ ] Call `ai_vet_relay.evaluate()`.
  - [ ] If approved: Append to log, replicate via `AppendEntries`, and return `COMMITTED` once a quorum acknowledges.
- [ ] Acceptance: Client receives a final status from the Leader.

## Step 4: Implement the Client CLI REPL (`client-cli`)

- [ ] Implement a "Smart Client" with `client_id` (UUID) and `sequence_id`.
- [ ] Build an interactive REPL loop with Leader Redirection handling.
- [ ] Implement commands: `add <item> <qty> [unit] [category]`, `query`.
- [ ] Acceptance: User can interact with the cluster from the CLI and automatically find the leader.

## Step 5: Full Round-Trip Verification

- [ ] Create `scripts/phase4_round_trip.py` integration test.
- [ ] Verify: `client-cli` -> `follower` -> (hint) -> `leader` -> `ai-veto` -> `leader` -> `replication` -> `committed`.
- [ ] Acceptance: 100% success rate on the round-trip integration test.
