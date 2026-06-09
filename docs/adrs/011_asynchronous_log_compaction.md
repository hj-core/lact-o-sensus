# ADR 011: Asynchronous Log Compaction and State Snapshotting

## Metadata

- **Date:** 2026-05-15
- **Status:** Proposed
- **Scope:** Consensus log compaction and state machine snapshotting; excludes client-side WAL compaction and the network transport of snapshot payloads.
- **Primary Goal:** Mitigate unbounded log growth via state machine snapshotting while preserving the Stability Invariant.
- **Last Updated:** 2026-06-09

## Context

As the Lact-O-Sensus cluster operates, the Raft consensus log grows indefinitely. Without a mechanism to discard obsolete entries, the system will eventually exhaust physical storage and require an unacceptable amount of time to recover state upon a node restart.

The Raft paper (§7) defines log compaction and snapshotting as the standard solution. However, the implementation must adhere to three existing architectural mandates:

1. **The Tri-Layer Onion (ADR 009):** The separation between physical log storage, logical consensus orchestration, and the application state machine must be maintained. The state machine and logical layers are strictly synchronous.
2. **The Stability Invariant (ADR 003):** Generating a snapshot involves heavy, blocking disk I/O. This must not block the deterministic Raft tick loop, as doing so would cause heartbeat starvation and trigger disruptive, false-positive leader elections.
3. **The Halt Mandate (ADR 009):** State restoration is a destructive action; failures during this process compromise physical integrity and must produce a terminal node state.

## Options Considered

- **Option A: Synchronous snapshotting (block the event loop).** Generate and install snapshots inline within the Raft tick loop. Rejected because the I/O latency violates ADR 003's Stability Invariant — blocking for snapshot duration would starve heartbeats and cause spurious leader elections.
- **Option B: Background snapshotting with database-level transactions.** Offload snapshot generation to a background thread but use database transactions to guarantee point-in-time consistency. Rejected because iterating a full database under an active write workload risks livelock (repeated transaction retries under optimistic concurrency control) in a high-throughput cluster.
- **Option C: Background snapshotting with Freeze-Apply (chosen).** Offload snapshot generation to a background thread. Before snapshotting, pause state machine application; resume after the snapshot bytes are captured. This guarantees a stable state without database-level locking.
- **Option D: No snapshotting.** Append indefinitely without compaction. Rejected because storage growth is unbounded and recovery time grows linearly with the log length — unacceptable for a clinical-grade system.

## Decision

We will implement background snapshot generation and asymmetric log compaction, utilizing thread-pool offloading. A background thread serializes the state machine while the main event loop pauses application but continues replicating new entries.

### 1. Unified State Serialization (Non-Streaming Prototype)

We will serialize the entire application state (inventory, session table, and clock) into a single contiguous byte buffer. Streaming (chunked) snapshot transfer is deferred to a later phase to reduce asynchronous complexity during initial validation of the compaction mechanism.

### 2. Background Snapshot Generation

When a node determines its log size exceeds a configurable threshold, the orchestration layer must offload snapshot generation to a background worker thread.

To guarantee **Point-in-Time Consistency** without database-level transactions, the orchestrator implements a **Freeze-Apply** mechanism:

- The main Raft event loop pauses the application of new log entries to the state machine. New entries continue to be appended to the physical log to preserve cluster liveness.
- The snapshot operation is executed in the background thread. Since the state machine application is paused, the state is logically frozen.
- Once the snapshot is complete, the orchestrator resumes application of the buffered log entries.

### 3. Asymmetric Compaction Safety

Nodes monitor their own logs and truncate them independently based on local configuration. The compaction metadata (last included index and last included term) is the authoritative source of truth for a compacted log's history. These two values must be updated atomically to prevent inconsistent logical horizons during crash recovery.

### 4. Atomic Restoration and The Halt Mandate

During snapshot installation, a follower must cleanly wipe its current state and restore it from the provided payload. To preserve the Stability Invariant (ADR 003), the heavy I/O of restoration must not block the Raft event loop.

To prevent partial installations from corrupting the node upon an unexpected crash, the state machine implements a **Restoration Tombstone** protocol:

1. Before clearing existing data, a restoration-in-progress marker is written to persistent storage and synchronously flushed.
2. Existing data is cleared and the new snapshot data is applied.
3. The restoration marker is removed, the new applied index is written, and a final synchronous flush is performed.
4. On startup, the state machine checks for the restoration marker. If found (indicating a crash during restoration), all data trees are wiped, ensuring the node starts completely empty and relies on the leader to re-send the snapshot.

If any error occurs during deserialization or physical disk write during restoration, the node must immediately transition to a terminal poisoned state and halt.

## Rationale

- **Asynchronous Execution:** Serializing the entire state is an O(N) CPU and I/O operation. Executing it synchronously within the main Raft event loop would violate the Stability Invariant (ADR 003). The main loop must only be blocked for the brief time required to update compaction metadata and truncate the log after the background worker returns the generated payload — avoiding Option A's failure mode.
- **Orchestrated Consistency (Freeze-Apply):** Database-level transactions for full-state iteration risk livelock under concurrent writes (Option B's failure mode). By pausing state machine application at the orchestration layer, we guarantee a stable snapshot without the overhead or risk of transaction retries.
- **Local Compaction Safety:** Log truncation is a local physical optimization. A cluster operates safely even if every node has truncated at a different index, provided the compaction metadata correctly bridges the gap between the state machine and the remaining log entries.
- **Strict Halt on Restoration Failure:** Snapshot installation is destructive. A partial installation leaves the node in an unrecoverable state that no longer matches cluster consensus. Halting satisfies ADR 009's Halt Mandate and prevents a "Zombie Node" scenario.

## Assumptions and Constraints

- The snapshot serialization format is a single contiguous blob; streaming is deferred. This means RAM usage during snapshot generation is proportional to the total state size, which must remain within the node's memory budget.
- Snapshot generation completes in bounded time; an unbounded freeze would stall the mutation pipeline.
- Nodes may compact their logs at different indices without coordination; the compaction metadata is the sole bridge between the log and the snapshot.

## Consequences

### Pros

- **Bounded Disk Growth:** The Raft log is bounded by the compaction threshold, ensuring predictable disk utilization.
- **Rapid Synchronization:** A node recovering from an extended partition receives a single snapshot instead of replaying thousands of historical log entries.

### Cons

- **Memory Overhead:** Because streaming is deferred, snapshot generation temporarily spikes RAM usage proportional to the total state size.
- **RPC Payload Limits:** Snapshot messages may exceed default gRPC message size limits; the transport layer must be configured to accept larger payloads.

### Operational Impact

- **Capacity Planning:** Operators must ensure that node memory can accommodate a full-state snapshot buffer plus the running workload.
- **Monitoring:** Compaction frequency and duration should be monitored to detect when snapshot generation becomes a bottleneck.

## Follow-Up

- Implement streaming (chunked) snapshot transfer in a later phase to reduce memory pressure during generation and installation.
- Establish benchmarks for snapshot generation duration across representative inventory sizes to validate the bounded-time assumption.
- Add compaction-frequency metrics to operational monitoring dashboards.

## References

- Raft paper (Ongaro & Ousterhout, 2014), §7 — defines the log compaction and snapshotting protocol.
- ADR 003 — Stability Invariant motivating background offloading of blocking I/O.
- ADR 009 — Tri-Layer Onion architecture governing where snapshotting responsibilities live, and the Halt Mandate for restoration failures.
