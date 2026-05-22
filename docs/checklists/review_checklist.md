# Lact-O-Sensus Code Review Checklist

This checklist is derived from the project's Architectural Decision Records (ADRs) and the technical mandates. Use the specific rule numbers when citing violations during code reviews.

## Phase 1: Architecture & Structural Boundaries

1. **The `raft-engine`** should be domain-agnostic and free of grocery-specific logic, relying only on the `common` contract.
2. **Major functions** should act as high-level orchestrators, delegating implementation details to specialized sub-functions for top-down readability.
3. **Source code layout** should be logically ordered to support top-down readability, with public orchestrators and primary logic appearing before private helpers and internal utilities.
4. **gRPC messages** should be instantiated exclusively via NewType-aware factories (`new()`) in `common/src/proto.rs` instead of manual inline construction.
5. **Identity validation** (`cluster_id` and `target_node_id`) should be strictly enforced via centralized gRPC middleware. [ADR 004, ADR 005]

## Phase 2: Data Integrity & Foundational Types

6. **Domain identifiers** (e.g., `NodeId`, `ClusterId`, `LogIndex`, `Term`, `ClientId`) should use self-validating NewTypes to avoid primitive obsession. [ADR 004]
7. **Physical quantities** should be normalized to SI base units (`g` or `ml`) using Banker's Rounding (Half-to-Even). [ADR 008]
8. **Physical state values** should be transmitted as stringified fixed-point decimals before being logged to ensure cross-architecture determinism.
9. **AI-provided metadata** (categories/units) should be verified against hardcoded system registries before finalization.

## Phase 3: Clinical Safety & Reliability

10. **Production code** should avoid `unwrap()` or `expect()` entirely.
11. **Core library logic** (`crates/common`, `crates/raft-engine`, `crates/gateway`) should use `thiserror` for domain-specific enums.
12. **The `anyhow` crate** should be restricted exclusively to top-level `main.rs` binaries.
13. **Invariant violations** should trigger the `Poison-then-Panic` sequence, transitioning logical state to `Poisoned` before panicking. [ADR 009]
14. **External error responses** should hide internal metadata and state values while preventing probing of sequence gaps or internal states. [ADR 006]
15. **PII redaction** (e.g., Client ID truncation, sensitive payload justifications) should be strictly enforced according to ADR 010. [ADR 010]

## Phase 4: Persistence & Consensus Domain

16. **Consensus timers** (heartbeat and election) should be strictly decoupled from external I/O or policy resolution to guarantee cluster liveness.
17. **Raft core state mutations** should be followed by an explicit, synchronous disk `flush()` before responding to an RPC.
18. **The Session Table** should deduplicate intents and guarantee linearizable replays to enforce Exactly-Once Semantics (EOS). [ADR 006]
19. **Mutation outcomes** (both Approvals and Vetoes) should be recorded as first-class, verifiable consensus events to ensure ledger continuity. [ADR 006]

## Phase 5: Engineering Standards & Observability

20. **Clinical events** should use structured `tracing` spans and events (including `trace_id`, `term`, `index`, and `client_id`) instead of unstructured logging. [ADR 010]
21. **Non-trivial logic** should be documented and verified via the mandatory nested BDD-style module hierarchy.
22. **Imports** should be preferred over fully-qualified names (FQN) to maintain brevity and readability, unless FQN is strictly required for disambiguation.
23. **Modules and implementations** should be properly documented using a natural and professional tone.
24. **Documentation references** within the code (comments/docs) must be restricted to ADRs or the Project Roadmap; do not reference transient task orders or session-specific instructions.
25. **Code artifacts** should compile without clippy warnings and align with `cargo +nightly fmt` (zero-diff).

---

### BDD-Style Test Arrangement Example

All non-trivial logic MUST follow this structure:

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
