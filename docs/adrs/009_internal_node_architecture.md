# ADR 009: Internal Node Architecture (The Onion Model)

## Metadata

- **Date:** 2026-04-25
- **Status:** Accepted
- **Scope:** Internal Raft node structure and concurrency; excludes the gRPC transport layer, client-side components, and the AI Oracle relay.
- **Primary Goal:** Define the structural hierarchy of the Raft node to ensure strict isolation between protocol logic, concurrency management, and reactive signaling.
- **Last Updated:** 2026-06-09

## Context

Raft implementations frequently suffer from "God Object" syndrome, where protocol rules, state persistence, log I/O, and thread synchronization are tightly coupled. This coupling has three concrete consequences:

1. **Testing fragility:** Protocol logic cannot be unit-tested without spinning up async runtimes and mock storage backends.
2. **Halt Mandate opacity (ADR 001):** When a node must halt due to an invariant violation, the coupling makes it unclear which layer should detect, signal, and propagate the failure.
3. **Liveness hazards (ADR 003):** Mixing `async/await` with blocking storage operations (e.g., synchronous flush) inside the Raft tick loop can starve the async executor, causing missed heartbeats and spurious leader elections.

These forces demand a clean separation between deterministic protocol logic, persistent state mutation, and asynchronous coordination.

## Options Considered

- **Option A: Monolithic single-layer node.** All protocol logic, persistence, and async coordination live in one struct. Rejected because it reproduces the God Object problem, making the Halt Mandate impossible to implement cleanly and preventing unit testing of protocol rules without an async runtime.
- **Option B: Two-layer separation (sync core + async shell).** Split protocol logic from async coordination, but keep persistence embedded in the protocol layer. Rejected because the physical storage concerns (flush, compaction, truncation) remain coupled with logical protocol decisions, complicating the snapshotting pipeline required by ADR 011.
- **Option C: Tri-layered Onion (chosen).** Three strictly separated layers — Physical (persistence), Logical (protocol), Execution (async coordination) — with a Synchronous Core mandate.
- **Option D: Full actor model.** Every subsystem (log, protocol, snapshotting) is an independent actor communicating via message passing. Rejected because the overhead and complexity exceed the needs of a ≤7-node cluster.

## Decision

We will implement a tri-layered "Onion" architecture for the internal Raft node, strictly separating the Physical, Logical, and Execution domains. A core mandate of this architecture is the **Synchronous Core**: Layers 1 and 2 must be completely devoid of asynchronous (`async/await`) constructs.

### 1. Layer 1: The Physical Foundation (Isolated Persistence)

- **Nature:** Pure data mutator.
- **Responsibility:** Raw state management — log entries, term, voted-for, commit index, state machine application, snapshot metadata and truncation.
- **Constraint:** This layer must be strictly synchronous and deterministic. It contains only the logical state and transitions necessary for protocol correctness. All storage interactions are mediated through a storage abstraction that enforces component isolation.

### 2. Layer 2: The Logical Orchestrator (Safety Barrier)

- **Nature:** Protocol dispatcher and safety barrier.
- **Responsibility:** Mapping high-level RPC intents (AppendEntries, RequestVote, PreVote) to physical mutations, managing role transitions, and enforcing protocol invariants.
- **Constraint:** This layer must be strictly synchronous. All decisions are evaluated deterministically without yielding to an async executor.
- **The Halt Mandate (Poison-then-Panic):** Any terminal failure or invariant violation must follow a strict sequence:
  1. **Detect** the violation (e.g., sequence gap in log, rival leader detection).
  2. **Transition** the logical node state to a terminal poisoned state.
  3. **Panic** to halt the current thread.
- **Transition Safety:** Before executing any state-transition closure, the orchestrator must first set the node to a poisoned sentinel state. If the closure panics, the node remains permanently poisoned, preventing use of an inconsistent state.

### 3. Layer 3: The Execution Shell (Signaling Hub)

