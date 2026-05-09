# Phase 6 Task List: Persistence & Session Integrity (sled & EOS)

## 🎯 Goal

Implement Exactly-Once Semantics (EOS) and transition to persistent disk storage using an **Isolated Storage** architecture (Split DBs). Implement a **Strictly Linearizable Query Path** to provide visibility into the State Machine before and during the persistence transition.

---

## 🏗️ Task Hierarchy & Git Commit Strategy

### Step 1: The Linearizable Query Path (Instrumentation) [DONE]

**Commit:** `feat(gateway): implement InventorySource and linearizable query path`

- [x] Define `InventorySource` trait in `crates/gateway/src/ingress.rs`.
- [x] Update `RaftHandle` in `crates/common/src/raft_api.rs` to support `verify_leadership()` (Quorum Read).
- [x] Implement `verify_leadership` in `crates/raft-node/src/service/handle.rs` (forcing a heartbeat or using the local epoch).
- [x] Implement `IngressDispatcher::query_state` to perform the Quorum Read, fetch data via `InventorySource`, and support basic `query_filter` matching on `item_key`.
- [x] Update `raft-node/src/main.rs` to pass the `LactoStore` as the `InventorySource`.
- **Acceptance Tests (TDD):**
  - [x] Unit test: `query_state` returns data filtered by `item_key`.
  - [x] Integration test: A deposed leader correctly rejects a `query_state` request due to failing the Quorum Read.

### Step 2: Isolated Storage: Consensus Log [DONE]

**Commit:** `feat(raft): implement isolated sled persistence for consensus log`

- [x] Initialize a dedicated `sled::Db` instance (e.g., `data_dir/log`) in `raft-node/src/main.rs`.
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
  - [x] Initialize the `fsm` database handle in `raft-node/src/main.rs`.
  - [x] Refactor `LactoStore` to use `sled::Tree` for inventory storage.
  - [x] Update `InventorySource` to stream data from `sled`.
- **Acceptance Tests (TDD):**
  - [x] Unit test: `LactoStore::apply` persists items across `sled` instance restarts.
  - [x] Integration test: `smoke_test.py` verifies that inventory survives a total cluster shutdown.

### Step 3.2: Persistent Session Table & Recovery

**Commit:** `feat(raft): implement persistent Session Table and FSM recovery`

- **Description:** Implement exactly-once metadata and startup log-replay logic.
- **Changes:**
  - [ ] Implement the **Session Table** (ADR 006) within the FSM database (`client_id` -> `last_seq` mapping).
  - [ ] Implement `RaftHandle::check_session` by querying the persistent Session Table.
  - [ ] Implement **FSM Recovery**: On startup, compare the FSM's last applied index with the Raft log and replay missing entries.
- **Acceptance Tests (TDD):**
  - [ ] Unit test: `check_session` correctly identifies duplicate sequence IDs from disk.
  - [ ] Integration test: `smoke_test.py` verifies that client deduplication works across node restarts.

### Step 4: Exactly-Once Semantics (EOS) Barrier

**Commit:** `feat(gateway): enforce min_state_version for read-your-writes consistency`

- **Description:** Enforce "Read-Your-Writes" consistency by blocking queries until the local State Machine catches up to the client's requested index.
- **Changes:**
  - [ ] Update `query_state` to accept and enforce `min_state_version`.
  - [ ] If the local FSM index is lower than `min_state_version`, asynchronously wait until the state machine catches up (via `tokio::sync::watch` on the Consensus Progress channel).
- **Acceptance Tests (TDD):**
  - [ ] Unit test: Query requests block and eventually resolve when the FSM index advances past `min_state_version`.

### Step 5: The Halt Mandate & Chaos Testing

**Commit:** `test(system): verify crash recovery and Poison-then-Panic invariants`

- **Description:** Guarantee safety during recovery by aggressively testing crash scenarios and divergence.
- **Changes:**
  - [ ] Implement the `Poison-then-Panic` sequence (ADR 009) if the FSM index ever exceeds the Raft commit index during recovery (which indicates corruption).
  - [ ] Expand `smoke_test.py` to aggressively kill nodes during mutation proposals.
- **Acceptance Tests (TDD):**
  - [ ] Integration test: "Chaos Testing" verifies 100% data integrity across all 3 nodes after SIGKILL during active replication.

---

## 📈 Completion Status

- **Total Progress:** 60%
- **Current Focus:** Step 3.2: Persistent Session Table & Recovery
