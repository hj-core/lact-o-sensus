# Lact-O-Sensus: Implementation Roadmap

## 🗺 The "Skeleton-First" Strategy

This roadmap prioritizes establishing the **Logical Interface** and **Network Topology** before deep-diving into the complex Raft consensus logic. Each phase builds upon a verified foundation of architectural rigor.

---

## 🛠 Phase 1: The Contract (`common`) [DONE]

- **Goal:** Define the system-wide logical interface and shared data models.
- **Key Actions:**
  - Create `crates/common` library.
  - Define `lacto_sensus.proto` (Consensus, Ingress, and Policy services).
  - Implement the **12-Point Authorized Taxonomy** as a Rust Enum.
  - Set up `tonic-build` for automatic Rust code generation.
- **Success Metric:** All crates can import shared types without duplication.

## 🏗 Phase 2: The Infrastructure (`raft-node` Skeleton) [DONE]

- **Goal:** Establish the network mesh and basic RPC handlers.
- **Key Actions:**
  - Initialize `crates/raft-node` binary.
  - Implement gRPC server stubs for `RequestVote` and `AppendEntries`.
  - Establish the **Leader-Centric Hub-and-Spoke** topology (ADR 002).
  - Verify connectivity: 3 nodes can ping each other via RPC.
- **Success Metric:** A cluster of 3 nodes can start up and "listen" for traffic.

## ❤️ Phase 3: The Consensus Heart (`raft-node` Core) [DONE]

- **Goal:** Implement the Raft Leader Election and Heartbeat logic.
- **Key Actions:**
  - Implement the Raft State Machine (Follower, Candidate, Leader states).
  - Integrate **Randomized Election Timeouts** (ADR 003).
  - Implement Heartbeat logic to maintain leadership.
  - Verify: Killing the leader triggers a successful re-election.
- **Success Metric:** Stable leadership and failover in < 500ms.

## 🚪 Phase 4: Ingress & Mock Egress (`client-cli` & Mock `ai-veto`) [DONE]

- **Goal:** Connect external actors and verify the full request lifecycle using mock policy logic.
- **Key Actions:**
  - Implement the in-memory Log and `AppendEntries` replication logic in `raft-node`.
  - Initialize `crates/client-cli` as an interactive REPL with "Smart Client" logic.
  - Implement the `GrpcVetoRelay` in `raft-node` to bridge the Leader to the `ai-veto` node.
  - Verify: Client -> Leader -> Mock AI -> Consensus -> Commit (Round Trip).
- **Success Metric:** A client mutation is "Committed" after replication and a mock veto.

## 🏰 Phase 4.5: Fortress Hardening (Identity & Type Safety)

- **Goal:** Resolve "Legacy Debt" by aligning infrastructure with refined "Fortress" mandates.
- **Key Actions:**
  - **Identity Interceptors:** Migrate `cluster_id` and `target_node_id` validation from manual service guards to centralized gRPC middleware for all node types (ADR 004/005).
  - **NewType Migration:** Complete the transition for all domain identifiers (`LogIndex`, `SequenceId`, `Term`, `ClientId`) to prevent primitive obsession.
  - **Client-Side WAL:** Implement local persistence for pending `MutationIntent` and a recovery manager to ensure linearizable re-submission after client crashes (ADR 001).
  - **Resilient Client Loop:** Implement exponential backoff with jitter for retries and align default mutation timeouts with the 30s mandate to prevent thundering herds and premature timeouts (ADR 003).
  - **Identity Protocol Upgrade:** Update Protobuf and gRPC interceptors to enforce the `target_node_id` invariant, preventing logical misrouting and identity collisions (ADR 004/005).
- **Success Metric:** Cluster rejects misconfigured identity traffic via centralized middleware and client provides high-availability guarantees through durable WALs and stabilized retry backoff.

## 🧠 Phase 5: The Semantic Oracle (Resolution & Defense)

- **Goal:** Implement the 5-Layer Defensive Onion (ADR 007) and Semantic Resolution.
- **Key Actions:**
  - **Contract v2:** Update Protobuf and `CommittedMutation` to support resolved slugs, units, and display names (ADR 005).
  - **Unit Registry:** Implement the **Universal Unit Registry** (ADR 008) with the **Dual-Track Unit Model** (Base SI + Display Unit) and Banker's Rounding.
  - **The Onion:** Implement Layer 1-4 logic (Syntactic scrubbing, **Registry Firewall**, Dimensional Fence) in the `raft-node`.
  - **MutationLock:** Implement **Leader-Local MutationLock** to serialize AI evaluations across the 5s egress window (ADR 007).
  - **Robustness:** Implement **Leader-Internal Retries** for transient AI resolution failures.
- **Success Metric:** Messy user input (e.g., "OJ") is correctly resolved to canonical ledger facts (e.g., `orange_juice`).

## 🛡️ Phase 6: Precision & Persistence (SI & Session EOS)

- **Goal:** Implement SI Stabilization, Exactly-Once Semantics, and Persistent Storage.
- **Key Actions:**
  - **Internal SI:** Implement the **SI Stabilization Model** (ADR 008) using `rust_decimal` and mandatory **Banker's Rounding**.
  - **Session Table:** Implement the Session Table with **Atomic Side-Effect Updates** (ADR 006) and a **Deterministic Monotonic Clock** derived from log timestamps.
  - **The Halt Mandate:** Formalize immediate node panic on session table or state machine divergence during recovery or snapshot loading.
  - **Storage:** Integrate **`sled`** for persistent Raft logs and the State Machine with synchronous `fsync` (ADR 001).
  - **Chaos Testing:** Perform "Chaos Testing" to verify 100% recovery integrity and the **Halt Mandate** panic on state divergence.
- **Success Metric:** Idempotent recovery from the log using absolute SI results with zero arithmetic bias.

## 💾 Phase 7: The "Endless" Log (Log Compaction & Snapshotting)

- **Goal:** Implement state machine snapshots to manage log growth and accelerate recovery.
- **Key Actions:**
  - Implement State Machine serialization and `InstallSnapshot` RPC.
  - Implement log truncation logic once a snapshot is persisted.
  - Verify: A new node can catch up to the cluster using a snapshot.
- **Success Metric:** Successful log truncation and snapshot-based peer synchronization.

## 🛡️ Phase 8: Pre-Vote Integrity (Election Safety)

- **Goal:** Implement the Pre-Vote phase to prevent disruptive elections from partitioned nodes.
- **Key Actions:**
  - **Pre-Vote RPC:** Introduce a dry-run election phase where candidates check for a quorum before incrementing their term.
  - **Non-Disruptive Shield:** Update Follower logic to grant pre-votes without resetting their own election timers.
  - **Partition Verification:** Verify that a re-connecting node with a lower term does not force the cluster into a disruptive re-election.
- **Success Metric:** Cluster term remains stable during asymmetrical network partitions.

## 🚀 Phase 9: Future Horizons (Advanced Reliability & Scaling)

- **Proposed Future Works:**
  - **Elastic Consensus:** Safe dynamic membership changes (Joint Consensus) to add/remove nodes at runtime.
  - **Security & Access Control:** mutual TLS and RBAC for cluster access.
  - **Multi-Tenancy:** Utilizing `cluster_id` for isolated groups on shared infrastructure.
  - **Causal History:** Immutable audit trail and "point-in-time" recovery.
