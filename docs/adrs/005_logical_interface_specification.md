# ADR 005: System-Wide Logical Interface Specification

## Metadata

- **Date:** 2026-04-09
- **Status:** Accepted
- **Scope:** Protobuf interface boundary (consensus vs. application), identity metadata enforcement via gRPC interceptors, ledger entry format (Absolute State, stringified decimals). Excludes specific field-level protobuf definitions (defined in `.proto` files), wire protocol encoding details, gRPC channel configuration, and serialization library selection.
- **Primary Goal:** Define a consistent, typed interface for all inter-node communication, ensuring cluster isolation and semantic integrity.
- **Last Updated:** 2026-06-09

## Context

Lact-O-Sensus consists of three distinct interaction domains: internal consensus (Raft peer-to-peer), external user ingress (client-to-leader), and policy egress (leader-to-AI). Each domain has different contract requirements:

- **Cluster isolation** (ADR 004): Every gRPC request must carry `cluster_id` and `target_node_id` to prevent cross-environment contamination. This identity must be enforced at the interceptor layer, before application logic executes.
- **Domain agnosticism** (ADR 001, ADR 003): The consensus engine must be reusable across state machine implementations — it replicates opaque payloads and must not depend on grocery-specific schemas.
- **AI semantic resolution** (ADR 007): The AI Veto node receives normalized intents and returns semantic metadata (resolved item key, category, unit). These AI responses must enter the Raft log deterministically via the Leader (per ADR 002).
- **SI normalization** (ADR 008): All physical quantities are normalized to SI base units. The ledger must record absolute results as stringified fixed-point decimals to prevent IEEE 754 non-determinism across architectures.

Without a clean interface boundary, the consensus engine would become coupled to the grocery domain, preventing reuse and making it impossible to evolve the application schema without touching the Raft core.

## Options Considered

### Option A: Single Unified Protobuf

Define all RPCs and data structures in a single `.proto` file — consensus, ingress, policy, and ledger.
- **Complexity**: Lowest — one schema to maintain; no cross-file coordination.
- **Reusability**: None — the consensus services are coupled to grocery-specific types; cannot be reused for other state machine projects.
- **Schema evolution**: Harder — a change to any domain type requires regenerating the entire protobuf, including the consensus stubs.
- **Verdict**: Rejected — violates ADR 001's domain-agnosticism requirement for the consensus engine.

### Option B: Split Protobuf + Metadata Identity (Chosen)

Two `.proto` files: `raft.proto` (domain-agnostic consensus) and `app.proto` (grocery-specific ingress, policy, ledger). Identity flows through gRPC metadata headers, enforced at the interceptor layer.
- **Reusability**: Strong — `raft.proto` contains no grocery types; the consensus engine is a portable black box.
- **Complexity**: Moderate — requires interceptor middleware and cross-file coordination for schema evolution.
- **Safety**: Strong — identity enforcement is centralized in middleware, not scattered across handlers.
- **Verdict**: Chosen — balances reusability, safety, and evolutionary flexibility.

### Option C: Identity in Message Body + Single Protobuf

Identity fields (`cluster_id`, `target_node_id`) are carried inside every protobuf message rather than in gRPC metadata. All services live in one `.proto` file.
- **Reusability**: Weak — identity fields pollute every message; the consensus protobuf cannot be reused without carrying grocery-identity baggage.
- **Complexity**: Moderate — no interceptor needed, but each handler must manually validate identity.
- **Safety**: Weak — identity validation is scattered across handlers; a new handler could forget to check, creating a cross-contamination gap.
- **Verdict**: Rejected — weaker safety guarantees and no reuse benefit over Option B.

### Option D: Do Nothing

Continue without defined interface boundaries; consensus and application types coexist in shared protobuf definitions.
- **Reusability**: None — the current implementation is inherently coupled.
- **Complexity**: Lowest — no refactoring needed.
- **Safety**: Low — no systematic identity enforcement; each handler implements its own checks (or omits them).
- **Verdict**: Rejected — unacceptable for a clinical-grade system.

