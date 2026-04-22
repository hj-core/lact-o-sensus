# Phase 4.5 Task List: Fortress Hardening (Identity & Type Safety)

## 🎯 Goal

Resolve "Legacy Debt" by aligning infrastructure with refined "Fortress" mandates. Ensure cluster isolation via centralized gRPC interceptors, and provide high-availability guarantees for the client via a durable Write-Ahead Log (WAL) and resilient retry logic.

---

## ✅ Completed Tasks

- [x] **NewType Migration (ADR 001/005):** Verified that all domain identifiers (`LogIndex`, `Term`, `SequenceId`, `ClientId`, `NodeId`) have transitioned from primitive types to self-validating NewTypes. These are now consistently used across `raft-node`, `client-cli`, and `common`.

---

## 🏗️ Task Hierarchy & Git Commit Strategy

### Step 1: Centralized Identity Interceptors (ADR 004/005) [x]

**Commit:** `feat(raft): implement centralized identity gRPC interceptors`

- [x] Create `IdentityInterceptor` in `crates/raft-node/src/service/common.rs`.
  - [x] Extract `x-cluster-id` and `x-target-node-id` from gRPC metadata.
  - [x] Validate against local `ClusterId` and `NodeId`.
  - [x] Return `Status::unauthenticated` or `Status::invalid_argument` on mismatch.
- [x] Register the interceptor in `crates/raft-node/src/main.rs` for both `ConsensusService` and `IngressService`.
- [x] Remove manual identity checks from `service/consensus.rs` and `service/ingress.rs`.
- [x] **Verification:**
  - [x] Unit tests for `IdentityInterceptor` with mocked metadata.
  - [x] Integration test: Verified that a node rejects traffic missing identity headers. (Note: Step 2 is required for cluster recovery).

### Step 2: Protocol Invariant Enforcement [x]

**Commit:** `refactor(common): upgrade identity protocol for target_node_id`

- [x] Update `crates/raft-node/src/peer.rs` to attach `x-cluster-id` and `x-target-node-id` to all outbound `AppendEntries` and `RequestVote` calls.
- [x] Update `crates/client-cli/src/client.rs` to attach these headers to `ProposeMutation` and `QueryState` calls.
- [x] **Verification:**
  - [x] `smoke_test.py` confirms cluster still achieves consensus.
  - [x] Wireshark/Metadata logging confirms headers are present on the wire (Verified via `IdentityInterceptor` success).

### Step 3: Client-Side WAL for Mutation Intents (ADR 001) [x]

**Commit:** `feat(client): implement client-side WAL using sled for intent durability`

- [x] Integrate `sled` into `crates/client-cli`.
- [x] Define `IntentWal` manager to handle:
  - [x] `append(sequence_id, intent)`: Save to `sled` before RPC.
  - [x] `remove(sequence_id)`: Delete after `COMMITTED` response.
  - [x] `recover()`: Return all pending intents on startup.
- [x] Update `LactoClient::propose` to utilize the WAL.
- [x] Implement a recovery task in `main.rs` that re-proposes pending intents from the WAL.
- [x] **Verification:**
  - [x] Unit tests for `IntentWal` CRUD operations.
  - [x] Crash Test: Kill `client-cli` after it logs "Intent Persisted" but before response; verify re-proposal on restart.

### Step 4: Resilient Client Loop (ADR 003) [x]

**Commit:** `feat(client): implement exponential backoff and jitter for mutation retries`

- [x] Implement `retry_with_backoff` utility in `client.rs` (via `calculate_backoff` instance method).
  - [x] Strategy: Exponential backoff (initial 100ms, max 5s) with 20% jitter.
- [x] Align `DEFAULT_MUTATION_TIMEOUT` with 30s mandate.
- [x] Handle `NotLeader` redirection within the retry loop (via `reconcile_routing_failure`).
- [x] **Verification:**
  - [x] Unit test for backoff math (monotonicity and jitter bounds).
  - [x] Integration test: Mock a congested cluster and verify client retries without "thundering herd" behavior.

---

## 📈 Verification Summary

- [x] **NewType Migration Check:** Verified that `Term`, `LogIndex`, `SequenceId`, `ClientId`, and `NodeId` are used throughout `raft-node` and `client-cli`. (Completed).
- [x] **Identity Integrity:** Cluster rejects misconfigured traffic via middleware.
- [x] **Durability:** Client recovers pending mutations after a crash.
- [x] **Stability:** Retries are disciplined and respect the 30s timeout window.
