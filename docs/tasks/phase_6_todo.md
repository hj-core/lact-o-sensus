# Phase 6 Task List: Persistence & Session Integrity (sled & EOS)

## 🎯 Goal

Implement Exactly-Once Semantics (EOS) and transition to persistent disk storage using an **Isolated Storage** architecture (Split DBs). Implement a **Strictly Linearizable Query Path** to provide visibility into the State Machine before and during the persistence transition.

---

## 🏗️ Task Hierarchy & Git Commit Strategy

### Step 1: The Linearizable Query Path (Instrumentation) [DONE]

**Commit:** `feat(gateway): implement SessionProvider / InventoryReader and linearizable query path`

- [x] Define `SessionProvider` and `InventoryReader` traits in `crates/common/src/raft_api.rs`.
- [x] Update `ConsensusHandle` in `crates/common/src/raft_api.rs` to support `verify_leadership()` (Quorum Read).
- [x] Implement `verify_leadership` in `crates/raft-engine/src/service/handle.rs` (forcing a heartbeat or using the local epoch).
- [x] Implement `IngressDispatcher::query_state` to perform the Quorum Read, fetch data via `InventoryReader`, and support basic `query_filter` matching on `item_key`.
- [x] Update `main.rs` (originally in `raft-node`) to pass the `LactoStore` as both `SessionProvider` and `InventoryReader`.
- **Acceptance Tests (TDD):**
  - [x] Unit test: `query_state` returns data filtered by `item_key`.
  - [x] Integration test: A deposed leader correctly rejects a `query_state` request due to failing the Quorum Read.

### Step 2: Isolated Storage: Consensus Log [DONE]

**Commit:** `feat(raft): implement isolated sled persistence for consensus log`

- [x] Initialize a dedicated `sled::Db` instance (e.g., `data_dir/log`) in `main.rs` (originally in `raft-node`).
- [x] Update `RaftNode` (or a dedicated storage struct) to read/write `current_term`, `voted_for`, and the log entries to `sled`.
- [x] Ensure synchronous `fsync` (`db.flush()`) on every append to satisfy crash-recovery mandates.
- **Acceptance Tests (TDD):**
  - [x] Unit test: Log entries appended to `sled` can be successfully retrieved after a `db` restart.
  - [x] Integration test: Node restarts and correctly initializes `current_term` and `voted_for` from disk.

### Step 2.5: The Unified Ledger (Veto Logging) [DONE]

**Commit:** `feat(contract): implement Unified Ledger by logging AI vetoes`

- **Description:** Align the Gateway and Ledger by recording rejections as first-class Raft events.
- **Changes:**
  - [x] Update `app.proto`: Add `MutationStatus` (APPROVED/VETOED) and `moral_justification` to `CommittedMutation`.
  - [x] Refactor `IngressDispatcher` (Gateway): Propose **every** evaluation outcome to Raft, regardless of the AI's decision.
  - [x] Update `LactoStore` (FSM): Apply every log entry to the Session Table, but update Inventory only for `APPROVED` status.
  - [x] **Documentation Audit**: Review and update `roadmap.md` and `ADR 006` / `ADR 007` to reflect the transition from "Firewall Veto" to "Ledger Veto."
- **Acceptance Tests (TDD):**
  - [x] Unit test: `LactoStore` correctly acknowledges a vetoed mutation without changing inventory.
  - [x] Integration test: `smoke_test.py` verifies that a vetoed mutation appears in the Raft log of all nodes.

### Step 3.1: Persistent FSM (Physical Inventory) [DONE]

**Commit:** `feat(raft): implement persistent FSM inventory and total cluster restart verification`

- **Description:** Transition the State Machine from a volatile `HashMap` to a persistent `sled` tree.
- **Changes:**
  - [x] Initialize the `fsm` database handle in `main.rs` (originally in `raft-node`).
  - [x] Refactor `LactoStore` to use `sled::Tree` for inventory storage.
  - [x] Update `InventoryReader` to stream data from `sled`.
- **Acceptance Tests (TDD):**
  - [x] Unit test: `LactoStore::apply` persists items across `sled` instance restarts.
  - [x] Integration test: `smoke_test.py` verifies that inventory survives a total cluster shutdown.

### Step 3.2: Persistent Session Table (Exactly-Once Semantics) [DONE]

**Commit:** `feat(raft): implement permanent Session Table and Secure Clinical reporting`

- **Description:** Implement durable tracking of client session records to enforce sequence strictness and provide linearizable replays (ADR 006).
- **Changes:**
  - [x] Initialize the `sessions` tree within the FSM database tree in `LactoStore`.
  - [x] **Eternal Persistence:** Implement the "No-Purge" policy for session records to eliminate the Double-Bootstrap hazard.
  - [x] **ADR 006 Alignment:** Update `SessionRecord` (Protobuf) to include `last_activity_effective_time`.
  - [x] **Opaque Firewall:** Update `IngressDispatcher` to use `check_session` for Layer 2 deduplication and return `**Secure Clinical**` opaque error messages.
  - [x] **Stateful Determinism:** Implement persistent `last_effective_time` metadata in the State Machine, derived from consensus log timestamps.
  - [x] **Strict Apply Logic** in `LactoStore::apply`:
    - [x] **Deduplication:** If `seq == last_seen`, return cached metadata (replay path).
    - [x] **Continuity:** Reject sequence gaps (`seq > last_seen + 1`).
    - [x] **Atomic Commitment:** Persist `SessionRecord` (including `last_activity_effective_time`), inventory changes, `last_applied`, and `last_effective_time` in a single `sled` transaction.