## Decision

We will define three logical services with strict contracts, decoupled into two distinct protobuf definitions to separate the generic consensus engine from the grocery application logic.

### Assumptions & Constraints

- **Crash-Recovery model** (per ADR 001): Nodes may stop and restart; interface contracts must survive log replay across restarts.
- **Cluster size ≤ 7 nodes**: Interface overhead (double serialization, metadata injection) is acceptable for small clusters.
- **gRPC transport** (per ADR 002): All inter-node communication uses gRPC; identity metadata flows through gRPC headers.
- **Stringified decimals** (per ADR 008): All physical quantities must be transmitted as stringified fixed-point decimals to avoid IEEE 754 cross-architecture non-determinism.
- **Absolute State recording**: The ledger records absolute results (not deltas) to ensure state machine idempotency.
- **Forward compatibility**: Ledger entries are persistent; schema changes must support backward compatibility (e.g., protobuf optional fields) to allow older logs to be replayed by newer binaries.

### 1. Identity Enforcement via gRPC Metadata

Every gRPC request MUST carry identity (`cluster_id`, `target_node_id`) as gRPC metadata headers, not in the protobuf message body. This ensures that identity enforcement is transparent to individual service implementations and prevents cross-environment contamination (per ADR 004).

- **Inbound:** A common interceptor validates both headers against the local node's identity before the request reaches any handler. Mismatched or missing headers are rejected immediately.
- **Outbound:** The same interceptor injects both headers into every outbound RPC — internal consensus calls, client-to-leader calls, and leader-to-AI policy calls.

### 2. Consensus Interface — Domain-Agnostic (`raft.proto`)

Used exclusively for Raft peer-to-peer communication. This interface MUST NOT contain any application-level types — it replicates opaque byte payloads only. It defines the standard Raft RPCs:

- **RequestVote:** Exchanged during leader election. Carries the candidate's term and log metadata; returns the responder's term and grant status.
- **AppendEntries:** Used for log replication and heartbeats. Carries the leader's term, log entries (as opaque bytes), and commit index.
- **InstallSnapshot:** Transfers a full state snapshot to a lagging follower. Carries the snapshot metadata and data payload (opaque bytes).
- **LogEntry:** A generic container holding `(term, index, data)` where `data` is opaque bytes containing a serialized application-level entry.

The exact field definitions for these RPCs follow the Raft paper (Ongaro & Ousterhout, USENIX ATC 2014) and are specified in `crates/common/proto/raft.proto`.

### 3. Application Interface — Domain-Specific (`app.proto`)

Used for external client ingress, policy egress, and the internal state machine representation. The exact field definitions are specified in `crates/common/proto/app.proto`.

#### A. Ingress Service (Client-to-Leader)

Provides mutation and query operations. Every request carries the client's identity (`client_id`) and a monotonic `sequence_id` for exactly-once deduplication (per ADR 006).

