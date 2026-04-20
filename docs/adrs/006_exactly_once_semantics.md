# ADR 006: Exactly-Once Semantics (EOS) and Session Management

## Metadata

- **Date:** 2026-04-09
- **Status:** Proposed
- **Scope:** State Machine Reliability and Linearizability
- **Primary Goal:** Ensure every mutation is executed exactly once, regardless of network retries or leader elections.
- **Last Updated:** 2026-04-20

## Context

In a distributed system, network instability and node failures often lead to request retries. Without a deduplication mechanism, "At-Least-Once" delivery can cause data corruption (e.g., adding an item twice). To provide a consistent "Exactly-Once" experience, the system must recognize retried requests and return the original result without re-executing the business logic.

## Decision

We will implement a **Stateful Session Table** as an integral, replicated component of the State Machine.

### 1. The Client Session Record

The state machine will maintain a registry of active client sessions. To optimize storage, the `cached_response` must store the **logical outcome** (as defined in ADR 005) rather than the raw RPC frame. Each record will contain:

- **`client_id`**: The unique logical identifier (NewType) of the client.
- **`last_sequence_id`**: The monotonic sequence number (**NewType `SequenceId`**) of the most recently processed sequence ID.
- **`cached_response`**: The logical outcome (Status, State Version, Error Message) of the most recently processed sequence ID.
- **`last_activity_effective_time`**: The state-machine-derived time of the last activity.

### 2. Deterministic Deduplication Logic

Upon applying a command from the Raft log, the state machine must execute the following logic:

- **Unknown `client_id`**: **Initialize**. Treat the first appearance of a `client_id` as a registration event; create a session record and proceed to **Process**.
- **`seq_id < last_seen_seq`**: **Discard**. The request is an out-of-order or ancient retry.
- **`seq_id == last_seen_seq`**: **Replay**. Return the `cached_response` without re-applying any mutation or re-querying the AI Node.
- **`seq_id == last_seen_seq + 1`**: **Process**. Execute the mutation, update the grocery list, and overwrite the `cached_response` and `last_sequence_id` with the outcome.
- **`seq_id > last_seen_seq + 1`**: **Reject**. A "gap" in sequences indicates a client-side failure or an ordering violation.

### 3. Atomic Side-Effect Updates

The Session Table is not updated via separate Raft commands. Instead, it is updated as a **deterministic side-effect** whenever a mutation is applied to the State Machine. This ensures that the grocery list and the session table are always in perfect sync across all nodes.

### 4. Deterministic Session Expiration (Monotonic TTL)

To prevent the Session Table from growing indefinitely, inactive sessions will be purged. To ensure absolute cluster-wide agreement across leader elections and clock drift:

- **Effective Time:** The State Machine maintains a `last_effective_time`. For every log entry applied, `effective_time = max(entry.timestamp, last_effective_time)`.
- **Monotonic Purge:** Sessions are purged only when their `last_activity_effective_time` plus the TTL duration (e.g., 30 days) is less than the current `effective_time`.
- **Atomic Snapshotting:** Any implementation of Log Compaction **must** include the complete Session Table.
- **The Halt Mandate:** Any detection of session table inconsistency (e.g., during snapshot loading or hash verification) MUST trigger an immediate node panic to prevent linearizability violations.

## Rationale

- **Linearizability**: Deduplication at the state machine level ensures that the system provides a "Single System Image" to the user, even across leader failovers.
- **Implicit Replication**: By making the session update a side-effect of applying a log entry, we avoid the overhead of additional Raft commands for session management.
- **Crash-Consistency**: Because the session table is reconstructed by replaying the Raft log, a crashed node will always arrive at the correct session state upon recovery.

## Consequences

### Pros

- **Data Integrity**: Guarantees that "Double Writes" (e.g., double-adding milk) are impossible.
- **User Trust**: Clients can safely retry failed RPCs without fear of side-effects.
- **Architectural Cleanliness**: The State Machine remains a "Black Box" that handles its own internal reliability metadata.

### Cons

- **Storage Overhead**: Storing a response cache for every client consumes persistent storage and memory.
- **Complexity**: The state machine application logic becomes more sophisticated, requiring a "Deduplication Layer" before the "Business Logic Layer."

### Operational Impact

- **Client Requirements**: Clients **must** maintain their `client_id` and `sequence_id` across restarts to benefit from EOS.
- **Snapshot Size**: Including the session table will increase the size of state machine snapshots.
