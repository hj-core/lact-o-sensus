# Retrospective: Phase 6 - Persistence & Session Integrity (sled & EOS)

## 🗓 Date: 2026-05-16

## 🎯 Scope: Isolated Storage, Unified Ledger, Exactly-Once Semantics (EOS), and Chaos Auditing

---

### 🏛 Summary of Achievements

1. **Isolated Storage (Split DBs):** Decoupled the Raft Consensus Log from the Application State Machine (FSM) by implementing dedicated `sled` instances. This architectural split ensures that consensus persistence does not leak into business logic, satisfying crash-recovery mandates without cross-domain contamination.
2. **Exactly-Once Semantics (EOS):** Implemented a persistent Session Table and linearizable retry logic (ADR 006). The system now provides durable tracking of client sequences, guaranteeing that mutations (whether Approved or Vetoed) are applied exactly once even across total cluster restarts.
3. **Unified Ledger (Veto Logging):** Transformed the "Firewall Veto" into a "Ledger Veto" by recording rejections as first-class Raft events. This ensures that every node in the cluster reaches consensus on moral judgments, preserving cluster-wide determinism.
4. **Strict Horizon Barrier:** Implemented a strictly linearizable query path with a "Read-Your-Writes" barrier. By utilizing a reactive `last_applied` signal and a **Strict Horizon Check**, the Gateway now ensures that clients never observe stale snapshots or "future-dated" versions beyond the consistent cluster horizon.
5. **Replication Chaos Audit:** Proven system durability through a rigorous chaos suite in `scripts/smoke_test.py`. Verified 100% data integrity and state parity after multiple randomized `SIGKILL` cycles during active, high-pressure mutation floods.

---

### ✅ What Went Well

* **Reactive Signal Integration:** Adding `last_applied` to the `ConsensusProgress` signal created a clean, event-driven bridge between the physical FSM and the logical Gateway, eliminating the need for inefficient polling.
* **Information Opacity (Mandate 4.3):** successfully hardened error reporting to be clinically opaque. Rejection messages now identify the category of failure without disclosing internal cluster indices.
* **Raft-Native Leader Discovery:** Transitioning the smoke test runner to use **Term Sovereignty** for leader discovery solved systemic flakiness during rapid failover cycles, proving that test infrastructure must be as robust as the system under test.

---

### ❌ Challenges & Mistakes

* **The "Zombie Task" Hazard:** Initially proposed an unbounded wait for `min_state_version`, which could have leaked server-side resources if a client requested a far-future version.
  * *Correction:* Implemented the **Strict Horizon Check** to reject queries for versions exceeding the current `last_committed` immediately.
* **Semantic Normalization Jitter:** AI item key resolution (e.g., `apple_2`) initially caused false positives in the integrity audit.
  * *Correction:* Hardened the audit suite with flexible substring matching and normalized key comparisons.
* **Stale Seed Ports:** Realized that the `MutationFlooder` was "blind" during chaos if its initial seed node was killed.
  * *Correction:* Refactored the flooder to be cluster-aware and dynamically select living nodes for seed connection.

---

### 🧠 Lessons for the Future

1. **Strictness is Safety:** In distributed systems, "waiting patiently" is often a resource leak. Strict boundary checks (like the Horizon Check) are better than timeouts for preventing systemic congestion.
2. **Consensus is a Two-Sided Heart:** A persistent log is useless without a persistent session table. Linearizability is a property of the *entire* storage stack, not just the Raft log.
3. **Chaos is the best Architect:** The "Replication Chaos" test revealed more subtle architectural edge cases (like term-blind leader discovery) than weeks of unit testing could have reached. High-pressure end-to-end testing is mandatory for distributed clinical state.

---

### 📈 Phase 6 Grade: A+

*The system has transitioned from a volatile prototype to a hardened, persistent distributed ledger. With EOS and the Chaos Audit complete, Lact-O-Sensus is now physically and logically durable.*
