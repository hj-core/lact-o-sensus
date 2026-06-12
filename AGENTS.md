# Project: Lact-O-Sensus

## MASTER RULE

Be brutally honest, don't flatter. If code is wrong, say it's wrong. If a design is flawed, call it out. Never sugarcoat or hedge. The codebase demands clinical rigor — feedback must match.

## 1. Project Overview

**Lact-O-Sensus** is a clinical, leader-centric distributed ledger designed for high-fidelity grocery inventory management. It treats physical grocery state with the same rigor as financial transactions, utilizing a **Domain-Agnostic Replicated State Machine (RSM)** powered by the Raft consensus protocol. The system adheres to **Clean Architecture** principles to ensure that consensus, business logic, and delivery layers are strictly decoupled.

## 2. Project Structure

The workspace is organized into 9 crates to enforce dependency inversion and boundary defense:

- **`common`**: Foundational types, Protobuf contracts, and the Universal SI Unit Registry.
- **`common-rpc`**: gRPC middleware and identity interceptors (ADR 004/005).
- **`raft-engine`**: Domain-agnostic Raft implementation (Election, Replication, Heartbeats).
- **`lacto-fsm`**: The business logic; implements the `StateMachine` trait and manages `sled` persistence.
- **`gateway`**: The gRPC delivery layer and defensive "Ingress Firewall."
- **`ai-veto`**: External Oracle for semantic resolution and moral evaluation.
- **`mock-veto`**: Lightweight mock AI oracle for deterministic testing.
- **`client-cli`**: Consumer REPL with local WAL for linearizable retries and automatic leader discovery/redirection.
- **`node-server`**: The composition root submerged in dependency injection.

## 3. Architectural Decision Records

- **3.1. Node Failure Model (ADR 001):** Adopts a Crash-Recovery (CR) model supported by a client-side WAL for linearizable retries and treats the AI Veto as a Byzantine Oracle.
- **3.2. Network Topology (ADR 002):** Enforces a Leader-Centric Hub-and-Spoke model for external clients and a Full-Mesh network for internal cluster consensus.
- **3.3. Timing & Synchrony (ADR 003):** Sets a 1:3–1:6 Heartbeat-to-Election ratio to ensure liveness while maintaining safety independence from timing assumptions.
- **3.4. Bootstrapping & Identity (ADR 004):** Mandates strict identity validation via `(ClusterId, NodeId)` NewTypes and enforces a Bootstrap Halt on configuration mismatches.
- **3.5. Logical Interface (ADR 005):** Strictly separates generic consensus payloads from application domain logic through gRPC metadata and interceptors.
- **3.6. Exactly-Once Semantics (ADR 006):** Guarantees linearizability by recording every mutation outcome in a replicated Session Table.
- **3.7. Defensive Mutation Lifecycle (ADR 007):** Implements a 5-layer "Defense Onion" pipeline to scrub, resolve, and validate mutation intents before they reach consensus.
- **3.8. Universal Unit Registry (ADR 008):** Normalizes all physical quantities to SI base units using high-precision fixed-point arithmetic and Banker's Rounding for stability.
- **3.9. Internal Node Architecture (ADR 009):** Mandates a strictly synchronous core (Physical and Logical layers) protected by a "Poison-then-Panic" protocol within an asynchronous Execution Shell.
- **3.10. Clinical Telemetry (ADR 010):** Establishes a structured tracing framework with authoritative Gateway `trace_id` generation and mandatory PII redaction.
- **3.11. Asynchronous Log Compaction (ADR 011):** Offloads state machine snapshot generation and restoration to background threads using a "Restoration Tombstone" protocol to preserve heartbeat stability and crash-safety.

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
- **5.4. Clinical Review:** Evaluate verified changes against the [Review Checklist](docs/checklists/review_checklist.md) and resolve all violations.
- **5.5. Atomic Commits:** Finalize changes as atomic units following [Conventional Commits](https://www.conventionalcommits.org/). Ensure the commit message body briefly describes the context and the major works that have been done.
- **5.6. Documentation Boundary Discipline:** Restricts cross-references in textual artifacts to the `docs/adrs/` directory only.
