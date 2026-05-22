# Project: Lact-O-Sensus

## 1. Project Overview

**Lact-O-Sensus** is a clinical, leader-centric distributed ledger designed for high-fidelity grocery inventory management. It treats physical grocery state with the same rigor as financial transactions, utilizing a **Domain-Agnostic Replicated State Machine (RSM)** powered by the Raft consensus protocol. The system adheres to **Clean Architecture** principles to ensure that consensus, business logic, and delivery layers are strictly decoupled.

## 2. Project Structure

The workspace is organized into 7 specialized crates to enforce dependency inversion and boundary defense:

- **`common`**: Foundational types, Protobuf contracts, and the Universal SI Unit Registry.
- **`raft-engine`**: Domain-agnostic Raft implementation (Election, Replication, Heartbeats).
- **`lacto-fsm`**: The business logic; implements the `StateMachine` trait and manages `sled` persistence.
- **`gateway`**: The gRPC delivery layer and defensive "Ingress Firewall."
- **`ai-veto`**: External Oracle for semantic resolution and moral evaluation.
- **`client-cli`**: Consumer REPL with local WAL for linearizable retries and automatic leader discovery/redirection.
- **`node-server`**: The composition root submerged in dependency injection.

## 3. Architectural Decision Records

- **3.1. Node Failure Model (ADR 001):** Implements a Crash-Recovery (CR) model for cluster nodes and treats the AI Veto as a Byzantine Oracle to preserve cluster determinism.
- **3.2. Network Topology (ADR 002):** Enforces a Leader-Centric Hub-and-Spoke model for external interactions and a Full-Mesh for internal consensus.
- **3.3. Timing & Synchrony (ADR 003):** 1:3–1:6 Heartbeat-to-Election ratio (50ms/150ms-300ms). Safety is independent of time; liveness is partially synchronous.
- **3.4. Bootstrapping & Identity (ADR 004):** Identifies nodes by `(ClusterId, NodeId)` NewTypes. Prohibits cross-cluster contamination via Middleware identity guards.
- **3.5. Logical Interface (ADR 005):** Strict separation between generic consensus (`raft.proto`) and application logic (`app.proto`).
- **3.6. Exactly-Once Semantics (ADR 006):** Guarantees linearizability via a replicated Session Table. Every mutation outcome (Success or Veto) is a logged consensus event.
- **3.7. Defensive Mutation Lifecycle (ADR 007):** A 5-layer "Defense Onion" pipeline: Structural Intent -> Syntactic Fortress -> Semantic Oracle -> Registry Firewall -> Consensus Commit.
- **3.8. Universal Unit Registry (ADR 008):** Internal SI stabilization using `rust_decimal`. All physical state is normalized to `g` or `ml` using Banker's Rounding.
- **3.9. Internal Node Architecture (ADR 009):** The "Tri-Layer Onion" (Physical Foundation -> Logical Orchestrator -> Execution Shell). Implements **Poison-then-Panic** to handle invariant violations.
- **3.10. Clinical Telemetry (ADR 010):** Establishes a structured tracing framework with mandatory PII redaction (Client ID truncation, TRACE-only justifications) to enable deterministic reconstruction of distributed events.

## 4. Technical Standards

Implementation must adhere to the [Lact-O-Sensus Review Checklist](docs/checklists/review_checklist.md). All code reviews and new implementations are evaluated against these clinical audit points.

## 5. Workflow

- **5.1. Implementation Planning:** Major features or refactors require an implementation plan following the [Planning Checklist](docs/checklists/planning_checklist.md).
- **5.2. TDD Protocol:** Define behavior through failing tests (Red) before implementation (Green).
- **5.3. Verification Pipeline:** Execute the clinical verification sequence and confirm success:
  - `cargo +nightly fmt --all` (verify zero diff)
  - `cargo test --all-features`
  - `cargo clippy --all-targets -- -D warnings`
  - `python3 scripts/smoke_test.py`
- **5.4. Clinical Review:** Evaluate verified changes against the [Review Checklist](docs/checklists/review_checklist.md). Wait for feedback and resolve all findings before proceeding.
- **5.5. Atomic Commits:** Finalize changes as atomic units following [Conventional Commits](https://www.conventionalcommits.org/).
