# ADR 001: Node Failure Model and Reliability Guarantees

## Metadata

- **Date:** 2026-04-09
- **Status:** Accepted
- **Scope:** Raft cluster nodes, client nodes, and the AI Veto node; excludes Gateway, FSM, and network transport layers.
- **Primary Goal:** Ensure system integrity across crashes and provide linearizable semantics to clients.
- **Last Updated:** 2026-06-09

## Context

Lact-O-Sensus is a distributed system managing a replicated state machine (grocery inventory). It comprises three node types in distinct failure domains:

- **Raft Cluster Nodes** — Run the Raft consensus protocol to replicate state across a quorum-based group. These nodes must tolerate crashes and restarts while preserving persisted state.
- **Client Nodes** — Submit mutation intents to the cluster. They must survive local crashes without losing in-flight requests to maintain linearizable semantics.
- **AI Veto Node** — An external LLM-based oracle that semantically evaluates mutation intents. It is non-deterministic by nature and cannot be held to the same reliability guarantees as the consensus core.

Distributed systems must choose between two fundamental failure models: **Crash-Recovery (CR)** — nodes fail only by stopping and may recover with persistent state intact; and **Byzantine-Fault Tolerance (BFT)** — nodes may behave arbitrarily, including maliciously. Raft (the project's consensus protocol) is mathematically proven under the CR model, but does not protect against Byzantine peers. To ensure data integrity and system liveness, we must define which failure model applies to each node type and how the system guards against violations of those assumptions.

## Options Considered

### Option A: Full BFT for All Nodes

Adopt a Byzantine-Fault-Tolerant consensus protocol (e.g., PBFT, HotStuff) for the entire cluster, including clients and the AI node.

- **Safety**: Highest — protects against arbitrary node misbehavior.
- **Performance**: Significantly lower throughput and higher latency due to multi-round message exchanges (O(n²) communication).
- **Complexity**: Requires a fundamentally different consensus engine; no existing Raft codebase can be reused.
- **Cost**: Implementation effort far exceeds the 3-month project scope.

### Option B: Crash-Recovery Baseline (Chosen)

Adopt CR for Raft cluster nodes and clients; treat the AI Veto as Byzantine. This is the layered model described in this ADR.

- **Safety**: Sufficient — Raft's mathematical guarantees cover the core cluster; the AI node's non-determinism is isolated by the Fortress validation layer.
- **Performance**: Standard Raft performance (leader-driven, O(n) replication); no BFT overhead.
- **Complexity**: Moderate — requires client-side WAL, but reuses off-the-shelf Raft implementations.
- **Cost**: Within project scope; achievable in 3 months.

### Option C: Best-Effort Clients

Keep cluster nodes as CR but do not require a client-side WAL or deduplication; accept at-most-once semantics.

- **Safety**: Weak — no linearizability guarantee; duplicate or lost intents on client crash.
- **Performance**: Highest — no synchronous disk I/O for client persistence.
- **Complexity**: Lowest — simplest to implement.
- **Cost**: Trivial — but fails the clinical requirement for exactly-once semantics.

### Option D: Do Nothing

Continue without an explicit failure model; handle errors reactively.

- **Safety**: None — no systematic recovery or deduplication.
- **Performance**: Baseline — no added overhead.
- **Complexity**: None — but guarantees are undefined.
- **Cost**: Zero upfront, but unacceptable operational risk for a clinical-grade system.

## Decision

We will adopt a layered failure model, transitioning from a "Honest Crash-Recovery" baseline to a "Fortress" model that handles external Byzantine behavior.

### Assumptions & Constraints

- **Raft correctness**: The Raft protocol is implemented exactly as specified in the paper (Ongaro & Ousterhout, 2014) — any deviation invalidates the CR safety guarantees.
- **Cluster size**: A cluster must contain at least 3 nodes (quorum of 2) and is expected to operate with ≤ 7 nodes for the foreseeable future.
- **Persistent storage**: All Raft cluster nodes and client nodes have access to reliable, crash-safe persistent storage (e.g., local SSD, not NFS).
- **LLM non-determinism**: AI Veto responses are non-deterministic and non-reproducible; the system must never depend on them for cluster-wide consistency.
- **Timing independence**: Safety must not depend on timing assumptions — the CR model holds regardless of message delays or clock skew (per Raft's safety proof).
- **Scope boundary**: The network transport layer is assumed to be reliable FIFO (TCP); message corruption or reordering is not handled at the consensus layer.

### 1. Raft Cluster Nodes: Crash-Recovery (CR)

- **Model:** Non-Byzantine, Crash-Recovery.
- **Assumption:** Nodes follow the Raft protocol but may stop and restart with persistent state intact.
- **Mandate:**
  - Persist critical state (`currentTerm`, `votedFor`, `log[]`) to stable storage before responding to RPCs.
  - Load the latest snapshot (if any) and replay the remaining persistent log on recovery to reconstruct the State Machine.
  - On snapshot installation, follow the Restoration Tombstone protocol (ADR 011) to ensure crash-safe state restoration.

### 2. Client Nodes: Stateful Recovery & Linearizability

- **Model:** Crash-Recovery. (Note: Evolution to Byzantine-Robust is deferred to Phase 9 — see [Roadmap](../roadmap.md).)
- **Mandate:**
  - Provide `client_id` and monotonic `sequence_id` for deduplication.
  - Persist pending requests locally in a client-side WAL to handle client crashes.
  - Recover and re-propose unacknowledged intents on startup for linearizable retries.

### 3. AI Veto Node: Byzantine Oracle

- **Model:** Byzantine-Faulty (Non-Deterministic).
- **Mandate:**
  - Treat as an "Unreliable Oracle" due to LLM non-determinism.
  - Only the Raft Leader interacts with the AI Node to ensure cluster-wide determinism.

### 4. Input Validation (The "Fortress" State Machine)

- **Mandate:** The Raft Leader performs strict schema and "moral" validation on all inputs before proposing them to the cluster.

## Rationale

- **Raft Compatibility:** The Raft protocol is mathematically proven for the Crash-Recovery model (Ongaro & Ousterhout, "In Search of an Understandable Consensus Algorithm", USENIX ATC 2014). Straying into BFT for the core consensus would exceed the 3-month project scope.
- **LLM Reality:** LLMs are inherently non-deterministic. Treating the AI Node as Byzantine protects the deterministic nature of the state machine.
- **End-to-End Reliability:** Standard "Best Effort" clients cannot guarantee exactly-once semantics if the client itself crashes. Local persistence is required for true linearizability.

### Implementation Notes

- **Exactly-Once Semantics (ADR 006):** Deduplication is enforced at two levels for defense-in-depth. The Gateway's Sequence Firewall (pre-consensus) returns cached responses for duplicate sequences, avoiding unnecessary consensus rounds. The FSM's Session Table (post-consensus) catches any duplicates that bypass the firewall, advancing only `last_applied` without mutating state.
- **Client WAL Backend:** The client WAL uses a transactional, crash-safe key-value store for pending intents, providing atomic read-and-delete semantics during intent replay.
- **Asynchronous Snapshotting (ADR 011):** Snapshot generation and installation are offloaded to background tasks to preserve heartbeat stability, with a Restoration Tombstone marker ensuring crash-safe recovery.

## Consequences

### Pros

- **High Academic Rigor:** Adheres strictly to the Raft paper's safety requirements.
- **Robustness:** System survives standard network partitions and hardware reboots without data loss.
- **Modular Security:** By treating external nodes as Byzantine, we create a secure perimeter around the consensus group.

### Cons

- **Performance Latency:** Synchronous disk I/O ("Sync-before-ACK") will significantly slow down throughput.
- **Implementation Complexity:** Requires building a robust local WAL for the client and a recovery manager for the cluster nodes.
- **Internal Vulnerability:** The system remains vulnerable to "traitor" cluster nodes (malicious Raft participants).

### Operational Impact

- **Storage:** Requires reliable filesystem access for all node types.
- **Testing:** Necessitates "Chaos Testing" to verify recovery logic.

## Follow-Up

- **Phase 4 — Client WAL implementation:** Build and integrate the client-side write-ahead log for linearizable retries, tracked in the client-cli crate.
- **Phase 9 — Byzantine-robust client model:** Revisit the client failure model if clinical requirements demand protection against malicious clients. See [Roadmap](../roadmap.md).
- **Chaos testing validation:** Re-evaluate the CR assumption for cluster nodes if testing reveals Byzantine-like behavior (e.g., state corruption, split-brain).
- **Cluster size constraint:** If the cluster grows beyond 7 nodes, revisit the failure model — larger clusters may require leader lease extensions or BFT hybrid approaches.
