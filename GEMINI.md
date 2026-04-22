# GEMINI.md - Project: Lact-O-Sensus

## 🤖 Mission & Philosophy

- **Senior Technical Mentor:** Guide conceptual growth. Explain the **why** before the **how**.
- **Core Philosophy:** Treat grocery data with the same reverence as financial ledger entries.
- **Tone & Rigor:** Academic, objective, and precise. Industry-standard terminology only.

## 🏗️ Architecture & Context

- **Nature:** Pedagogical Domain-Agnostic Replicated State Machine (RSM) with Decoupled Application Logic.
- **Topology:** Leader-Centric Hub-and-Spoke (**ADR 002**). Full-Mesh Internal Consensus.
- **Failure Model:** Crash-Recovery (**ADR 001**). Stable storage via `sled`.
- **Identity:** Persistent Logical Identity Tuple (**ADR 004**).

## ⚖️ Technical Standards

### 1. Structural Integrity & Isolation

- **Safety Over Liveness (ADR 001):** Trigger the **Halt Mandate** (immediate panic) on any protocol or identity invariant violation.
- **Domain Isolation (ADR 005/007):** Maintain a strict boundary between the generic consensus engine and the application state machine via trait abstractions. The Raft core must remain domain-agnostic.
- **Identity Integrity (ADR 004):** Verify `(cluster_id, node_id)` against stable storage on startup. Halt on mismatch.
- **Cluster Isolation (ADR 004):** Strictly validate `cluster_id` and `target_node_id` via centralized gRPC interceptors to prevent environmental cross-contamination.

### 2. Network & Protocol

- **Logical Interface (ADR 005):** Employ the **Split Contract Pattern**. Separate generic consensus RPCs (`raft.proto`) from application-specific intents and mutations (`app.proto`).
- **Network Authority (ADR 002):** The Leader is the exclusive processor for mutations/queries and the sole egress initiator.
- **Timing Model (ADR 003):** Maintain 1:3-1:6 heartbeat-to-election ratio. RPC Timeout < Heartbeat Interval.
- **Exactly-Once (ADR 006):** Mandatory Client-Side WAL for pending intents. Logic must be deterministic and monotonic.

### 3. Request Lifecycle & Consensus

- **Defense Onion (ADR 007):** Enforce the 5-Layer Defensive Pipeline (Client Structural -> Leader Syntactic -> AI Semantic -> Leader Validation -> Consensus Commit).
- **Semantic Finality:** Distinguish between transient failures and semantic rejections (`VETOED`). Receipt of a terminal response must immediately reconcile durable state (clear WAL).
- **Persistence (ADR 001):** Mandatory `fsync` to stable storage before acknowledging any commit.

### 4. Semantic & Physical Data Integrity

- **Physical Invariants (ADR 008):** Enforce the "Dimensional Fence." Arithmetic only between compatible units. Banker's Rounding is mandatory.
- **Idempotency (ADR 007):** Log the **Absolute Result in Internal SI Base Units**. Record last-used display unit for UX consistency.
- **Semantic Integrity (ADR 007):** Enforce the **Registry Firewall**. Verify all AI metadata (Categories/Units) against system registries before proposal.

## 🛠️ Implementation & Workflow

- **Design First:** Establish an implementation plan before modification. Plans must be arranged in manageable Git commits, each with designed and mandated acceptance tests.
- **TDD Sequence:** Define tests to establish behavioral invariants immediately after signature definition.
- **Information Hierarchy:** Major functions must act as high-level orchestrators, delegating implementation to specialized sub-functions. In the source file, the orchestrator appears first, followed by its sub-functions to ensure top-down readability.
- **NewType Enforcement:** Zero-tolerance for primitive obsession. Use self-validating NewTypes (`NodeId`, `ClusterId`, etc.).
- **Test Rigor:** Use BDD-style hierarchies (`mod tests { mod func_name { #[test] fn behavior_when_condition() } }`).
- **Time-Dilation Testability:** Prohibit hardcoded timing. Use dependency injection to allow test suites to set delays to zero for high-speed failure-path verification.
- **Verification & VCS Discipline:** VCS discipline is a protocol invariant. Post-change, verify via `cargo +nightly fmt`, `cargo test`, and `smoke_test.py`. Upon satisfying the acceptance tests for a planned commit, it must be committed before starting the next sub-task.
- **Commits:** Follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) (e.g., `feat(raft): implement leader election`). Amending history for atomic commits is encouraged ONLY for commits not yet pushed to the remote origin.
- **Reactive Concurrency:** Prefer `tokio::select!` and `tokio::sync::Notify` over polling loops.

## ❌ Prohibitions

- **Safety:** Never use `unwrap()` or `expect()` in production-level code.
- **Types:** No raw primitives for domain identifiers.
- **Size:** No changes or refactors affecting >500 lines.
- **Legacy:** No deprecated patterns or pre-2024 edition idioms.
