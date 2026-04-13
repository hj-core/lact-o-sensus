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

## 🚪 Phase 4: Ingress & Egress (`client-cli` & `ai-veto`)

* **Goal:** Connect external actors and verify the full request lifecycle.
* **Key Actions:**
  * Initialize `crates/client-cli` (Mutation requests and Leader discovery).
  * Initialize `crates/ai-veto` (Mock response -> OpenAI/Ollama).
  * Implement **Leader Redirection** logic (ADR 002).
  * Verify: Client -> Leader -> Mock AI -> Consensus -> Commit.
* **Success Metric:** A "Full Round Trip" from user input to committed state.

## 🏰 Phase 5: The "Fortress" Layer (Final Polish)

* **Goal:** Implement Persistence, Exactly-Once Semantics (EOS), and real AI logic.
* **Key Actions:**
  * Integrate **`sled`** for persistent Raft logs and State Machine.
  * Implement the **Session Table** and Deduplication logic (ADR 006).
  * Implement the real **AI Moral Advocate** logic in `ai-veto`.
  * Perform "Chaos Testing" (ADR 001) to verify recovery from crashes.
* **Success Metric:** 100% data integrity after cluster-wide power-off/restart.

## 🚀 Phase 6: Future Horizons (Advanced Reliability & Scaling)

* **Goal:** Extend the system's operational efficiency and dynamic flexibility.
* **Proposed Future Works:**
  * **Log Compaction (Snapshotting):** Implementation of full state machine snapshots to manage disk usage and accelerate node recovery.
  * **Dynamic Membership:** Implementing the **Joint Consensus** protocol to safely add/remove nodes from the cluster at runtime (ADR 004 Phase 3).
  * **Dynamic Discovery:** Moving from a static seed list to an automated discovery mechanism (e.g., mDNS or dedicated registry).
  * **Causal History & Rollback:** Leveraging the `state_version` to provide an immutable audit trail and "point-in-time" recovery for the grocery state.
  * **Security & Access Control:** Implementing robust client authentication (e.g., mutual TLS or token-based auth) and authorization policies (RBAC) to restrict access to sensitive categories or administrative operations.
  * **Multi-Tenancy:** Utilizing the `cluster_id` to support multiple isolated consensus groups on shared physical infrastructure.
