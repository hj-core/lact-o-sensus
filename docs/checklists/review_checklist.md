# Lact-O-Sensus Code Review Checklist

This checklist is derived from the project's Architectural Decision Records (ADRs) and the technical mandates. Use the specific rule numbers when citing violations during code reviews.

1. The **`raft-engine`** should be domain-agnostic and free of grocery-specific logic.
2. **Major functions** should act as high-level orchestrators, delegating implementation details to specialized sub-functions.
3. **Source code layout** should be logically ordered to support top-down readability.
4. **gRPC messages** should be instantiated exclusively via NewType-aware factories (`new()`) in `common/src/proto.rs`.
5. **Identity validation** (`cluster_id` and `target_node_id`) should be strictly enforced via centralized gRPC middleware. [ADR 004, ADR 005]
6. **Heavy operations** (e.g., reading the full inventory) should be deferred until after initial structural intent validation is complete.
7. **Identity Interceptors:** Verify that any new gRPC services are wrapped with the `IdentityInterceptor` to enforce isolation mandates. [ADR 004, ADR 005]
8. **Protobuf Purity:** Ensure new protobuf messages in `app.proto` do not redundantly include `cluster_id` if they are passed through the gRPC ingress. [ADR 005]
9. **Domain identifiers** (e.g., `NodeId`, `LogIndex`, `Term`, `ClientId`) should use self-validating NewTypes to avoid primitive obsession. [ADR 004]
10. **Physical quantities** should be normalized to SI base units (`g` or `ml`) using Banker's Rounding (Half-to-Even). [ADR 008]
11. **Physical state values** should be transmitted as stringified fixed-point decimals before being logged to ensure cross-architecture determinism.
12. **AI-provided metadata** (categories/units) should be verified against hardcoded system registries before finalization. [ADR 007]
13. **Unit Conversions:** Verify that any new unit conversions added to the `UnitRegistry` utilize `rust_decimal::Decimal` and explicitly enforce Banker's Rounding. [ADR 008]
14. **Dimensional Fence:** Ensure new physical dimensions do not bypass the Dimensional Invariance logic, prohibiting cross-dimensional addition/subtraction. [ADR 008]
15. **Syntactic Scrubbing:** Check that any new user input fields undergo Layer 2 Syntactic Scrubbing (`trim()`, `to_lowercase()`) before being sent to the AI Oracle. [ADR 007]
16. **Production code** should avoid `unwrap()` or `expect()` entirely.
17. **Core library logic** should use `thiserror` for domain-specific enums.
18. **The `anyhow` crate** should be restricted exclusively to top-level `main.rs` binaries.
19. **Invariant violations** should trigger the `Poison-then-Panic` sequence, transitioning logical state to `Poisoned` before panicking. [ADR 009]
20. **Violation paths** leading to an invariant-triggered panic should emit a structured `error!` event containing physical state values (Halt Forensics).
21. **External error responses** should use a neutral "Statement of Fact" tone, avoiding fault attribution or internal state leakage (Secure Clinical). [ADR 006]
22. **PII redaction** (e.g., Client ID truncation, sensitive payload justifications) should be strictly enforced. [ADR 010]
23. **Crash-Recovery WAL:** Client state mutations MUST be durably logged to the local WAL (`IntentWal`) prior to network dispatch to handle crash-recovery. [ADR 001]
24. **Leader Redirection:** Follower and Candidate nodes MUST reject external mutations and queries by providing a valid `leader_hint` for client redirection. [ADR 002]
25. **Bootstrap Halt Mandate:** Nodes MUST enforce the Halt Mandate by panicking on startup if their persisted identity on disk does not match their loaded configuration. [ADR 004]
26. **Lock-Signal Atomicity:** `ConsensusShell` mutations MUST utilize the `MutationGuard` to ensure that reactive state changes (`ConsensusProgress`) are broadcast before the lock is released. [ADR 009]
27. **Halt Mandate Enforcement:** Role-specific state transitions MUST exclusively use the `delegate_to_inner!` macro (or its mutable/async equivalents) to guarantee immediate panic upon encountering the `Poisoned` state. [ADR 009]
28. **Mutation Lock Sequentiality:** Ensure `MutationLock` is held sequentially during Layer 3 (AI Resolution) and Layer 4 (Postprocess) to prevent concurrent mutation regressions. [ADR 007]
29. **Consensus timers** (heartbeat and election) should be strictly decoupled from external I/O or policy resolution to guarantee cluster liveness.
30. **Raft core state mutations** should be followed by an explicit, synchronous disk `flush()` before responding to an RPC.
31. **The Session Table** should deduplicate intents and guarantee linearizable replays to enforce Exactly-Once Semantics (EOS). [ADR 006]
32. **Mutation outcomes** (both Approvals and Vetoes) should be recorded as first-class, verifiable consensus events to ensure ledger continuity. [ADR 006]
33. **Liveness Configuration:** Configuration parsers MUST statically enforce the 1:3 to 1:6 heartbeat-to-election ratio to guarantee cluster liveness. [ADR 003]
34. **Monotonic Sequences:** Verify that all sequence ID checks remain monotonic and gapless (Sequence N+1). [ADR 006]
35. **Monotonic Clock:** Ensure state machine `last_effective_time` is updated monotonically using `max(event_time, current_effective)` on every applied entry. [ADR 006]
36. **Snapshot Offloading:** Heavy FSM serialization (`StateMachine::snapshot`) and deserialization (`StateMachine::install_snapshot`) MUST be offloaded to a background blocking thread pool (e.g., `tokio::task::spawn_blocking`) to avoid blocking the main Raft event loop. [ADR 011]
37. **Crash-Safe Restoration:** Any destructive wipe of physical state during log compaction/snapshot installation MUST be guarded by the 'Restoration Tombstone' protocol (`KEY_RESTORE_IN_PROGRESS`), explicitly flushed to disk prior to data deletion. [ADR 011]
38. **Clinical events** should use structured `tracing` spans and events (including `trace_id`, `term`, `index`, and `client_id`). [ADR 010]
39. **Telemetry targets** should utilize the `ClinicalTarget` registry instead of raw string literals.
40. **Module-level docstrings** should be present, providing a concise architectural summary of the file's clinical role.
41. **Implementation documentation** should be technically accurate, complete, and maintained in a professional tone.
42. **Non-trivial logic** should be verified via the mandatory nested BDD-style module hierarchy.
43. **Imports** should be preferred over fully-qualified names (FQN) to maintain brevity and readability.
44. **Code artifacts** should compile without clippy warnings and align with `cargo +nightly fmt` (zero-diff).
45. **Trace Authority:** The Gateway / Ingress Firewall MUST act as the authoritative generator of `trace_id`s (UUIDv7). Any client-provided `x-trace-id` MUST be ignored to prevent Byzantine correlation grafting. [ADR 010]

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
