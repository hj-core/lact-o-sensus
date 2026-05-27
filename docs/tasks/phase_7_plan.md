# Phase 7 Implementation Plan: Log Compaction & Snapshotting

## Task 1: Protobuf Contracts & Trait Boundaries

- **Goal**: Define the clinical contracts for snapshot synchronization (`InstallSnapshot` RPC) and extend the boundary traits to support state serialization and restoration.
- **Affected Files**:
  - `crates/common/proto/raft.proto`
  - `crates/common/src/raft_api.rs`
  - `crates/common/src/types/trace.rs`
- **Major Steps**:
    1. Add `InstallSnapshotRequest` and `InstallSnapshotResponse` messages to `raft.proto`.
    2. Add the `InstallSnapshot` RPC to the `ConsensusService` definition.
    3. Extend the `StateMachine` trait in `raft_api.rs` with `snapshot(&self) -> Result<Vec<u8>, Self::Error>` and `install_snapshot(&self, last_included_index: LogIndex, data: &[u8]) -> Result<(), Self::Error>`.
    4. Add the `RaftCompaction` variant to the `ClinicalTarget` enum in `types/trace.rs` and map it to the `"raft::compaction"` target string (ADR 010).
- **Acceptance Tests**:
  - **TDD Requirement:** All new logic must be driven by failing tests (Red) before implementation (Green).
  - Identify and update existing mock implementations of `StateMachine` in `crates/raft-engine` test modules to satisfy the new trait bounds (preventing compilation failures).
  - Write a failing test ensuring the `ConsensusService` routing can reach the new `InstallSnapshot` endpoint (initially returning `Unimplemented`).
- **Consequences**: The generic consensus boundary now explicitly requires state machines to support serialization and atomic restoration.
- **Caveats**: We are introducing large payloads (`data: bytes`) into the RPC. This prototype will rely on gRPC's default message limits; production systems might require streaming.
- **Verification**: `cargo +nightly fmt --all`, `cargo test --all-features`, `cargo clippy --all-targets -- -D warnings`.
- **Draft Commit Message**: `feat(common): define InstallSnapshot contract and state machine traits`

---

## Task 2: Persistent Snapshot Metadata & Log Truncation

- **Goal**: Enhance the physical storage layer (`SledStorage`) to persist snapshot metadata and implement robust, synchronous log truncation.
- **Affected Files**:
  - `crates/raft-engine/src/storage.rs` (Implementation and trait)
- **Major Steps**:
    1. Extend `LogStorage` trait with methods for snapshot metadata: `save_snapshot_metadata(last_included_index: LogIndex, term: Term)`, `last_included_index() -> LogIndex`, and `last_included_term() -> Term`.
    2. Implement these methods in `SledStorage`, storing the values atomically in the existing `meta` tree using a `sled::Batch`.
    3. Update `last_log_index()` and `last_log_term()` in `SledStorage` to return the maximum of the `log` tree's highest entry OR the `last_included_index`/`last_included_term` from the metadata. This prevents returning `0` if the log is completely truncated.
    4. Implement the `truncate_log_front(up_to_index: LogIndex)` logic in `SledStorage` to physically remove old entries from the `log` tree.
    5. Ensure all metadata saves and truncations execute a synchronous `self.db.flush()` to satisfy the Crash-Recovery mandate (ADR 001).
    6. **Safety & Telemetry Mandate:** To protect the Stability Invariant (ADR 011), the heavy I/O of `truncate_log_front` and `db.flush()` MUST be wrapped in `tokio::task::spawn_blocking`. All compaction events MUST use the `raft::compaction` telemetry target and include the `last_included_index` field (ADR 010).
- **Acceptance Tests**:
  - **TDD Requirement:** All new logic must be driven by failing tests (Red) before implementation (Green).
  - Update existing mock implementations of `LogStorage` in test suites to support the new metadata methods.
  - Write a failing test verifying that `truncate_log_front` correctly removes older entries while preserving newer ones.
  - Write a failing test verifying persistence and retrieval of snapshot metadata across `SledStorage` instantiations.
- **Consequences**: The physical log can now be bounded, preventing unbounded disk growth.
- **Caveats**: `truncate_log_front` and `flush` are blocking I/O operations. They are offloaded to `spawn_blocking`, but excessive disk I/O could still impact overall system latency.
- **Verification**: `cargo +nightly fmt --all`, `cargo test --all-features`, `cargo clippy --all-targets -- -D warnings`.
- **Draft Commit Message**: `feat(engine): implement persistent log truncation and snapshot metadata`

