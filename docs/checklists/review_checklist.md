# Lact-O-Sensus Code Review Checklist

## Instruction to the Reviewing LLM

You will code review a given subject. First, read this checklist completely. Evaluate the provided source code strictly against these rules. For every violation discovered, output a structured ticket containing:

- **Rule ID**: The alphanumeric tag violated.
- **Severity**: CRITICAL, WARNING, or STYLE.
- **Location**: The specific file and code section.
- **Explanation**: A concise description of the issue.
- **Suggested Fix**: Refactored code complying with the rule.

If you discover a structural or behavioral issue that is NOT covered by the checklist below, still report it using the same ticket format with a descriptive placeholder Rule ID (e.g., `[CUSTOM-01]`). Clearly state that it falls outside the defined checklist.

---

## 1. Architecture & Structural Boundaries [ARCH]

### [ARCH-01] Domain Agnosticism

- **Target Scope**: `crates/raft-engine`
- **Severity**: CRITICAL
- **DO**: Keep the consensus engine strictly generic and focused solely on replication and leadership.
- **DO NOT**: Introduce grocery-specific domain logic, schemas, or application-level rules into the Raft implementation.

### [ARCH-02] Identity Isolation Interceptors

- **Target Scope**: gRPC Ingress (`crates/gateway`, `crates/common/src/rpc.rs`)
- **Severity**: CRITICAL
- **DO**: Wrap any new gRPC services with the `IdentityInterceptor` to enforce validation of `cluster_id` and `target_node_id` via centralized middleware [ADR 004, ADR 005].
- **DO NOT**: Implement identity validation manually inside individual RPC handler endpoints or rely on insecure defaults.

### [ARCH-03] Deferred Processing

- **Target Scope**: Request Handlers
- **Severity**: WARNING
- **DO**: Defer heavy logical operations (such as loading or parsing full inventory states) until after the structural intent validation is successful.
- **DO NOT**: Execute blocking or expensive computations prior to validating the legitimacy and identity of an incoming request.

### [ARCH-04] Protobuf Purity

- **Target Scope**: `crates/common/proto/app.proto`, `raft.proto`
- **Severity**: CRITICAL
- **DO**: Maintain pure message contracts focusing only on the data payload.
- **DO NOT**: Redundantly include metadata attributes like `cluster_id` within the Protobuf message structures if they are managed and validated via gRPC ingress metadata [ADR 005].

---

## 2. Data Integrity & Foundational Types [DATA]

### [DATA-01] Primitive Obsession Prevention

- **Target Scope**: Domain Models, `crates/common/src/types/`
- **Severity**: CRITICAL
- **DO**: Use self-validating NewTypes for all domain identifiers (e.g., `NodeId`, `LogIndex`, `Term`, `ClientId`) [ADR 004]. Instantiate gRPC messages exclusively via NewType-aware factories (`new()`).
- **DO NOT**: Pass or store identifiers using raw primitive types like `u64` or `String` across module boundaries.

### [DATA-02] Physical Quantity Normalization

- **Target Scope**: `UnitRegistry`, Physical State
- **Severity**: CRITICAL
- **DO**: Normalize all physical quantities to SI base units (`g` or `ml`) using `rust_decimal::Decimal` and explicitly enforce Banker's Rounding (Half-to-Even) for stability [ADR 008].
- **DO NOT**: Use native IEEE 754 floating-point types (`f32`, `f64`) for physical measurements, or implement custom/unstable rounding mechanisms.

### [DATA-03] Cross-Architecture Determinism

- **Target Scope**: DTOs, Serialization
- **Severity**: CRITICAL
- **DO**: Transmit physical state values as stringified fixed-point decimals before serializing or logging them.
- **DO NOT**: Transmit structural floating-point representations over the wire where parsing behavior could differ across platforms.

### [DATA-04] Dimensional Invariance

- **Target Scope**: Domain Logic
- **Severity**: CRITICAL
- **DO**: Enforce Dimensional Invariance to ensure arithmetic only occurs between compatible physical measurements.
- **DO NOT**: Allow logic that bypasses the "Dimensional Fence", permitting cross-dimensional addition or subtraction (e.g., adding `ml` to `g`) [ADR 008].

