# Lact-O-Sensus: Implementation Roadmap

## 🗺 The "Skeleton-First" Strategy

This roadmap prioritizes establishing the **Logical Interface** and **Network Topology** before deep-diving into the complex Raft consensus logic. Each phase builds upon a verified foundation.

---

## 🛠 Phase 1: The Contract (`common`)

* **Goal:** Define the system-wide logical interface and shared data models.
* **Key Actions:**
  * Create `crates/common` library.
  * Define `lacto_sensus.proto` (Consensus, Ingress, and Policy services).
  * Implement the **12-Point Authorized Taxonomy** as a Rust Enum.
  * Set up `tonic-build` for automatic Rust code generation from Protobuf.
* **Success Metric:** All crates can import shared types without duplication.

## 🏗 Phase 2: The Infrastructure (`raft-node` Skeleton)

* **Goal:** Establish the network mesh and basic RPC handlers.
* **Key Actions:**
  * Initialize `crates/raft-node` binary.
  * Implement gRPC server stubs for `RequestVote` and `AppendEntries`.
  * Establish the **Leader-Centric Hub-and-Spoke** topology (ADR 002).
  * Verify connectivity: 3 nodes can ping each other via RPC.
* **Success Metric:** A cluster of 3 nodes can start up and "listen" for traffic.

## ❤️ Phase 3: The Consensus Heart (`raft-node` Core)

* **Goal:** Implement the Raft Leader Election and Heartbeat logic.
* **Key Actions:**
  * Implement the Raft State Machine (Follower, Candidate, Leader states).
  * Integrate **Randomized Election Timeouts** (ADR 003).
  * Implement Heartbeat logic to maintain leadership.
  * Verify: Killing the leader triggers a successful re-election.
* **Success Metric:** Stable leadership and failover in < 500ms.

## 🚪 Phase 4: Ingress & Mock Egress (`client-cli` & Mock `ai-veto`)

* **Goal:** Connect external actors and verify the full request lifecycle using mock policy logic.
* **Key Actions:**
  * Implement the in-memory Log and `AppendEntries` replication logic in `raft-node`.
  * Initialize `crates/client-cli` as an interactive REPL with "Smart Client" redirection logic.
  * Implement the `GrpcVetoRelay` in `raft-node` to bridge the Leader to the `ai-veto` node.
  * Implement a skeletal gRPC server in `crates/ai-veto` that returns deterministic mock approvals/vetoes.
  * Verify: Client -> Leader -> Mock AI -> Consensus -> Commit (Round Trip).
* **Success Metric:** A client mutation is "Committed" to the cluster after successful replication and a mock veto.

## 🧠 Phase 5: The AI Moral Advocate (Real `ai-veto` Integration)

* **Goal:** Transition from mock logic to a relational AI evaluation engine.
* **Key Actions:**
  * Integrate OpenAI API or local Llama via `ollama-rs` into `crates/ai-veto`.
  * Implement the **12-Point Authorized Taxonomy** classifier.
  * Develop relational "Moral Heuristics" (e.g., rejecting sweets based on existing inventory).
  * Verify: The AI correctly categorizes and vetoes items based on the "Moral Advocate" persona.
* **Success Metric:** Deterministic classification and context-aware vetoing by the LLM.

## 🏰 Phase 6: The "Fortress" Layer (Persistence & EOS)

* **Goal:** Implement Persistence, Exactly-Once Semantics (EOS), and crash-recovery.
* **Key Actions:**
  * Integrate **`sled`** for persistent Raft logs and State Machine.
  * Implement the **Session Table** and Deduplication logic (ADR 006).
  * Perform "Chaos Testing" (ADR 001) to verify recovery from crashes.
* **Success Metric:** 100% data integrity after cluster-wide power-off/restart.

## 💾 Phase 7: The "Endless" Log (Log Compaction & Snapshotting)

* **Goal:** Implement state machine snapshots to manage log growth and accelerate recovery.
* **Key Actions:**
  * Implement State Machine serialization and `InstallSnapshot` RPC.
  * Implement log truncation logic once a snapshot is persisted.
  * Verify: A new node can catch up to the cluster using a snapshot.
* **Success Metric:** Successful log truncation and snapshot-based peer synchronization.

## 🔄 Phase 8: Elastic Consensus (Dynamic Membership & Reconfiguration)

* **Goal:** Safely add and remove nodes from the cluster at runtime using Joint Consensus.
* **Key Actions:**
  * Implement the two-phase configuration change ($C_{old} \rightarrow C_{old,new} \rightarrow C_{new}$).
  * Update quorum calculation logic to handle overlapping configurations.
  * Verify: Adding/removing a node does not break safety or liveness invariants.
* **Success Metric:** Seamless cluster expansion/contraction without downtime.

## 🚀 Phase 9: Future Horizons (Advanced Reliability & Scaling)

* **Goal:** Extend the system's operational efficiency and dynamic flexibility.
* **Proposed Future Works:**
  * **Security & Access Control:** mutual TLS and RBAC for cluster access.
  * **Multi-Tenancy:** Utilizing `cluster_id` for isolated groups on shared infrastructure.
  * **Causal History:** Immutable audit trail and "point-in-time" recovery.