---

## Task 3: State Machine Serialization (Lacto-FSM)

- **Goal**: Implement the semantic serialization and atomic restoration of the persistent grocery inventory and session tables.
- **Affected Files**:
  - `crates/lacto-fsm/build.rs` (New)
  - `crates/lacto-fsm/proto/internal.proto` (New)
  - `crates/lacto-fsm/src/lib.rs`
- **Major Steps**:
    1. Define a new internal Protobuf message `SnapshotData` (containing repeated `GroceryItem`, `SessionRecord`, and `last_effective_time`) in `crates/lacto-fsm/proto/internal.proto`. Configure `tonic-build` in `build.rs`.
    2. Implement `StateMachine::snapshot`. Iterate over the `inventory`, `sessions`, and `meta` `sled` trees, serialize their contents into `SnapshotData`, and encode to bytes. (Note: Consistency is guaranteed by the Task 4 orchestrator pausing application during this call).
    3. Implement `StateMachine::install_snapshot`. Utilize the **Restoration Tombstone** protocol (ADR 011) to ensure crash safety without full-DB transactions:
       - Write a `restoring=true` flag to the meta tree and flush.
       - Clear the inventory and session trees.
       - Use `sled::Batch` to bulk-insert the restored data.
       - Remove the `restoring=true` flag, update `last_applied_index`, and perform a final flush.
    4. Update `LactoStore::new()` to check for the `restoring=true` flag on startup and purge all data if found.
    5. Enforce the "Halt Mandate" (ADR 009): any deserialization failure during `install_snapshot` must return an error that causes a panic.
    6. **Halt Forensics:** Any invariant-triggered panic during restoration MUST emit a structured `error!` event under the `clinical::telemetry` target containing the `trace_id` and the invalid `last_included_index` (Rule 15).
- **Acceptance Tests**:
  - **TDD Requirement:** All new logic must be driven by failing tests (Red) before implementation (Green).
  - Write a failing test populating a `LactoStore`, triggering `snapshot()`, creating a new empty `LactoStore`, running `install_snapshot()`, and verifying identical state (inventory, sessions, and time).
- **Consequences**: The entire application state can now be serialized and restored from an opaque byte vector.
- **Caveats**: Loading the entire database into memory for serialization is an O(N) memory spike. This is acceptable for the prototype's scale but a caveat for production.
- **Verification**: `cargo +nightly fmt --all`, `cargo test --all-features`, `cargo clippy --all-targets -- -D warnings`.
- **Draft Commit Message**: `feat(fsm): implement atomic serialization and restoration of sled trees`

---

## Task 4: Log Compaction Trigger & InstallSnapshot RPC Handlers

- **Goal**: Integrate the snapshot mechanisms into the Raft orchestrator, triggering compaction automatically based on configuration, and handling incoming sync requests.
- **Affected Files**:
  - `crates/raft-engine/src/config.rs`
  - `crates/raft-engine/src/consensus.rs`
  - `crates/raft-engine/src/node.rs`
  - `crates/raft-engine/src/service/consensus.rs`
- **Major Steps**:
  1. Update `RaftConfig` in `config.rs` to include `snapshot_threshold: u64` (defaulting to 10,000) and validate it is > 0.
  2. Add logic to trigger log compaction when `last_log_index - last_included_index > config.snapshot_threshold`. Crucially, per ADR 011, this MUST:
     - Pause the application of log entries to the `StateMachine` (buffering new commits in the log).
     - Offload the `StateMachine::snapshot()` call to a background worker (`tokio::task::spawn_blocking`).
     - Resume application once the snapshot is complete.
  3. **Causal Correlation:** The background worker MUST inherit the parent `tracing` span or explicitly propagate the `trace_id` to ensure that "Snapshot Latency" is visible in clinical traces.
  4. **Snapshot Finalization:** Once the background worker completes serialization, it must re-acquire the `ConsensusShell` write lock to safely trigger `truncate_log_front()`, update snapshot metadata, and apply any entries committed during the freeze.
  5. In `shell.rs`, implement `handle_install_snapshot` logic using the **Non-Blocking Handoff (ADR 011)**:
     - Validate term and last_included_index while holding the write lock.
     - Enable the `is_snapshotting` flag (Freeze-Apply) to prevent concurrent FSM mutations.
     - Use `tokio::task::spawn_blocking` to call `StateMachine::install_snapshot()`.
     - Once the task finishes, re-acquire the lock to finalize snapshot metadata, advance the commit index, and clear the `is_snapshotting` flag.
  6. In `service/consensus.rs`, wire the incoming `InstallSnapshotRequest` gRPC call to the internal consensus handler.
