# ADR 011: Asynchronous Log Compaction and State Snapshotting

## Metadata

- **Date:** 2026-05-15
- **Status:** Proposed
- **Scope:** Consensus Reliability and Physical Storage
- **Primary Goal:** Mitigate unbounded log growth via state machine snapshotting while preserving the Stability Invariant.
- **Last Updated:** 2026-05-27

## Context

As the Lact-O-Sensus cluster operates, the Raft consensus log grows indefinitely. Without a mechanism to discard obsolete entries, the system will eventually exhaust its physical storage capacity and require an unacceptable amount of time to recover state upon a node restart.

To address this, we must implement log compaction and snapshotting as defined in §7 of the Raft consensus algorithm. However, this implementation must adhere to our existing architectural mandates:

1. **The Tri-Layer Onion (ADR 009):** The separation between physical log storage (`sled`), logical consensus orchestration, and the application state machine (`lacto-fsm`) must be maintained.
2. **The Stability Invariant (ADR 003):** Generating a snapshot must not block the deterministic `tick` loop, as this would cause heartbeat starvation and trigger disruptive, false-positive leader elections.
3. **The Halt Mandate (ADR 009):** State restoration is a destructive action; failures during this process compromise physical integrity.

## Decision

We will implement asynchronous snapshot generation and asymmetric log compaction using unified state serialization.

### 1. Unified State Serialization (Non-Streaming Prototype)

We will serialize the entire application state (Inventory, Session Deduplication Table, and Clinical Clock) into a single, contiguous byte array (`Vec<u8>`) using a new Protobuf message (`SnapshotData`).

### 2. Asynchronous Snapshot Generation

When a node determines its log size exceeds the configurable `snapshot_threshold`, the Raft orchestrator must offload the `StateMachine::snapshot()` execution to a background worker pool (e.g., `tokio::task::spawn_blocking`).

To guarantee **Point-in-Time Consistency** without relying on database-level locking or high-overhead transactions, the orchestrator MUST implement a **"Freeze-Apply" mechanism**:

- **Phase 1:** The main Raft event loop pauses the application of new log entries to the `StateMachine`. New entries continue to be appended to the physical log to preserve cluster liveness.
- **Phase 2:** The `StateMachine::snapshot()` method is called in the background. Since the `apply()` method is not being called, the FSM state is logically frozen.
- **Phase 3:** Once the snapshot is complete, the orchestrator resumes the application of the buffered log entries.

### 3. Asymmetric Compaction Safety

Nodes will monitor their own logs and truncate them independently based on local configurations. The `last_included_index` and `last_included_term` metadata are the authoritative source of truth for a compacted log's history. These two values MUST be updated atomically (e.g., via a database transaction or batch) to prevent inconsistent logical horizons during crash recovery.

### 4. Atomic Restoration and The Halt Mandate

During an `InstallSnapshot` RPC, a follower must cleanly wipe its current state and restore it from the provided payload. To preserve the **Stability Invariant (ADR 003)**, the heavy I/O of restoration MUST NOT block the Raft event loop.

To prevent partial installations from corrupting the node upon an unexpected crash, the FSM must implement a **"Restoration Tombstone" protocol**:

1. **Mark as Dirty:** Before clearing the data trees, a `restoring=true` flag MUST be written to the metadata tree and synchronously flushed to disk.
2. **Execute Restoration:** The FSM clears the old data and applies the new snapshot data in batches.
3. **Mark as Clean:** The `restoring=true` flag is removed, the new `last_applied_index` is written, and a final synchronous flush is performed.
4. **Startup Sanitization:** Upon initialization, the FSM MUST check for the `restoring=true` flag. If found, it indicates a crash during restoration. The node MUST wipe all its data trees, ensuring it wakes up completely empty (Index 0) and relies on the leader to resend the snapshot.

If any error occurs during deserialization or physical disk write during the restoration phase, the node must immediately transition to the `Poisoned` state and panic.

## Rationale

- **Non-Streaming Prototype:** While streaming (chunking) the snapshot over gRPC is more memory-efficient for multi-gigabyte datasets, implementing a robust, resumable streaming protocol across the `sled` database boundary introduces significant asynchronous complexity. To rapidly verify the logical correctness of the Raft compaction mechanisms in Phase 7, we will utilize gRPC's default payload mechanics. Streaming snapshots are deferred to Phase 9.
- **Asynchronous Execution:** Serializing the entire database is an O(N) CPU and I/O operation. If executed synchronously within the main Raft event loop, the node would miss incoming heartbeats or fail to send them, violating the Stability Invariant. The main loop must only be blocked for the fraction of a millisecond required to update physical metadata (`last_included_index`) and truncate the log after the background worker returns the generated payload.
- **Orchestrated Consistency (Freeze-Apply):** While `sled::transaction` provides atomicity, using it for full-database iteration in a high-concurrency environment risks an "Infinite Retry" loop due to Optimistic Concurrency Control (OCC). By pausing the `apply()` pipeline at the orchestration layer, we guarantee a stable state for serialization without the overhead or performance risks of database-level transactions.
- **Local Compaction Safety:** Log truncation is a local physical optimization. A cluster operates safely even if every node has truncated its log at a different index, provided the physical metadata correctly bridges the gap between the application state machine and the remaining log entries.
- **Strict Halt on Restoration Failure:** Snapshot installation is destructive. A partial installation leaves the node in a corrupt, unrecoverable state that no longer matches the cluster consensus. Panicking satisfies the Halt Mandate (ADR 009) and prevents a "Zombie Node" scenario.

## Consequences

### Pros

- **Mitigated Disk Growth:** The Raft log is now bounded by the `snapshot_threshold`, ensuring predictable disk utilization.
- **Rapid Synchronization:** A node recovering from an extended partition will receive a single `InstallSnapshot` RPC instead of replaying thousands of historical log entries.

### Cons

- **Memory Overhead:** Because we defer streaming to Phase 9, snapshot generation will temporarily spike RAM usage proportional to the total database size.
- **RPC Payload Limits:** The `InstallSnapshotRequest` may exceed default gRPC message size limits (typically 4MB). The network layer (`gateway`) must be configured to accept larger payloads.
