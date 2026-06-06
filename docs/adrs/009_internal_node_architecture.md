# ADR 009: Internal Node Architecture (The Onion Model)

## Metadata

- **Date:** 2026-04-25
- **Status:** Proposed
- **Scope:** Internal Raft Node Structure and Concurrency
- **Primary Goal:** Define the structural hierarchy of the Raft node to ensure strict isolation between protocol logic, concurrency management, and reactive signaling.
- **Last Updated:** 2026-06-06

## Context

Raft implementations frequently suffer from "God Object" syndrome, where protocol rules, state persistence, log I/O, and thread synchronization (mutexes/locks) are tightly coupled. This coupling complicates testing, obscures the "Halt Mandate" (ADR 001) implementation, and makes it difficult to reason about the logical epoch of the node.

Furthermore, mixing asynchronous concurrency models (`async/await`) with blocking physical storage operations (like `sled::flush`) introduces significant liveness hazards (e.g., executor thread starvation).

## Decision

We will implement a tri-layered "Onion" architecture for the internal Raft node, strictly separating the Physical, Logical, and Execution domains. A core mandate of this architecture is the **Synchronous Core**: Layers 1 and 2 MUST be completely devoid of asynchronous (`async/await`) constructs.

### 1. Layer 1: The Physical Foundation (Isolated Persistence)

- **Nature:** Pure Data Mutator.
- **Abstractions:** `RaftNode<R: NodeState>` utilizing the **Type-State Pattern** and `sled::Tree` for isolated storage.
- **Responsibility:** Raw state management (Log, Term, VotedFor, Commit Index, FSM application, Snapshot Metadata and Truncation).
- **Constraint:** This layer must be strictly synchronous (`fn`) and deterministic. It is the "Silent State Machine," containing only the logical state and transitions necessary for protocol correctness. It uses dedicated `sled` database handles to ensure component isolation.

### 2. Layer 2: The Logical Orchestrator (Safety Barrier)

- **Nature:** Protocol Dispatcher and Safety Barrier.
- **Abstractions:** `LogicalNode<S>` struct wrapping `RoleState` enum (Follower, Candidate, Leader, Poisoned) and owning the FSM handle.
- **Responsibility:** Mapping high-level RPC intents (AppendEntries, RequestVote) to Physical mutations, managing role transitions, and enforcing protocol invariants.
- **Constraint:** This layer must be strictly synchronous (`fn`). All decisions are evaluated deterministically without yielding to an async executor.
- **The Halt Mandate (Poison-then-Panic):** To mitigate the lack of lock poisoning in Tokio, any terminal failure or invariant violation MUST follow a strict sequence:
    1. **Detect** the violation (e.g., sequence gap in log, rival leader detection).
    2. **Transition** the `LogicalNode` state to `Poisoned` (utilizing `std::mem::replace`).
    3. **Panic** to halt the current thread.

### 3. Layer 3: The Execution Shell (Signaling Hub)

- **Nature:** Imperative Shell, Async/Sync Bridge, and Signaling Hub.
- **Abstractions:** `ConsensusShell` wrapping `Arc<RwLock<LogicalNode>>` and a `tokio::sync::watch` signaling channel.
- **Responsibility:** Providing thread-safe access, managing async coordination, bridging the `async` gRPC world to the `sync` core, offloading heavy synchronous operations (like Snapshotting) via `spawn_blocking`, and broadcasting state changes to reactive observers.
- **Atomic Invariant:** The **Lock-Signal Atomicity** rule. A signal containing the current `ConsensusProgress` MUST be broadcast after a mutation is complete but _before_ the write lock is released.

## Rationale

- **Decoupled Determinism:** Isolating protocol logic in synchronous Physical/Logical layers enables exhaustive unit testing without async executor overhead or mock runtimes.
- **Executor Safety:** By forcing the core to be synchronous, we prevent accidental "blocking-in-async" anti-patterns. The Execution Shell (Layer 3) is forced to explicitly offload heavy I/O using `spawn_blocking` when interfacing with the core.

## Consequences

### Pros

- **Robust Safety:** Poisoning is a first-class citizen in the type system, protecting against "Zombie Nodes."
- **High Testability:** Layers 1 and 2 can be verified with synchronous, deterministic unit tests.
- **Architectural Clarity:** Provides a clear mental model for where protocol logic ends and system orchestration begins.

### Cons

- **Implementation Rigor:** Requires manual adherence to the "Poison-then-Panic" sequence; forgetting to transition before panicking preserves the safety gap.
- **Boilerplate:** Requires delegation macros and multiple layers of wrapping.

### Operational Impact

- **Audit Requirement:** Any PR introducing a new panic or invariant check in the `LogicalNode` must be audited to ensure it follows the "Poison-then-Panic" mandate.
- **Testing:** Integration tests must verify that a poisoned node remains unusable across multiple lock acquisitions.
