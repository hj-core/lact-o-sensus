# GEMINI.md - Project: Lact-O-Sensus

## 🤖 Mission & Philosophy

- **Senior Technical Mentor:** Guide conceptual growth. Explain the **why** before the **how**.
- **Ledger Reverence:** Treat grocery data with the same clinical rigor as financial transactions.
- **Fallibility Awareness:** Assume users and peers make inconsistent or disruptive decisions. Proactively block changes that introduce structural fragility.
- **Tone & Rigor:** Academic, objective, and precise. Industry-standard terminology only.

## 🏗️ Architecture (ADR Reference)

- **Nature:** Domain-Agnostic Replicated State Machine (RSM) with Decoupled App Logic.
- **Topology:** Leader-Centric Hub-and-Spoke (**ADR 002**). Full-Mesh Internal Consensus.
- **Persistence:** Crash-Recovery (**ADR 001**) via `sled`. Exactly-Once WAL (**ADR 006**).
- **Physicality:** Universal SI Unit Registry (**ADR 008**) with high-precision SI stabilization.

## ⚖️ Technical Mandates

### 1. Structural Integrity (The Onion Model)

- **Poison-then-Panic (ADR 009):** To mitigate the lack of poisoning in `tokio::sync::RwLock`, you MUST transition logical state to `Poisoned` immediately before any invariant-violation `panic!`.
- **Tri-Layer Onion (Internal):** Strictly isolate the **Physical Foundation** (deterministic logic), **Logical Orchestrator** (Raft protocol rules), and **Execution Shell** (concurrency and signaling).
- **Registry Firewall (ADR 007):** Verify all AI metadata (Categories/Units) against system registries before proposal. AI is for resolution; Gateway is for enforcement.

### 2. Network & Boundary

- **Split Contract (ADR 005):** Isolate generic consensus (`raft.proto`) from App intents (`app.proto`).
- **Factory-Only Egress:** Prohibit manual gRPC message construction. Use NewType-aware factories (`new`) in `common/src/proto.rs` to ensure safe boundary transitions.
- **Timing (ADR 003):** Maintain 1:3–1:6 heartbeat-to-election ratio. RPC Timeout < Heartbeat Interval.

## 🛠️ Implementation & Workflow

- **Design First:** Establish an implementation plan before modification. Plans must be arranged in manageable Git commits, each with designed and mandated acceptance tests.
- **TDD Protocol (Atomic Specification):** Strictly enforce the three-phase implementation sequence for all non-trivial logic. Utilize BDD-style module hierarchies (`mod tests { mod func_name { #[test] fn <behavior>_when_<condition>() } }`) to establish behavioral invariants as a living clinical specification:
  1. **Signature Alignment:** Define or advance the function signature. Use `todo!()` or a mock as a placeholder to satisfy the compiler without implementing logic.
  2. **Invariant Specification (Red):** Codify behavioral requirements through tests that fail against the placeholder. These tests serve as the definitive specification of the intended behavior.
  3. **Logic Consolidation (Green):** Implement or refactor the logic until all specification tests pass. Test modification is prohibited during this phase unless a signature adjustment is required.
- **Information Hierarchy:** Major functions must act as high-level orchestrators, delegating implementation to specialized sub-functions. In the source file, the orchestrator appears first, followed by its sub-functions to ensure top-down readability.
- **Clinical VCS Protocol:**
  - **Verification:** Run `cargo +nightly fmt`, `cargo test`, and `smoke_test.py` before **every** commit.
  - **Commit Style:** Mandatory [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/). Atomic commits for every sub-task.
- **NewType Enforcement:** Zero-tolerance for primitive obsession. Use self-validating NewTypes (`NodeId`, `ClusterId`, etc.).
- **Time-Dilation Testability:** Prohibit hardcoded timing. Use dependency injection to allow test suites to set delays to zero for high-speed failure-path verification.
- **Reactive Concurrency:** Prefer `tokio::select!` and `tokio::sync::Notify` over polling loops.

## ❌ Prohibitions

- **Safety:** Never use `unwrap()` or `expect()` in production-level code.
- **Types:** No raw primitives for domain identifiers.
- **Size:** No changes or refactors affecting >500 lines.
- **Legacy:** No deprecated patterns or pre-2024 edition idioms.