### [DATA-05] Defensive Mutation Lifecycle

- **Target Scope**: `Gateway`, `ai-veto`
- **Severity**: CRITICAL
- **DO**: Apply Layer 2 Syntactic Scrubbing (`trim()`, `to_lowercase()`) on new user input. Verify AI-provided metadata (categories/units) against hardcoded system registries before applying.
- **DO NOT**: Forward raw, unscrubbed input to the AI Oracle or implicitly trust AI-generated dimensions without registry verification [ADR 007].

---

## 3. Clinical Safety & Reliability [SAFE]

### [SAFE-01] Panic Avoidance

- **Target Scope**: Production Code
- **Severity**: CRITICAL
- **DO**: Handle missing values and errors gracefully using `Result` or `Option`.
- **DO NOT**: Use `unwrap()` or `expect()` in production paths.

### [SAFE-02] Error Handling Strategy

- **Target Scope**: Error Enums
- **Severity**: CRITICAL
- **DO**: Use `thiserror` for domain-specific enums within core library logic. Limit the `anyhow` crate strictly to top-level binary entry points (`main.rs`).
- **DO NOT**: Use opaque `anyhow::Result` types within reusable library crates or traits.

### [SAFE-03] Invariant Violation Handling (Poison-then-Panic)

- **Target Scope**: `lacto-fsm`, `raft-engine`
- **Severity**: CRITICAL
- **DO**: Transition the logical state to `Poisoned` and emit a structured `error!` event containing Halt Forensics before crashing the thread on invariant violations. Use the `delegate_to_inner!` macro to guarantee immediate panic on poisoned states [ADR 009].
- **DO NOT**: Panic directly without poisoning the state, or allow a node to continue processing after detecting structural corruption.

### [SAFE-04] Halt Forensic Context

- **Target Scope**: Invariant Violations (`apply_fatal`)
- **Severity**: WARNING
- **DO**: Include detailed, structured context (e.g., `peer_id`, `log_index`, `term`, or specific error variants) in the error messages passed to `apply_fatal` to enable deterministic forensic reconstruction [ADR 009].
- **DO NOT**: Use generic or opaque error messages (e.g., "protocol error") that fail to identify the specific state coordinates at the moment of failure.

### [SAFE-05] Bootstrap Integrity

- **Target Scope**: Node Initialization
- **Severity**: CRITICAL
- **DO**: Enforce the Bootstrap Halt Mandate by panicking proactively on startup if the node's persisted disk identity does not match its loaded `.toml` configuration [ADR 004].
- **DO NOT**: Automatically overwrite or ignore identity mismatches during node bootstrapping.

### [SAFE-06] Lock-Signal Atomicity

- **Target Scope**: `ConsensusShell`
- **Severity**: CRITICAL
- **DO**: Utilize the `MutationGuard` to ensure that reactive state changes (`ConsensusProgress`) are broadcast before the lock is released [ADR 009].
- **DO NOT**: Modify internal consensus state and drop the lock before signaling dependent watchers, risking race conditions.

### [SAFE-07] Concurrency Safety

- **Target Scope**: AI Resolution, Mutation Pipelines
- **Severity**: CRITICAL
- **DO**: Hold `MutationLock` sequentially during Layer 3 (AI Resolution) and Layer 4 (Postprocess) to maintain ordering [ADR 007].
- **DO NOT**: Allow concurrent evaluation pipelines to interleave and regress the deterministic ordering of mutation intents.

### [SAFE-08] Async Temporary Guard Scoping

- **Target Scope**: All async code holding `tokio::sync::RwLock` / `Mutex` guards
- **Severity**: CRITICAL
- **DO**: Extract lock guard dereferences into named local variables before passing them into async function calls that may re-acquire the same lock.
- **DO NOT**: Inline expressions like `state.read().await.field()` inside the arguments of an async function that may re-acquire the same lock. The temporary `RwLockReadGuard` lives until the end of the enclosing statement (the semicolon), which spans the `.await` boundary of the outer call, causing a deadlock on a `current_thread` runtime.