- **Nature:** Imperative shell, async/sync bridge, and signaling hub.
- **Responsibility:** Providing thread-safe access to the synchronous core, managing async coordination, bridging the async gRPC world to the synchronous core, offloading heavy synchronous operations (e.g., snapshotting) to background threads, and broadcasting state changes to reactive observers.
- **Lock Choice:** A non-poisoning read-write lock is used because the Halt Mandate implements its own poisoning via the logical node's poisoned state (Layer 2); standard lock poisoning would interfere with the ability to broadcast a terminal signal after a panic.
- **Atomic Invariant — Lock-Signal Atomicity:** A signal containing the current consensus progress must be broadcast after a state mutation is complete but before the write lock is released. This guarantees that observers never see stale state. The guard that releases the write lock must also handle the panicking edge case by automatically poisoning the node and broadcasting a terminal signal.
- **FSM Freeze/Thaw (ADR 011):** The shell maintains a freeze depth counter and an asynchronous lock for the state machine. During snapshot installation, the state machine application loop is frozen (depth incremented) and later thawed (depth decremented). While frozen, committed entries are buffered but not applied. Overflow or underflow of the depth counter triggers the Halt Mandate.
- **In-Flight Snapshot Tracking:** The shell prevents concurrent snapshot replications to the same peer by maintaining a set of peers currently receiving a snapshot. Acquisition of a slot returns a guard that removes the peer from the set when dropped.

## Rationale

- **Decoupled Determinism (testing):** Isolating protocol logic in synchronous Physical and Logical layers enables exhaustive unit testing without async executor overhead or mock runtimes. Under Options A and B, testing a role transition requires mocking both storage and networking.
- **Executor Safety (liveness):** By forcing the core to be synchronous, we prevent accidental "blocking-in-async" anti-patterns. The Execution Shell (Layer 3) is forced to explicitly offload heavy I/O to background threads when interfacing with the core — satisfying ADR 003's Stability Invariant.
- **Halt Mandate clarity:** The Poison-then-Panic sequence has a well-defined owner (Layer 2) and a well-defined propagation path (Layer 3), eliminating the ambiguity that plagued the monolithic design.

## Assumptions and Constraints

- The cluster runs on commodity hardware with a single filesystem; a distributed filesystem would introduce new durability constraints not addressed by this architecture.
- Layer 1 and Layer 2 run on the same thread as the Raft tick loop; offloading them to separate threads would require revisiting the Synchronous Core mandate.
- The FSM Freeze/Thaw mechanism assumes snapshot generation completes in bounded time; an unbounded freeze would stall the mutation pipeline.

## Consequences

### Pros

- **Robust Safety:** Poisoning is a first-class concept in the hierarchy, protecting against "Zombie Nodes" that continue operating after an invariant violation.
- **High Testability:** Layers 1 and 2 can be verified with synchronous, deterministic unit tests without async infrastructure.
- **Architectural Clarity:** Provides a clear mental model for where protocol logic ends and system orchestration begins.

### Cons

- **Implementation Rigor:** Requires manual adherence to the Poison-then-Panic sequence; forgetting to transition before panicking preserves the safety gap.
- **Boilerplate:** Requires delegation layers between the Execution Shell and the synchronous core.

### Operational Impact

- **Audit Requirement:** Any change introducing a new panic or invariant check in the Logical Orchestrator must be audited to ensure it follows the Poison-then-Panic mandate.
- **Testing:** Integration tests must verify that a poisoned node remains unusable across multiple lock acquisitions. Regression tests should cover the freeze-depth overflow/underflow paths.

## Follow-Up

- Audit existing Logical Orchestrator code for compliance with the Poison-then-Panic sequence before the clinical release.
- Implement integration tests that verify node behavior after a simulated panic (poisoned state persists, terminal signal is broadcast).
- Revisit the Synchronous Core mandate if profiling reveals that the Raft tick loop is CPU-bound due to state machine application.

## References

- Raft paper (Ongaro & Ousterhout, 2014) — defines the protocol invariants enforced by Layer 2.
- ADR 001 — Halt Mandate requiring the Poison-then-Panic mechanism.
- ADR 003 — Stability Invariant motivating the Synchronous Core and background offloading.
- ADR 011 — Asynchronous snapshotting requiring the FSM Freeze/Thaw mechanism.