- **ProposeMutation:** Accepts a `MutationIntent` (the user's raw input). Returns the committed result status (Committed, Rejected, or Vetoed), the new state version, and a `leader_hint` for redirection if the target node is not the leader.
- **QueryState:** Returns the current inventory state, optionally filtered. Supports a minimum state version for read-after-write consistency.

#### B. Policy Service (Leader-to-AI)

Used exclusively by the Raft Leader (per ADR 002) to request semantic resolution of a mutation intent. Identity flows through gRPC metadata.

- **EvaluateProposal:** Accepts a normalized intent and the current inventory snapshot. Returns:
  - An approval decision with moral justification.
  - Semantic metadata: resolved canonical item key, suggested display name, category assignment, and resolved SI unit.
  - A conversion multiplier (as a stringified fixed-point decimal) to normalize the user's quantity to the base SI unit.

#### C. Replicated Ledger Entry

The serialized binary stored as opaque bytes within the Raft `LogEntry`. This is the single source of truth for all inventory state. The entry MUST record the absolute result of the mutation (not the delta) to ensure idempotent replay. It contains the following categories of data:

- **Identity:** The canonical item key (stable slug) for deduplication across mutations.
- **Display:** UI-facing metadata (display name, category label).
- **State:** The updated absolute quantity in SI base units (as a stringified fixed-point decimal), the canonical SI unit symbol, and the user's preferred display unit.
- **Session:** The originating `(client_id, sequence_id)` pair for exactly-once deduplication (per ADR 006).
- **Audit:** The original raw user input, the AI's moral justification, and the event timestamp — providing full causal history for every state change.
- **Control:** The outcome status (Approved or Vetoed) and a deletion flag.

## Rationale

- **Protocol Reusability:** By splitting the protobuf definitions, the consensus engine (`raft.proto`) contains zero grocery-specific types. This ensures the Raft implementation can be reused for any distributed state machine project, consistent with ADR 001's domain-agnosticism mandate. The opaque `bytes` payload in `LogEntry` is the standard decoupling mechanism described in the Raft paper (Ongaro & Ousterhout, USENIX ATC 2014, §5.3).
- **Identity Centralization** (per ADR 004): Enforcing identity via gRPC metadata interceptors rather than per-handler validation guarantees that no new RPC handler can accidentally omit identity checks. This is a defense-in-depth measure — the interceptor acts as a single validation point before any application logic executes. If identity were in the message body, every handler would need to independently validate it; a missed check would create a cross-contamination vulnerability.
- **Contractual Clarity:** The split creates two independent schemas with different evolution cycles — the consensus schema changes only when the Raft protocol changes, while the application schema evolves with the grocery domain. This prevents a grocery schema change from forcing regeneration of consensus stubs.
- **Separation of Concerns** (per ADR 007, ADR 008): The `MutationIntent` captures human-ambiguous input (an unstructured string like "2 gallons of milk"), while the `CommittedMutation` captures the deterministic resolution after AI processing and SI normalization. This separation maps directly to the Defense Onion pipeline (ADR 007): raw input enters at Layer 2 (Syntactic Scrubbing), passes through AI resolution (Layer 3), and emerges as an absolute SI-quantified state (Layer 4). Recording the absolute result (not a delta) ensures that replaying the log is idempotent — applying the same `CommittedMutation` twice yields the same state, regardless of the order of other entries (per ADR 006).

## Consequences

### Pros

- **Engine Portability:** The Raft implementation is now 100% domain-agnostic and reusable.
- **Parallel Evolution:** The consensus protocol and the grocery schema can evolve independently.
- **Auditability:** Every ledger entry carries its complete causal history (raw input + AI reasoning).

### Cons

- **Serialization Overhead:** Mutations require "Double Serialization" (Application -> Bytes -> Raft LogEntry) and corresponding deserialization on followers.
- **Rigidity:** Schema changes across the split boundary require coordinated updates across all node types.

### Operational Impact

- **Schema Evolution:** Since the ledger format (`CommittedMutation`) is persistent, any changes to the logical interface must support backward compatibility (e.g., Protobuf optional fields) to allow older logs to be replayed by newer binaries.
- **Observability:** The inclusion of audit metadata (original intent and AI rationale) significantly simplifies debugging but requires monitoring for log-induced disk pressure.
- **Protocol Drift:** Strict validation of `cluster_id` and `node_id` simplifies the isolation of environmental issues but requires rigorous configuration management during cluster deployment.

## Follow-Up

- **Protobuf specification audit:** Verify that `raft.proto` contains no application-level types and `app.proto` contains no consensus types. Enforce via code review checklist.
- **Schema evolution policy:** Document the backward-compatibility requirements for `CommittedMutation` — new fields MUST be protobuf optional; existing fields MUST NOT be removed or repurposed.
- **Log compaction monitoring:** Add disk-pressure alerting for the Raft log, informed by the audit metadata size in each `CommittedMutation` entry.
- **Interface boundary review:** Revisit the split boundary if a new service domain is added (e.g., external marketplace integration) that could benefit from its own protobuf file.
