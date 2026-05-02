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

### Step 2: Isolated Storage: Consensus Log

**Commit:** `feat(raft): implement isolated sled persistence for consensus log`

- **Description:** Persist the Raft physical state (Term, Vote, Log) to a dedicated `sled` database.
- **Changes:**
  - Initialize a dedicated `sled::Db` instance (e.g., `data_dir/log`) in `raft-node/src/main.rs`.
  - Update `RaftNode` (or a dedicated storage struct) to read/write `current_term`, `voted_for`, and the log entries to `sled`.
  - Ensure synchronous `fsync` (`db.flush()`) on every append to satisfy crash-recovery mandates.
- **Acceptance Tests (TDD):**
  - Unit test: Log entries appended to `sled` can be successfully retrieved after a `db` restart.
  - Integration test: Node restarts and correctly initializes `current_term` and `voted_for` from disk.

### Step 3: Isolated Storage: State Machine & Session Table

**Commit:** `feat(raft): transition LactoStore to sled and implement Session Table`

- **Description:** Persist the Application State and EOS tracking data using a second, strictly isolated `sled` database.
- **Changes:**
  - Initialize a second `sled::Db` instance (e.g., `data_dir/fsm`).
  - Transition `LactoStore` inventory from `HashMap` to the `sled` tree.
  - Implement the **Session Table** (ADR 006) within this FSM database to track `client_id` -> `last_sequence_id` and the corresponding `LogIndex`.
  - Implement recovery logic: on startup, compare the FSM applied index against the Raft commit index and replay logs if the FSM fell behind.
- **Acceptance Tests (TDD):**
  - Unit test: `LactoStore::apply` writes item data and updates the session table atomically in `sled`.
  - Unit test: Client deduplication successfully returns cached responses using the persisted Session Table.

### Step 4: Exactly-Once Semantics (EOS) Barrier

**Commit:** `feat(gateway): enforce min_state_version for read-your-writes consistency`

- **Description:** Enforce "Read-Your-Writes" consistency by blocking queries until the local State Machine catches up to the client's requested index.
- **Changes:**
  - Update `query_state` to accept and enforce `min_state_version`.
  - If the local FSM index is lower than `min_state_version`, asynchronously wait until the state machine catches up (via `tokio::sync::watch` on the Consensus Progress channel).
- **Acceptance Tests (TDD):**
  - Unit test: Query requests block and eventually resolve when the FSM index advances past `min_state_version`.

### Step 5: The Halt Mandate & Chaos Testing

**Commit:** `test(system): verify crash recovery and Poison-then-Panic invariants`

- **Description:** Guarantee safety during recovery by aggressively testing crash scenarios and divergence.
- **Changes:**
  - Implement the `Poison-then-Panic` sequence (ADR 009) if the FSM index ever exceeds the Raft commit index during recovery (which indicates corruption).
  - Expand `smoke_test.py` to aggressively kill nodes during mutation proposals.
- **Acceptance Tests (TDD):**
  - Integration test: "Chaos Testing" verifies 100% data integrity across all 3 nodes after SIGKILL during active replication.

---

## 📈 Completion Status

- **Total Progress:** 20%
- **Current Focus:** Step 2: Isolated Storage: Consensus Log
