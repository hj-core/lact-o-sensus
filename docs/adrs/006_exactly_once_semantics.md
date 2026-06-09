# ADR 006: Exactly-Once Semantics (EOS) and Session Management

## Metadata

- **Date:** 2026-04-09
- **Status:** Accepted
- **Scope:** State machine session table for deduplication, sequence gap detection, no-purge policy, temporal determinism, and halt-on-inconsistency. Excludes Gateway-level error formatting (separate concern per Clean Architecture), client-side WAL implementation (defined in `client-cli`), and snapshot serialization details (defined in ADR 011).
- **Primary Goal:** Ensure every mutation is executed exactly once, regardless of network retries or leader elections.
- **Last Updated:** 2026-06-09

## Context

In a distributed system, network instability and node failures often lead to request retries (at-most-once or at-least-once delivery). Without a deduplication mechanism, at-least-once delivery can cause data corruption — e.g., adding an item twice for the same user intent. The Raft consensus protocol (Ongaro & Ousterhout, USENIX ATC 2014) guarantees total order of log entries, but it does not inherently distinguish between a new request and a replay of a previously committed one. A client that retries a mutation after a timeout may find that the first attempt was already committed, resulting in duplicate state changes.

To provide exactly-once semantics, the system must recognize retried requests and return the original result without re-executing the business logic. This requires:
- A unique identity for each client session (`client_id`, per ADR 004) and a monotonic `sequence_id` for each request within that session.
- A replicated session table within the state machine that records every processed `(client_id, sequence_id)` pair and its outcome.
- The guarantee that all outcomes — including vetoes from the AI Veto (ADR 007) — are recorded as consensus events, so that crash-recovery (ADR 001) reconstructs the exact session state.

Per ADR 005, the session table records the logical outcome (status, state version, error message) rather than the raw RPC frame, and is serialized into state machine snapshots (ADR 011) to persist across log compaction boundaries.

## Options Considered

### Option A: Client-Side Idempotency Tokens Only

Clients generate and retry with a unique idempotency key; the Gateway caches responses for a bounded time window.
- **Complexity**: Lowest — no state machine changes; caching is simple.
- **Safety**: Weak — cache expiry or leader failover can lose the dedup state, causing duplicate mutations.
- **Storage**: Minimal — cache is ephemeral.
- **Verdict**: Rejected — cannot guarantee exactly-once across leader failovers; crashes clear the cache.

### Option B: Replicated Session Table in State Machine (Chosen)

A session table within the state machine records every processed `(client_id, sequence_id)` pair and its outcome. Deduplication is a side-effect of log application (per ADR 005).
- **Safety**: Strong — session state is replicated via Raft and survives leader failovers; crash-recovery reconstructs the session table from the log (per ADR 001).
- **Complexity**: Moderate — the state machine must implement dedup logic, but no additional Raft commands are needed for session management.
- **Storage**: Permanent — session records are never purged; approximately 1KB per client including audit metadata.
- **Verdict**: Chosen — provides strong linearizability guarantees consistent with ADR 001's crash-recovery model.

### Option C: Distributed Lock for Sequences

Use an external coordination service (e.g., ZooKeeper, etcd) to track the highest committed sequence per client.
- **Safety**: Moderate — introduces a new failure domain; the lock service becomes a single point of failure.
- **Complexity**: Highest — requires deploying and operating an additional coordination service.
- **Storage**: Minimal — external service manages state.
- **Verdict**: Rejected — unnecessary operational complexity; the Raft cluster already provides consensus.

### Option D: Do Nothing

Accept at-least-once semantics; rely on clients to handle duplicates via application-level checks.
- **Safety**: Weak — no systematic deduplication; duplicate mutations are possible.
- **Complexity**: Lowest — no changes needed.
- **Storage**: None.
- **Verdict**: Rejected — violates the clinical requirement for exactly-once semantics.

## Decision

We will implement a **Stateful Session Table** as an integral, replicated component of the State Machine.

### Assumptions & Constraints

- **Crash-Recovery model** (per ADR 001): Nodes may stop and restart; the session table must be reconstructed deterministically from the Raft log on recovery.
- **Client identity** (per ADR 004): Every client has a unique `client_id` (NewType) and maintains a local monotonic `sequence_id` across restarts via a local WAL.
- **Storage cost**: Each session record is approximately 1KB including audit metadata (per ADR 005). Storage is bounded by the number of unique clients, not by request volume.
- **No-purge**: Session records MUST NOT be purged — the "Double-Bootstrap" hazard (where a network replay is mistaken for a new session after deletion) violates linearizability.
- **Gap-free monotonicity**: Sequence IDs within a client session MUST be contiguous and monotonically increasing; gaps indicate client-side failures or ordering violations.

### 1. Client Session Record

The state machine maintains a registry of active client sessions. Each session record captures the most recently processed mutation outcome per client. It records:

