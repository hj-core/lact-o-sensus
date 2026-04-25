# ADR 009: Internal Node Architecture (The Onion Model)

## Metadata

- **Date:** 2026-04-25
- **Status:** Proposed
- **Scope:** Internal Raft Node Structure and Concurrency
- **Primary Goal:** Define the structural hierarchy of the Raft node to ensure strict isolation between protocol logic, concurrency management, and reactive signaling.
- **Last Updated:** 2026-04-25

## Context

Raft implementations frequently suffer from "God Object" syndrome, where protocol rules, state persistence, log I/O, and thread synchronization (mutexes/locks) are tightly coupled. This coupling complicates testing, obscures the "Halt Mandate" (ADR 001) implementation, and makes it difficult to reason about the logical epoch of the node.

Furthermore, the choice of `tokio::sync::RwLock` for concurrency introduces a safety risk: unlike `std::sync::RwLock`, the Tokio implementation does not poison itself on panic. If a task panics while holding a write lock, the lock is released, and the potentially corrupted state remains accessible to other tasks.

## Decision

We will implement a tri-layered "Onion" architecture for the internal Raft node, strictly separating the Physical, Logical, and Execution domains.

### 1. Layer 1: The Physical Foundation

- **Nature:** Pure Data Mutator.
- **Abstractions:** `RaftNode<S: NodeState>` utilizing the **Type-State Pattern**.
- **Responsibility:** Raw state management (Log, Term, VotedFor, Commit Index, FSM application).
- **Constraint:** This layer must be synchronous and deterministic. It has no knowledge of networking, I/O, or thread synchronization. It is the "Silent State Machine."

### 2. Layer 2: The Logical Orchestrator

- **Nature:** Protocol Dispatcher and Safety Barrier.
- **Abstractions:** `LogicalNode` enum (Follower, Candidate, Leader, Poisoned).
- **Responsibility:** Mapping high-level RPC intents (AppendEntries, RequestVote) to Physical mutations, managing role transitions, and enforcing protocol invariants.
- **The Halt Mandate (Poison-then-Panic):** To mitigate the lack of lock poisoning in Tokio, any terminal failure or invariant violation MUST follow a strict sequence:
    1. **Detect** the violation (e.g., rival leader detection).
    2. **Transition** the `LogicalNode` state to `Poisoned`.
    3. **Panic** to halt the current task.
  This ensures that once a node is compromised, all subsequent attempts to acquire the lock and access the node (via the `delegate_to_inner!` macro) will trigger an immediate panic.

### 3. Layer 3: The Execution Shell

- **Nature:** Imperative Shell and Signaling Hub.
- **Abstractions:** `ConsensusShell` wrapping `Arc<RwLock<LogicalNode>>` and a `tokio::sync::watch` signaling channel.
- **Responsibility:** Providing thread-safe access, managing async coordination, and broadcasting state changes to reactive observers.
- **Atomic Invariant:** The **Lock-Signal Atomicity** rule. A signal containing the current `ConsensusProgress` MUST be broadcast after a mutation is complete but *before* the write lock is released.

## Rationale

- **Mitigation of Non-Poisoning Locks:** Explicitly poisoning the `LogicalNode` variant provides a manual safety mechanism that the underlying lock primitive lacks.
- **Decoupled Determinism:** Isolating protocol logic in the Physical layer enables exhaustive unit testing without async overhead.
- **Reactive Consistency:** The `signal_counter` (Logical Epoch) in the Execution shell ensures that external components are notified of every state change, preventing "lost updates."

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
