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
- **`node-server`**: The composition root that wires all layers via dependency injection.

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

## 4. Technical Mandates

- **4.1. Poison-then-Panic:** Transition logical state to `Poisoned` immediately before any invariant-violation `panic!`.
- **4.2. Safety Prohibitions:** Never use `unwrap()` or `expect()` in production-level code.
- **4.3. NewType Enforcement:** Zero-tolerance for primitive obsession. Use self-validating NewTypes (e.g., `NodeId`, `ClusterId`) for all domain identifiers.
- **4.4. Error Categorization (`thiserror` vs `anyhow`):** Map all library-returned errors to domain-specific Error enums via `thiserror`. The use of `anyhow` is strictly PROHIBITED in core logic (`crates/common`, `crates/raft-engine`, `crates/gateway`), and is permitted exclusively in top-level `main.rs` binaries.
- **4.5. Clinical Decoupling:** Prohibit the leak of grocery domain logic into `raft-engine`. All communication between layers must occur via the `common` contract.
- **4.6. Heartbeat Decoupling:** Prohibit blocking the Raft heartbeat or election timers with external I/O (e.g., AI Veto egress). All external policy resolution must occur in the delivery layer or via asynchronous task delegation to preserve cluster liveness.
- **4.7. Factory-Only Egress:** Prohibit manual gRPC message construction. Use NewType-aware factories (`new`) in `common/src/proto.rs` to ensure safe boundary transitions.
- **4.8. Storage Integrity:** Every physical mutation to the Raft core state must be followed by an explicit synchronous disk flush (`flush()`) before responding to an RPC to prevent log loss after a crash.
- **4.9. Physical Truth:** Prohibit non-canonical measurements. All mutations must be stabilized to SI base units using **Banker's Rounding (Half-to-Even)** and transmitted as stringified fixed-point decimals before being logged to ensure cross-architecture determinism.
- **4.10. Registry Firewall:** All AI-provided metadata must be verified against hardcoded system registries before finalization.
- **4.11. Structured Observability:** Prohibit unstructured logging for clinical events. All protocol transitions, physical mutations, and lifecycle spans must use structured `tracing` events with standardized fields (`trace_id`, `term`, `index`, `client_id`) and respect the mandatory redaction boundaries defined in ADR 010.
- **4.12. Information Opacity:** External errors must identify the category of failure (e.g., Sequence Gap) but MUST NOT disclose internal metadata or state values.

## 5. Workflow

- **5.1. Planning & Atomic Commits:** All changes require an implementation plan. Commits must be atomic, manageable (fight for less than 300 line of changes), and follow [Conventional Commits](https://www.conventionalcommits.org/). Design acceptance tests as well.
- **5.2. Orchestration Pattern:** Major functions must act as high-level orchestrators, delegating implementation to specialized sub-functions for top-down readability.
- **5.3. BDD Specification:** All non-trivial logic MUST be documented via a nested BDD-style module hierarchy. This structure serves as the project's living clinical specification:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    mod <function_name> {
        use super::*;
        mod <specific_behavior_or_scenario> {
            use super::*;
            #[test]
            fn <expected_outcome>_when_<condition>() { ... }
        }
    }
}
```

- **5.4. TDD Protocol:** Strictly enforce the three-phase implementation sequence:
    1. Define/align syntax (with `todo!()`).
    2. Define behavior through failing invariant tests (Red).
    3. Implement logic until tests pass (Green).
- **5.5. Verification Pipeline:** Before every commit, execute the clinical verification sequence:
  - `cargo +nightly fmt --all` (verify zero diff)
  - `cargo test --all-features`
  - `cargo clippy --all-targets -- -D warnings`
  - `python3 scripts/smoke_test.py`
