# GEMINI.md - Project: Lact-O-Sensus

## 1. Mission & Philosophy

- **1.1. Senior Technical Mentor:** Guide conceptual growth. Explain the **why** before the **how**.
- **1.2. Ledger Reverence:** Treat grocery data with the same clinical rigor as financial transactions. Grocery state is a clinical record of the physical world, not a mere approximation.
- **1.3. Fallibility Awareness:** Assume users and peers make inconsistent or disruptive decisions. Proactively block changes that introduce structural fragility.
- **1.4. Tone & Rigor:** Academic, objective, and precise. Industry-standard terminology only.

## 2. Architecture

- **2.1. Nature:** Domain-Agnostic Replicated State Machine (RSM) with Decoupled App Logic.
- **2.2. Topology:** Leader-Centric Hub-and-Spoke (**ADR 002**). Full-Mesh Internal Consensus.
- **2.3. Persistence:** Crash-Recovery (**ADR 001**) via `sled`. Exactly-Once WAL (**ADR 006**).
- **2.4. Physicality:** Universal SI Unit Registry (**ADR 008**) with high-precision SI stabilization.

## 3. Technical Mandates

### 3.1. Structural Integrity (The Onion Model)

- **3.1.1. Poison-then-Panic (ADR 009):** To mitigate the lack of poisoning in `tokio::sync::RwLock`, you MUST transition logical state to `Poisoned` immediately before any invariant-violation `panic!`. This protocol MUST also be triggered if a persisted identity (`ClusterId`/`NodeId`) mismatch is detected at startup (**ADR 004**).
- **3.1.2. Tri-Layer Onion (Internal):** Strictly isolate the **Physical Foundation** (deterministic logic), **Logical Orchestrator** (Raft protocol rules), and **Execution Shell** (concurrency and signaling).
- **3.1.3. Registry Firewall (ADR 007):** Verify all AI metadata (Categories/Units) against system registries. AI is for resolution; the Gateway acts as a **Clinical Notary**, proposing both Approvals and Vetoes to the ledger to maintain contiguous sequence integrity.
- **3.1.4. Storage Integrity (ADR 001):** Utilize specialized trees (`hard_state`, `logs`, `conf_state`) within the `sled` database. Every physical mutation to the Raft core state (`currentTerm`, `votedFor`, and `log[]`) MUST be followed by an explicit `flush_async()` before responding to an RPC to prevent "Phantom Votes" or log loss after a crash.

### 3.2. Network & Boundary

- **3.2.1. Split Contract (ADR 005):** Isolate generic consensus (`raft.proto`) from App intents (`app.proto`).
- **3.2.2. Factory-Only Egress:** Prohibit manual gRPC message construction. Use NewType-aware factories (`new`) in `common/src/proto.rs` to ensure safe boundary transitions.
- **3.2.3. Timing (ADR 003):** Maintain 1:3–1:6 heartbeat-to-election ratio. RPC Timeout < Heartbeat Interval.
- **3.2.4. Identity Guarding (ADR 004):** Every gRPC request MUST be validated at the **Interceptor/Middleware layer** against the local logical identity. Messages with mismatched `ClusterId` or `TargetNodeId` MUST be rejected before reaching application logic.

### 3.3. Error Categorization & Boundary Defense

- **3.3.1. Explicit Failure Modes:** Map all library-returned errors to domain-specific Error enums (via `thiserror`) before they exit their originating layer.
- **3.3.2. The Core Boundary:** Prohibit the use of `anyhow` in internal core logic (all files in `crates/common`, `crates/raft-node/src/`, and `crates/gateway`). `anyhow` is permitted EXCLUSIVELY in `main.rs` and binary-entry modules for top-level bootstrap reporting.

### 3.4. Data Fidelity & Physical Truth

- **3.4.1. Physical Determinism (ADR 008):** Strictly prohibit the entry of non-canonical or un-stabilized measurements. The SI Unit Registry is the absolute source of physical truth. All physical quantities MUST be represented using `rust_decimal::Decimal` to avoid floating-point inaccuracies.
- **3.4.2. Drift Prevention:** All physical state mutations MUST be stabilized to SI base units using **Banker's Rounding** (`RoundingStrategy::MidpointNearestEven`) to eliminate cumulative numeric bias within the Replicated State Machine.

### 3.5. Linearizability & Session Integrity (ADR 006)

- **3.5.1. Replicated Session Table:** Exactly-Once deduplication MUST be implemented as a deterministic, replicated side-effect within the State Machine.
- **3.5.2. Sequence Strictness:** The State Machine MUST strictly enforce the `seq == last_seen + 1` invariant for new mutations. Out-of-order or "gapped" sequences MUST be rejected. To satisfy this without client-side complexity, **every evaluation outcome (Success/Veto) MUST be logged as a first-class consensus event.**
- **3.5.3. Session Halt Mandate:** Any detection of session table inconsistency (e.g., during snapshot loading or hash verification) MUST trigger an immediate `Poison-then-Panic`.

## 4. Implementation & Workflow

### 4.1. Design First

Establish an implementation plan before modification. Plans must be arranged in manageable Git commits, each with designed and mandated acceptance tests.

### 4.2. Anatomy of a Clinical Specification (BDD)

All non-trivial logic MUST be documented via a nested BDD-style module hierarchy. This structure serves as the project's living clinical specification, ensuring behavioral invariants are easily discoverable and auditable:

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

### 4.3. TDD Protocol (Atomic Specification)

Strictly enforce the three-phase implementation sequence for all non-trivial logic:

1. **Signature Alignment:** Define or advance the function signature. Use `todo!()` or a mock as a placeholder to satisfy the compiler without implementing logic.
2. **Invariant Specification (Red):** Codify behavioral requirements through tests (following the BDD anatomy above) that fail against the placeholder.
3. **Logic Consolidation (Green):** Implement or refactor the logic until all specification tests pass. Test modification is prohibited during this phase unless a signature adjustment is required.

### 4.4. Information Hierarchy

Major functions must act as high-level orchestrators, delegating implementation to specialized sub-functions. In the source file, the orchestrator appears first, followed by its sub-functions to ensure top-down readability.

### 4.5. Clinical VCS Protocol

- **4.5.1. Commit Style:** Mandatory [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/). Atomic commits for every sub-task.

### 4.6. Defensive Patterns

- **4.6.1. NewType Enforcement:** Zero-tolerance for primitive obsession. Use self-validating NewTypes (`NodeId`, `ClusterId`, etc.).
- **4.6.2. Time-Dilation Testability:** Prohibit hardcoded timing. Use dependency injection to allow test suites to set delays to zero for high-speed failure-path verification.
- **4.6.3. Reactive Concurrency:** Prefer `tokio::select!` and `tokio::sync::Notify` over polling loops.

## 5. Standard Operations (Verification)

Before every commit, the following clinical verification sequence MUST be executed:

1. **Formatting:** `cargo +nightly fmt --all` (verify zero diff).
2. **Logic Check:** `cargo test --all-features`
3. **Static Analysis:** `cargo clippy --all-targets -- -D warnings`
4. **Physicality Validation:** `python3 scripts/smoke_test.py`

## 6. Prohibitions

- **6.1. Safety:** Never use `unwrap()` or `expect()` in production-level code.
- **6.2. Types:** No raw primitives for domain identifiers.
- **6.3. Size:** No changes or refactors affecting >500 lines.
- **6.4. Legacy:** No deprecated patterns or pre-2024 edition idioms.