### [SAFE-09] Client Intent Durability

- **Target Scope**: `client-cli`
- **Severity**: CRITICAL
- **DO**: Durably log client state mutations to the local WAL (`IntentWal`) prior to network dispatch to handle crash-recovery reliably [ADR 001].
- **DO NOT**: Send mutations to the cluster while keeping them solely in volatile memory.

### [SAFE-10] Opaque Clinical Reporting

- **Target Scope**: External APIs
- **Severity**: CRITICAL
- **DO**: Provide external error responses using a neutral "Statement of Fact" tone [ADR 006].
- **DO NOT**: Expose fault attribution, stack traces, or internal state leaks in external API error payloads (Secure Clinical).

### [SAFE-11] Client Redirection

- **Target Scope**: Follower/Candidate Nodes
- **Severity**: CRITICAL
- **DO**: Reject external mutations and queries on non-leader nodes by returning a valid `leader_hint` payload [ADR 002].
- **DO NOT**: Process queries silently on non-leaders or drop requests without guiding the client to the active leader.

---

## 4. Persistence & Consensus Domain [RAFT]

### [RAFT-01] Liveness Decoupling

- **Target Scope**: Consensus Core
- **Severity**: CRITICAL
- **DO**: Decouple consensus timers (heartbeat and election) strictly from external I/O or AI policy resolution.
- **DO NOT**: Hold the primary consensus lock (`MutationGuard` / `state.write()`) while awaiting slow disk reads, remote RPC calls, or State Machine application, as this stalls the ticking Raft heartbeat loop and causes cluster instability.

### [RAFT-02] Liveness Configuration

- **Target Scope**: Configuration Parsers
- **Severity**: CRITICAL
- **DO**: Statically enforce the 1:3 to 1:6 heartbeat-to-election timeout ratio configuration [ADR 003].
- **DO NOT**: Allow cluster nodes to boot with configurations mathematically incapable of stable elections.

### [RAFT-03] Synchronous Persistence

- **Target Scope**: Storage Layer (`sled`)
- **Severity**: CRITICAL
- **DO**: Follow Raft core state mutations with an explicit, synchronous disk `flush()` before acknowledging an RPC request.
- **DO NOT**: Reply to peer nodes using un-flushed, volatile state approximations.

### [RAFT-04] Monotonic Sequences

- **Target Scope**: State Machine
- **Severity**: CRITICAL
- **DO**: Ensure all sequence ID evaluations remain strictly monotonic and gapless (`Sequence N+1`) [ADR 006].
- **DO NOT**: Accept out-of-order execution or jump sequence numbers arbitrarily.

### [RAFT-05] Exactly-Once Semantics (EOS)

- **Target Scope**: `lacto-fsm`
- **Severity**: CRITICAL
- **DO**: Deduplicate intents via the Session Table to guarantee linearizable replays [ADR 006].
- **DO NOT**: Allow the same `client_id` + `sequence_id` combination to apply multiple mutations to the physical inventory.

### [RAFT-06] Ledger Continuity

- **Target Scope**: Consensus Core
- **Severity**: CRITICAL
- **DO**: Record all AI evaluation outcomes (both Approvals and Vetoes) as first-class, verifiable consensus events [ADR 006].
- **DO NOT**: Silently drop vetoed intents without establishing a ledger trail.

### [RAFT-07] Stateful Temporal Determinism

- **Target Scope**: State Machine
- **Severity**: CRITICAL
- **DO**: Update the state machine's `last_effective_time` monotonically using `max(event_time, current_effective)` upon applying every entry [ADR 006].
- **DO NOT**: Trust local node wall-clocks for evaluating time-dependent state changes.

### [RAFT-08] Freeze-Apply Invariance

- **Target Scope**: Snapshot Generation
- **Severity**: CRITICAL
- **DO**: Explicitly toggle the `is_snapshotting` flag (or equivalent structural lock) before initiating FSM serialization to prevent concurrent mutations from the apply pipeline [ADR 011].
- **DO NOT**: Serialize the state machine for background snapshotting or peer replication without ensuring the state is "frozen" against logical mutations.

