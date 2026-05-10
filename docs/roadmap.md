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

## 🏗 Phase 2: The Infrastructure (`raft-engine` Skeleton) [DONE]

- **Goal:** Establish the network mesh and basic RPC handlers.
- **Key Actions:**
  - Initialize `crates/raft-engine` library (originally `crates/raft-node` binary).
  - Implement gRPC server stubs for `RequestVote` and `AppendEntries`.
  - Establish the **Leader-Centric Hub-and-Spoke** topology (ADR 002).
  - Verify connectivity: 3 nodes can ping each other via RPC.
- **Success Metric:** A cluster of 3 nodes can start up and "listen" for traffic.

## ❤️ Phase 3: The Consensus Heart (`raft-engine` Core) [DONE]

- **Goal:** Implement the Raft Leader Election and Heartbeat logic.
- **Key Actions:**
  - Implement the Raft State Machine (Follower, Candidate, Leader states) within `raft-engine`.
  - Integrate **Randomized Election Timeouts** (ADR 003).
  - Implement Heartbeat logic to maintain leadership.
  - Verify: Killing the leader triggers a successful re-election.
- **Success Metric:** Stable leadership and failover in < 500ms.

## 🚪 Phase 4: Ingress & Mock Egress (`client-cli` & Mock `ai-veto`) [DONE]

- **Goal:** Connect external actors and verify the full request lifecycle using mock policy logic.
- **Key Actions:**
  - Implement the in-memory Log and `AppendEntries` replication logic in `raft-engine`.
  - Initialize `crates/client-cli` as an interactive REPL with "Smart Client" logic.
  - Implement the `GrpcVetoRelay` to bridge the Leader to the `ai-veto` node.
  - Verify: Client -> Leader -> Mock AI -> Consensus -> Commit (Round Trip).
- **Success Metric:** A client mutation is "Committed" after replication and a mock veto.

## 🏰 Phase 4.5: Fortress Hardening (Identity & Type Safety) [DONE]

- **Goal:** Resolve "Legacy Debt" by aligning infrastructure with refined "Fortress" mandates.
- **Key Actions:**
  - **Identity Interceptors:** Migrate `cluster_id` and `target_node_id` validation from manual service guards to centralized gRPC middleware for all node types (ADR 004/005).
  - **NewType Migration:** Complete the transition for all domain identifiers (`LogIndex`, `SequenceId`, `Term`, `ClientId`) across `common` and `raft-engine`.
  - **Client-Side WAL:** Implement local persistence for pending `MutationIntent` and a recovery manager to ensure linearizable re-submission after client crashes (ADR 001).
  - **Resilient Client Loop:** Implement exponential backoff with jitter for retries and align default mutation timeouts with the 30s mandate.
  - **Identity Protocol Upgrade:** Update Protobuf and gRPC interceptors to enforce the `target_node_id` invariant.
- **Success Metric:** Cluster rejects misconfigured identity traffic via centralized middleware.

## 🧠 Phase 5: The AI Moral Advocate (Semantic Oracle) [DONE]

- **Goal:** Implement the 5-Layer Defensive Onion (ADR 007) and transition from mock logic to a relational AI evaluation engine.
- **Key Actions:**
  - **Internal Onion Alignment:** Refactor the Raft engine into the tri-layered **Onion Model** (ADR 009).
  - **Clean Architecture Refactor:** Establish the **node-server** composition root and decouple the State Machine into **lacto-fsm**. This inverted the dependency between consensus and delivery layers.
  - **Contract v2:** Update Protobuf and `CommittedMutation` to support resolved slugs and SI units (ADR 005).
  - **The Onion:** Implement Layer 1-4 logic (Syntactic scrubbing, **Registry Firewall**, Dimensional Fence).
  - **Real AI Integration:** Integrate local Llama via `ollama-rs` into `crates/ai-veto`.
  - **Moral Heuristics:** Develop the "Moral Advocate" persona.
  - **Robustness:** Implement **Leader-Internal Retries** for transient AI resolution failures.
- **Success Metric:** Messy user input is correctly resolved and vetoed by the LLM based on context-aware moral judgement.

## 🛡️ Phase 6: Persistence & Session Integrity (sled & EOS)

- **Goal:** Implement Exactly-Once Semantics and transition to high-fidelity persistent disk storage.
- **Key Actions:**
  - **Consistent Query Path:** Implement the `InventorySource` trait and the `query_state` RPC with Quorum Read verification (ADR 007). [DONE]
  - **Unified Ledger Alignment**: Record all AI evaluation outcomes (Approvals and Vetoes) as first-class consensus events to ensure ledger continuity (ADR 006). [DONE]
  - **Isolated Persistent Storage:** Transition Raft logs, FSM inventory, and system identity to dedicated `sled` database trees with synchronous `fsync` (ADR 001/009). [IN PROGRESS]
  - **Eternal Session Table:** Implement the Session Table as **Permanent Metadata** to prevent the Double-Bootstrap hazard. Deduplicate requests and provide linearizable replays.
  - **Opaque Clinical Reporting:** Implement the `**Secure Clinical**` error standard to prevent session probing and information disclosure.
  - **Stateful Temporal Determinism:** Maintain a persistent logical clock (`last_effective_time`) derived from consensus log timestamps to ensure temporal consistency across the cluster.
  - **The Halt Mandate:** Implement the **Poison-then-Panic** sequence (ADR 009) to prevent "Zombie Node" behavior during state machine or session table divergence.
- **Success Metric:** Idempotent recovery from the log with 100% linearizability and zero internal state disclosure in error responses.

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