- **Acceptance Tests (TDD):**
  - [x] Unit tests: Verify that the FSM rejects gaps.
  - [x] Security test: Verify that firewall error messages do not disclose internal sequence numbers.
  - [x] Integration test: Verify that a client receives the exact same response (with justification and timestamp) when retrying a mutation after a leader failover.

### Step 3.3: FSM Recovery (Cold-Boot Replay) [DONE]

**Commit:** `feat(raft): implement cold-boot FSM recovery loop`

- **Description:** Synchronize the State Machine with the Consensus Log during startup.
- **Changes:**
  - [x] Implement `RecoveryManager` in `crates/raft-engine/src/recovery.rs` to decouple replay logic from the composition root.
  - [x] Implement the replay loop: compare `fsm.last_applied_index()` with `storage.last_committed()` and apply missing entries.
  - [x] **Safety Barrier:** Implement the **Poison-then-Panic** protocol if `fsm > storage.commit` (indicates log regression or disk corruption).
  - [x] Integrate recovery into `node-server/src/main.rs` to block gRPC listener start until state convergence is achieved.
- **Acceptance Tests (TDD):**
  - [x] **Unit Test (Recovery Logic):** Verify that `RecoveryManager` correctly identifies and applies missing log entries.
  - [x] **Unit Test (Safety Guard):** Verify that `RecoveryManager` triggers `Poison-then-Panic` (fatal error) if the FSM index is ahead of the Log's commit index.
  - [x] **Integration Test (Cold-Boot Convergence):** Verify that a node killed after a log commit but before FSM apply correctly recovers state on restart.
  - [x] **Stress Test (Idempotent Chaos):** Verify that replaying the entire log (via manual index reset) results in an identical final state.

### Step 4: Exactly-Once Semantics (EOS) Barrier [DONE]

**Commit:** `feat(gateway): enforce min_state_version for read-your-writes consistency`

- **Description:** Enforce "Read-Your-Writes" consistency by blocking queries until the local State Machine catches up to the client's requested index.
- **Changes:**
  - [x] **Reactive Instrumentation:**
    - Add `last_applied` to `ConsensusProgress` in `crates/raft-engine/src/node.rs`.
    - Update `LogicalNode::try_consensus_progress` to populate this field.
  - [x] **Handle Extension:**
    - Add `await_apply(index: LogIndex)` to the `ConsensusHandle` trait.
    - Implement `await_apply` in `LocalRaftHandle` using the `ConsensusShell` subscription.
    - Update `ConsensusStatus` to include `last_committed` for horizon checks.
  - [x] **Gateway Enforcement:**
    - Update `IngressDispatcher::query_state` to extract `min_state_version`.
    - Implement **Strict Horizon Check**: Reject queries for future-dated versions (Mandate 4.3).
    - Call `raft_handle.await_apply` before fetching inventory if a version is requested.
- **Acceptance Tests (TDD):**
  - [x] Unit test: `rejects_query_exceeding_horizon` verifies the strict EOS boundary.
  - [x] Integration test: `Read-Your-Writes Consistency` smoke test verifies end-to-end synchronization.

### Step 5: The Halt Mandate & Chaos Testing [DONE]

- **Description:** Guarantee safety by implementing the mandatory Poison-then-Panic machinery (ADR 009). This ensures that any node detecting an invariant violation (FSM failure, storage corruption, or recovery divergence) immediately halts to prevent cluster-wide state drift.
- **Changes:**
  - [x] **Safety Barrier:** Implement the **Poison-then-Panic** protocol in `LogicalNode`. Catch FSM `apply` errors and transition the node to `LogicalNode::Poisoned` before panicking.
  - [x] **Recovery Guard:** Implement the `Poison-then-Panic` sequence if the FSM index exceeds the Raft commit index during recovery.
  - [x] **Chaos Engineering:** Expand `smoke_test.py` with the **Replication Chaos Audit**, verifying data integrity under concurrent pressure.
  - [x] **Robustness Audit:** Implement Raft-native **Term Sovereignty** in the test runner to ensure deterministic leader discovery during rapid failover.
- **Acceptance Tests (TDD):**
  - [x] Unit test: Verify that a node transitions to `Poisoned` and then panics when the FSM returns an Invariant error.
  - [x] Integration test: **Replication Chaos Audit** verifies 100% data integrity and state machine parity across all 3 nodes after randomized SIGKILL during active replication.

---

## 📈 Completion Status

- **Total Progress:** 100%
- **Current Focus:** Phase 6 Complete (Persistence & EOS)