### [RAFT-09] Asynchronous Compaction

- **Target Scope**: Snapshot Handlers
- **Severity**: CRITICAL
- **DO**: Offload heavy FSM serialization (`StateMachine::snapshot`) and deserialization (`StateMachine::install_snapshot`) to background blocking thread pools (e.g., `tokio::task::spawn_blocking`) [ADR 011].
- **DO NOT**: Execute heavy gigabyte-scale disk writes directly on the main async Raft loop.

### [RAFT-10] Crash-Safe Restoration

- **Target Scope**: Log Compaction
- **Severity**: CRITICAL
- **DO**: Guard any destructive wipe of physical state during snapshot installation with the 'Restoration Tombstone' protocol (`KEY_RESTORE_IN_PROGRESS`), explicitly flushed to disk prior to deletion [ADR 011].
- **DO NOT**: Delete foundational state files without writing a recovery tombstone.

---

## 5. Engineering Standards & Observability [ENG]

### [ENG-01] Method Orchestration & Readability

- **Target Scope**: All Source Files
- **Severity**: STYLE
- **DO**: Act as high-level orchestrators in major functions, delegating details to specialized sub-functions. Arrange code in a logically ordered, top-down reading pattern.
- **DO NOT**: Pack dense procedural lines into a single primary controller or scatter target helper methods randomly throughout the file.

### [ENG-02] Clinical Telemetry

- **Target Scope**: Observability Layer
- **Severity**: CRITICAL
- **DO**: Use structured `tracing` spans and events. Truncate Client IDs and strictly redact sensitive AI payloads to `TRACE` levels. Act as the authoritative generator of `trace_id`s (UUIDv7) at the Gateway ingress. Utilize the `ClinicalTarget` registry for namespaces [ADR 010].
- **DO NOT**: Rely on unstructured `println!` or raw string literals. Do not trust client-provided `x-trace-id` headers for correlation.

### [ENG-03] Clinical Documentation

- **Target Scope**: Source Code
- **Severity**: STYLE
- **DO**: Provide module-level docstrings with a concise architectural summary. Maintain a technically accurate, professional tone.
- **DO NOT**: Submit functional code changes lacking rationale or inline documentation for edge cases.

### [ENG-04] Behavioral Verification

- **Target Scope**: All Source Files (`mod tests`)
- **Severity**: WARNING
- **DO**: Verify non-trivial logic via the mandatory nested BDD-style module hierarchy.
- **DO NOT**: Structure test classes as a flat, unorganized list of testing assertions.

### [ENG-05] Import Conventions

- **Target Scope**: All Source Files
- **Severity**: STYLE
- **DO**: Prefer idiomatic `use` statements over fully-qualified names (FQN) to maintain brevity.
- **DO NOT**: Clutter execution logic with deeply nested, repeated paths.

### [ENG-06] Artifact Cleanliness

- **Target Scope**: CI/CD Pipeline
- **Severity**: CRITICAL
- **DO**: Compile without clippy warnings and align perfectly with `cargo +nightly fmt`.
- **DO NOT**: Merge code containing unresolved lint warnings or formatting diffs.

### [ENG-07] Async Function Discipline

- **Target Scope**: All Source Files
- **Severity**: WARNING
- **DO**: Declare functions `async fn` only when they contain at least one `.await` on a genuinely asynchronous operation (network I/O, tokio timer, channel, `RwLock`/`Mutex` acquisition).
- **DO NOT**: Mark a function `async fn` if its body contains no `.await` calls, or if all its `.await` calls chain to functions that are themselves zero-async and perform only synchronous blocking I/O (sled operations, protobuf encoding, file I/O). Such functions should be synchronous (`fn`) and, when the calling context requires offloading, explicitly wrapped in `tokio::task::spawn_blocking` at the offload boundary.

---

## Appendix: BDD-Style Test Arrangement Example

All non-trivial logic MUST follow this structural pattern:

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
