# Phase 4.5 Task List: Fortress Hardening (Identity & Type Safety)

## 🎯 Goal

Resolve "Legacy Debt" by aligning infrastructure with refined "Fortress" mandates. Ensure cluster isolation via centralized gRPC interceptors, and provide high-availability guarantees for the client via a durable Write-Ahead Log (WAL) and resilient retry logic.

---

## ✅ Completed Tasks

- [x] **NewType Migration (ADR 001/005):** Verified that all domain identifiers (`LogIndex`, `Term`, `SequenceId`, `ClientId`, `NodeId`) have transitioned from primitive types to self-validating NewTypes. These are now consistently used across `raft-node`, `client-cli`, and `common`.

---

## 🏗️ Task Hierarchy & Git Commit Strategy

### Step 1: Centralized Identity Interceptors (ADR 004/005)

**Commit:** `feat(raft): implement centralized identity gRPC interceptors`

- [ ] Create `IdentityInterceptor` in `crates/raft-node/src/service/common.rs`.
  - [ ] Extract `x-cluster-id` and `x-target-node-id` from gRPC metadata.
  - [ ] Validate against local `ClusterId` and `NodeId`.
  - [ ] Return `Status::unauthenticated` or `Status::invalid_argument` on mismatch.
- [ ] Register the interceptor in `crates/raft-node/src/main.rs` for both `ConsensusService` and `IngressService`.
- [ ] Remove manual identity checks from `service/consensus.rs` and `service/ingress.rs`.
- [ ] **Verification:**
  - [ ] Unit tests for `IdentityInterceptor` with mocked metadata.
  - [ ] Integration test: A node rejects a `RequestVote` with the wrong `cluster_id` at the middleware layer.

### Step 2: Protocol Invariant Enforcement

**Commit:** `refactor(common): upgrade identity protocol for target_node_id`

- [ ] Update `crates/raft-node/src/peer.rs` to attach `x-cluster-id` and `x-target-node-id` to all outbound `AppendEntries` and `RequestVote` calls.
- [ ] Update `crates/client-cli/src/client.rs` to attach these headers to `ProposeMutation` and `QueryState` calls.
- [ ] **Verification:**
  - [ ] `smoke_test.py` confirms cluster still achieves consensus.
  - [ ] Wireshark/Metadata logging confirms headers are present on the wire.

### Step 3: Client-Side WAL for Mutation Intents (ADR 001)

**Commit:** `feat(client): implement client-side WAL using sled for intent durability`

- [ ] Integrate `sled` into `crates/client-cli`.
- [ ] Define `IntentWal` manager to handle:
  - [ ] `append(sequence_id, intent)`: Save to `sled` before RPC.
  - [ ] `remove(sequence_id)`: Delete after `COMMITTED` response.
  - [ ] `recover()`: Return all pending intents on startup.
- [ ] Update `LactoClient::propose` to utilize the WAL.
- [ ] Implement a recovery task in `main.rs` that re-proposes pending intents from the WAL.
- [ ] **Verification:**
  - [ ] Unit tests for `IntentWal` CRUD operations.
  - [ ] Crash Test: Kill `client-cli` after it logs "Intent Persisted" but before response; verify re-proposal on restart.

### Step 4: Resilient Client Loop (ADR 003)

**Commit:** `feat(client): implement exponential backoff and jitter for mutation retries`

- [ ] Implement `retry_with_backoff` utility in `client.rs`.
  - [ ] Strategy: Exponential backoff (initial 100ms, max 5s) with 20% jitter.
- [ ] Align `DEFAULT_MUTATION_TIMEOUT` with 30s mandate.
- [ ] Handle `NotLeader` redirection within the retry loop.
- [ ] **Verification:**
  - [ ] Unit test for backoff math (monotonicity and jitter bounds).
  - [ ] Integration test: Mock a congested cluster and verify client retries without "thundering herd" behavior.

---

## 📈 Verification Summary

- [ ] **NewType Migration Check:** Verified that `Term`, `LogIndex`, `SequenceId`, `ClientId`, and `NodeId` are used throughout `raft-node` and `client-cli`. (Completed).
- [ ] **Identity Integrity:** Cluster rejects misconfigured traffic via middleware.
- [ ] **Durability:** Client recovers pending mutations after a crash.
- [ ] **Stability:** Retries are disciplined and respect the 30s timeout window.
