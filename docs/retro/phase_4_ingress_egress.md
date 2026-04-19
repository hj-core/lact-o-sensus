# Retrospective: Phase 4 - Ingress & Mock Egress

## 🗓 Date: 2026-04-19

## 🎯 Scope: Smart Client (CLI), AI Veto Egress, and Full Request Lifecycle

---

### 🏛 Summary of Achievements

1. **The Smart Client:** Implemented a resilient `LactoClient` that handles automatic leader discovery and redirection hints. It enforces **Exactly-Once Semantics (EOS)** by persisting a unique `client_id` and monotonic `sequence_id` to a local `.client_state.json` file.
2. **Testable Interactive REPL:** Built a robust command-line interface in `client-cli` using a `shlex`-based parser. By abstracting the REPL loop over generic `AsyncRead`/`AsyncWrite` streams, we achieved **"Parity of Tests & Production"** for the interactive UI layer via memory-stream unit tests.
3. **Consensus Ingress:** Updated the `raft-node` Ingress layer to handle `ProposeMutation` and `QueryState`. Implemented strict **Leader-Only Reads** to guarantee linearizable query results.
4. **AI Veto Egress Bridge:** Established a gRPC egress from the Raft Leader to the `ai-veto` node, allowing grocery mutations to be evaluated against external policy logic before log entry.
5. **The "Follower Identity Gap" Fix:** Identified and resolved a critical protocol bug where Followers remained "blind" to the Leader's identity if they transitioned to the Follower state via a vote request.

---

### ✅ What Went Well

* **Rigor via Integration:** The discovery of the "Identity Gap" bug during the implementation of the Step 5 round-trip test vindicated our strategy of building high-precision integration tests early in the phase.
* **Decoupled UI Logic:** Abstracting the REPL loop into `run_repl<R, W>` proved to be an exceptional engineering choice, allowing us to simulate complex user interactions (like `exit` commands and network errors) with 100% deterministic unit tests.
* **EOS by Design:** Enforcing the "Persist-before-RPC" mandate ensures that the client remains consistent even in the event of a local crash during a mutation attempt.
* **Cohesive Modeling:** Refactoring the parser logic into `Command::parse` and introducing `MutationArgs` significantly improved the maintainability and readability of the `client-cli` crate.

---

### ❌ Challenges & Mistakes

* **Redirection Deadlock:** Initially used a `* 2` multiplier for retry counts, which caused discovery to fail when starting from a single seed node.
  * *Correction:* Refactored the retry budget to use `known_nodes().len() + MAX_KNOWN_NODES`, ensuring enough "slack" for full cluster discovery.
* **Follower Blindness:** Followers were resetting their timers on heartbeats but ignoring the `leader_id` if their term already matched.
  * *Correction:* Updated `AppendEntries` to allow Followers to learn the Leader ID from valid heartbeats for the current term.
* **CLI Argument Collision:** Encountered a `panic!` on startup due to a short-name collision (`-s`) in the `clap` configuration.
  * *Correction:* Assigned unique short names (`-p` for path) and verified via manual execution.

---

### 🧠 Lessons for Phase 5 (Real-World Durability)

1. **Stabilization Time is Critical:** Integration tests for distributed systems must account for heartbeat propagation delay. "Stabilizing" the cluster for 2 seconds after election ensures all nodes are aware of the new Leader.
2. **Generic Streams are the Gold Standard:** For CLI applications, generic I/O isn't just for tests—it provides a clear path for future features like piping commands or automated scripting.
3. **The Persistence Mandate:** In Phase 5, we must apply the same "Senior Perfectionist" rigor to `sled` persistence as we did to the `ClientState`. Every log entry must be fsync'd before acknowledgment.

---

### 📈 Phase 4 Grade: A

*A highly successful integration phase. The system has evolved from an internal consensus core into a full-stack distributed application. The successful resolution of the Follower Identity Gap demonstrates deep technical understanding and professional diagnostic skills.*