- The client's unique identity and the last processed sequence ID.
- The logical outcome of that sequence (status, state version, error message — per ADR 005), enabling replay without re-executing business logic or re-querying the AI Veto.
- A deterministic timestamp derived from the consensus log (not the client's wall clock).

### 2. Deterministic Deduplication

When applying a log entry, the state machine evaluates the incoming `(client_id, sequence_id)` against the session table. The decision tree has four cases:

- **New client**: The `client_id` has not been seen before. Initialize a session record and process the mutation.
- **Replay** (`seq_id` matches the last seen sequence): Return the cached outcome without re-applying the mutation or re-querying the AI Veto.
- **Next sequence** (`seq_id` is exactly one greater than the last seen): Process the mutation. Record the outcome (Approved or Vetoed) in the session table and advance `last_sequence_id`. The inventory is modified only on Approval; Vetoes still consume the sequence ID to maintain a contiguous, gap-free ledger.
- **Gap detected** (`seq_id` skips one or more IDs): Reject. A gap indicates a client-side failure or ordering violation. Clients must always progress monotonically and must not reuse IDs after a rejection.

This logic ensures that every outcome — including Vetoes — occupies a deterministic position in the sequence. If a Leader crashes after vetoing a mutation but before notifying the client, the new Leader will replay the same Veto from the Raft log during recovery, preventing the client from retrying a sequence that was already consumed.

### 3. Permanent Session Metadata (No-Purge Policy)

To eliminate the "Double-Bootstrap" hazard (where a network replay is mistaken for a new session after a purge), the Session Table is considered **Permanent Metadata**. Client session records MUST NOT be purged from the state machine. This metadata is securely preserved across log compaction boundaries by serializing the entire Session Table into State Machine snapshots (ADR 011). Storage cost (~1KB per client) is negligible for the expected scale and is prioritized over storage reclamation.

### 4. Stateful Temporal Determinism

The State Machine MUST maintain a persistent logical clock derived from the consensus log. For every applied entry, the effective time is advanced monotonically using the entry's timestamp — never the local wall clock. This ensures all nodes share a deterministic temporal context regardless of clock skew.

### 5. The Halt Mandate

Any detection of session table inconsistency (e.g., during snapshot loading or hash verification) or sequence gap violation during log replay MUST trigger an immediate node panic to prevent linearizability violations. This follows the **Poison-then-Panic** sequence defined in ADR 009.

## Rationale

- **Linearizability** (per ADR 001, ADR 005): Deduplication at the state machine level ensures the system provides a single-system image to the user, even across leader failovers. Because the session table is part of the replicated state, every node — including the new leader after a crash — sees the exact same session state. This is stricter than client-side idempotency (Option A), which cannot survive leader failovers without an external cache.
- **Implicit Replication**: By making the session update a side-effect of applying a log entry, we avoid additional Raft commands for session management. Each log entry atomically updates both the application state and the session table — this is the same "Implicit Observation" pattern described in the Raft paper's discussion of linearizability (§5, Client Protocol).
- **Crash-Consistency** (per ADR 001): Because the session table is reconstructed by replaying the Raft log, a crashed node will always arrive at the correct session state upon recovery. No external coordination or state transfer is required beyond the existing Raft log replication and snapshot mechanism (ADR 011).
- **No-Purge Policy**: Purging session records creates a "Double-Bootstrap" hazard — a replayed network retry after a purge would be treated as a new session, allowing duplicate mutations. The storage cost (~1KB per client) is bounded by the number of unique clients, not by request volume, making permanent retention feasible for the expected cluster scale (≤7 nodes, tens of thousands of clients).

## Consequences

### Pros

- **Data Integrity**: Guarantees that "Double Writes" (e.g., double-adding milk) are impossible.
- **User Trust**: Clients can safely retry failed RPCs without fear of side-effects.
- **Architectural Cleanliness**: The State Machine remains a "Black Box" that handles its own internal reliability metadata.

### Cons

- **Storage Overhead**: Storing a complete response cache (including AI justifications) for every client consumes persistent storage and memory. While negligible for thousands of clients, it may scale significantly if the system services millions of unique IDs over its lifetime.
- **Complexity**: The state machine application logic becomes more sophisticated, requiring a "Deduplication Layer" before the "Business Logic Layer."

### Operational Impact

- **Client Requirements**: Clients **must** maintain their `client_id` and `sequence_id` across restarts to benefit from EOS.
- **Snapshot Size**: Including the session table will increase the size of state machine snapshots linearly with the number of unique clients, potentially impacting snapshot transmission times and recovery MTTR in high-scale scenarios.

## Follow-Up

- **Client WAL integration:** Ensure `client-cli` maintains and recovers `client_id` and `sequence_id` across restarts (per ADR 001's client-side WAL requirement).
- **Session table monitoring:** Add a metric for session table size (unique clients) and deduplication hit rate to clinical telemetry (per ADR 010).
- **Snapshot impact tracking:** Measure snapshot size growth as clients accumulate; establish a threshold for revisiting the no-purge policy (e.g., re-evaluate if session table exceeds 50% of total snapshot size).
- **Gateway opaque errors (cross-cutting):** The Ingress Firewall MUST expose opaque error messages to prevent session probing — document this requirement in the Gateway ADR or implementation spec, as it is outside this ADR's state machine scope.
