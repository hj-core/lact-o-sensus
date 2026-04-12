# Retrospective: Phase 2 - The Infrastructure

## 🗓 Date: 2026-04-12

## 🎯 Scope: Type-State Engine, gRPC Mesh, and Identity Persistence

---

### 🏛 Summary of Achievements

1. **Type-State Engine:** Implemented a generic `RaftNode<S>` engine that enforces role-specific behavioral invariants at compile-time (Follower, Candidate, Leader).
2. **Hardened Identity (ADR 004):** Established a persistent `NodeIdentity` using `sled` to prevent cross-cluster contamination and ensure immutable node IDs.
3. **Domain Integrity:** Eliminated "Primitive Obsession" by refactoring IDs and Addresses into self-validating NewTypes (`NodeId`, `ClusterId`) with zero-allocation failure paths.
4. **Network Mesh:** Orchestrated a 3-node local cluster with unique configurations, verified via a formal automation script (`smoke_test.sh`).
5. **Persistence-Aware Shutdown:** Hardened the lifecycle to ensure `sled` database flushes occur before process exit, fulfilling the "Sync-before-ACK" mandate.

---

### ✅ What Went Well

* **Correctness by Construction:** Adopting the Type-State pattern transformed the Raft specification from documentation into compiler-enforced rules.
* **Shared Guard Logic:** Abstracting cross-cutting concerns (Identity Guard, Health Checks) into the `ServiceState` trait ensured identical security boundaries for both peers and clients.
* **Observability Depth:** The implementation of "Root Node Spans" in `tracing` allows us to distinguish between concurrent requests in a multi-node log stream.
* **Refactor Discipline:** Choosing to perform a "Systemic Quality Pass" mid-phase significantly improved the type safety of the entire crate.

---

### ❌ Challenges & Mistakes

* **State Divergence Risk:** Initially implemented dispatchers with private state copies.
  * *Correction:* Refactored to use a shared `Arc<RwLock<RaftNodeState>>`, ensuring all gRPC services see a unified view of the node.
* **Serialization Escape Hatch:** Identified that default Serde deserialization on NewTypes bypasses constructors.
  * *Correction:* Implemented `#[serde(try_from = "String")]` to force validation during configuration loading.
* **Async Test Context:** Encountered Tokio runtime panics in peer tests due to missing async wrappers.
* **Completion Prematurity:** Marked the shutdown task as complete before verifying database synchronization.

---

### 🧠 Lessons for Phase 3 (The Consensus Heart)

1. **The "Atomic Swap" Requirement:** The transition from Follower to Candidate will require precise use of `std::mem::replace` within the `RwLock` to satisfy Rust's ownership rules.
2. **Interceptor Potential:** While we used manual guards in Phase 2, a gRPC Interceptor should be considered for the "Cluster ID Check" to further reduce boilerplate.
3. **YAGNI vs. Rigor:** We successfully avoided premature `Clone`/`Eq` implementations for complex states, keeping our markers flexible for the upcoming Raft fields.

---

### 📈 Phase 2 Grade: A

*High technical execution. The transition from primitives to domain entities and the successful orchestration of a 3-node mesh provides a rock-solid foundation for the consensus logic.*
