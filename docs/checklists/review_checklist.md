# Lact-O-Sensus Code Review Checklist

This checklist is derived from the project's Architectural Decision Records (ADRs) and the technical mandates. Use the specific rule numbers when citing violations during code reviews.

## Phase 1: Architecture & Structural Boundaries

1. The **`raft-engine`** should be domain-agnostic and free of grocery-specific logic.
2. **Major functions** should act as high-level orchestrators, delegating implementation details to specialized sub-functions.
3. **Source code layout** should be logically ordered to support top-down readability.
4. **gRPC messages** should be instantiated exclusively via NewType-aware factories (`new()`) in `common/src/proto.rs`.
5. **Identity validation** (`cluster_id` and `target_node_id`) should be strictly enforced via centralized gRPC middleware. [ADR 004, ADR 005]
6. **Heavy operations** (e.g., reading the full inventory) should be deferred until after initial structural intent validation is complete.

## Phase 2: Data Integrity & Foundational Types

7. **Domain identifiers** (e.g., `NodeId`, `LogIndex`, `Term`, `ClientId`) should use self-validating NewTypes to avoid primitive obsession. [ADR 004]
8. **Physical quantities** should be normalized to SI base units (`g` or `ml`) using Banker's Rounding (Half-to-Even). [ADR 008]
9. **Physical state values** should be transmitted as stringified fixed-point decimals before being logged to ensure cross-architecture determinism.
10. **AI-provided metadata** (categories/units) should be verified against hardcoded system registries before finalization. [ADR 007]

## Phase 3: Clinical Safety & Reliability

11. **Production code** should avoid `unwrap()` or `expect()` entirely.
12. **Core library logic** should use `thiserror` for domain-specific enums.
13. **The `anyhow` crate** should be restricted exclusively to top-level `main.rs` binaries.
14. **Invariant violations** should trigger the `Poison-then-Panic` sequence, transitioning logical state to `Poisoned` before panicking. [ADR 009]
15. **Violation paths** leading to an invariant-triggered panic should emit a structured `error!` event containing physical state values (Halt Forensics).
16. **External error responses** should use a neutral "Statement of Fact" tone, avoiding fault attribution or internal state leakage (Secure Clinical). [ADR 006]
17. **PII redaction** (e.g., Client ID truncation, sensitive payload justifications) should be strictly enforced. [ADR 010]

## Phase 4: Persistence & Consensus Domain

18. **Consensus timers** (heartbeat and election) should be strictly decoupled from external I/O or policy resolution to guarantee cluster liveness.
19. **Raft core state mutations** should be followed by an explicit, synchronous disk `flush()` before responding to an RPC.
20. **The Session Table** should deduplicate intents and guarantee linearizable replays to enforce Exactly-Once Semantics (EOS). [ADR 006]
21. **Mutation outcomes** (both Approvals and Vetoes) should be recorded as first-class, verifiable consensus events to ensure ledger continuity. [ADR 006]

## Phase 5: Engineering Standards & Observability

22. **Clinical events** should use structured `tracing` spans and events (including `trace_id`, `term`, `index`, and `client_id`). [ADR 010]
23. **Telemetry targets** should utilize the `ClinicalTarget` registry instead of raw string literals.
24. **Module-level docstrings** should be present, providing a concise architectural summary of the file's clinical role.
25. **Implementation documentation** should be technically accurate, complete, and maintained in a professional tone.
26. **Non-trivial logic** should be verified via the mandatory nested BDD-style module hierarchy.
27. **Imports** should be preferred over fully-qualified names (FQN) to maintain brevity and readability.
28. **Code artifacts** should compile without clippy warnings and align with `cargo +nightly fmt` (zero-diff).

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