- **Acceptance Tests**:
  - **TDD Requirement:** All new logic must be driven by failing tests (Red) before implementation (Green).
  - Write unit tests verifying followers correctly update their internal state and `LogStorage` when receiving an `InstallSnapshot`.
  - Write unit tests verifying that the tick loop continues to increment time even while a snapshot is being installed (proving non-blocking behavior).
  - Write unit tests verifying followers reject snapshots with stale terms.
  - Write unit tests verifying `snapshot_threshold` parsing and validation.
- **Consequences**: Followers can now jump forward in time without needing every historical log entry. The compaction threshold is configurable per node.
- **Caveats**: Snapshot generation might block the main Raft event loop if not offloaded. We must ensure it's executed safely (e.g., `spawn_blocking`) to prevent heartbeat starvation.
- **Verification**: `cargo +nightly fmt --all`, `cargo test --all-features`, `cargo clippy --all-targets -- -D warnings`.
- **Draft Commit Message**: `feat(engine): orchestrate automatic compaction and snapshot installation`

---

## Task 5: Leader Synchronization Fallback & System Verification

- **Goal**: Update the leader's replication loop to gracefully fallback to `InstallSnapshot` when a follower is lagging, and implement an end-to-end smoke test using a Mock AI Veto server to generate high-throughput load.
- **Affected Files**:
  - `crates/raft-engine/src/consensus.rs`
  - `crates/raft-engine/src/tick.rs`
  - `crates/raft-engine/src/peer.rs`
  - `crates/mock-veto/Cargo.toml` (New)
  - `crates/mock-veto/src/main.rs` (New)
  - `scripts/smoke_test.py`
- **Major Steps**:
  1. Modify the `send_append_entries` logic: if `next_index[peer]` <= `last_included_index`, switch strategy and issue an `InstallSnapshot` RPC instead.
  2. Implement the leader-side dispatch logic for `InstallSnapshotRequest`, utilizing the previously generated snapshot from the `StateMachine`.
  3. Upon a successful `InstallSnapshotResponse`, advance `next_index[peer]` and `match_index[peer]` to `snapshot.last_included_index`.
  4. Create `crates/mock-veto`, a tiny Rust binary that implements `EvaluateProposal` to instantly return `is_approved: true`.
  5. Update `smoke_test.py` to optionally launch this `mock-veto` binary instead of the real LLM service.
  6. Add `test_snapshot_installation` to `smoke_test.py`.
- **Acceptance Tests**:
  - **TDD Requirement:** All new logic must be driven by failing tests (Red) before implementation (Green).
  - Implement BDD unit scenario: A follower goes offline, the leader processes entries and compacts its log, the follower reconnects, and the leader successfully catches it up via `InstallSnapshot`.
  - The new smoke test (`test_snapshot_installation`) must perform the following:
    - Start a cluster pointing its Veto Oracle configuration to the `mock-veto` server.
    - Configure an artificially low log compaction threshold (e.g., `snapshot_threshold = 20`).
    - Partition/Kill Node C.
    - Blast the Leader (Node A) with >30 concurrent mutations using the `MutationFlooder` to force a snapshot generation and log truncation.
    - Reconnect/Restart Node C.
    - Poll Node C's `query_state` endpoint until its inventory successfully converges with Node A, proving the snapshot was installed.
- **Consequences**: The cluster can now gracefully recover partitioned nodes even after their missing history has been garbage collected. We also gain a high-throughput testing configuration.
- **Caveats**: We decided against writing a Python gRPC mock inside the test script because managing async Python servers alongside synchronous shell commands introduces test fragility. The dedicated Rust `mock-veto` binary provides a robust, compile-time verified test double.
- **Verification**: `cargo +nightly fmt --all`, `cargo test --all-features`, `cargo clippy --all-targets -- -D warnings`, `python3 scripts/smoke_test.py`.
- **Draft Commit Message**: `feat(engine): implement leader fallback to InstallSnapshot and add e2e verification`
