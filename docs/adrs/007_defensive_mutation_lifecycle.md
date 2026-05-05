# ADR 007: Defensive Mutation Lifecycle and Semantic Resolution

## Metadata

- **Date:** 2026-04-19
- **Status:** Proposed
- **Scope:** Mutation Request Lifecycle (Client, Leader, AI-Veto)
- **Primary Goal:** Transform ambiguous human intent into immutable, deterministic consensus data via a multi-layered defensive pipeline.
- **Last Updated:** 2026-05-05

## Context

In Lact-O-Sensus, the user provides grocery murations via natural language. Because human input is ambiguous and the AI Oracle is non-deterministic, we require a "Defense Onion"—a series of checkpoints that sanitize, resolve, and validate data as it moves toward the state machine.

## Decision

We will implement a **5-Layer Defensive Pipeline** for all mutation requests. A request must survive every layer to be committed. The physical behavior of quantities and units is governed by an external **Physical Invariant Policy** (ADR 008).

### Layer 1: The Client-Node (Structural Intent)

- **Responsibility:** Structural validation and session binding.
- **Logic:**
  - **Ambiguity Filter:** Rejects semantically conflicting commands (e.g., `DELETE` with a quantity) and prompts the user for specific intent (`SUBTRACT`).
  - **Session Tagging:** Injects persistent `client_id` and increments monotonic `sequence_id`.
- **Outbound:** `ProposeMutationRequest` (Raw strings).

### Layer 2: The Leader-Preprocess (Syntactic Fortress)

- **Responsibility:** Syntactic normalization and concurrency control.
- **Logic:**
  - **Deduplication:** Verifies `sequence_id` against the Session Table (ADR 006); returns the cached logical outcome (including `state_version`) for retries.
  - **Syntactic Scrubbing:** Performs `trim()` and `to_lowercase()` on `item_key`, `unit`, and user-supplied `category` hints.
  - **Taxonomy Guard:** Validates that user-supplied category hints exist in the authorized registry; rejects unknown categories.
  - **Strict Serialization:** Acquires a **Leader-Local MutationLock**. This lock is transient and exists only for the duration of the current leader's tenure. If the leader steps down or crashes, the lock is implicitly invalidated to prevent system deadlocks.
- **Outbound:** `EvaluateProposalRequest` (containing normalized intent and `current_inventory` context).

### Layer 3: The AI-Resolution (Semantic Oracle)

- **Responsibility:** Semantic mapping and moral evaluation.
- **Logic:**
  - **Identity Resolution:** Maps synonyms (e.g., "oj") to a unique **Canonical Slug**.
  - **Taxonomic Mapping:** Assigns items to the authorized taxonomy (overriding user hints if inaccurate).
  - **Unit Canonicalization:** Maps variations to symbols and provides **Conversion Multipliers** according to the Physical Invariant Policy.
  - **Conversion Priority:** The AI should prioritize Standardized Conversion (SI or Units) whenever a reasonable heuristic exists (e.g., estimating the count in a "bunch"). **Identity Splitting** (ADR 008) must be reserved for cases where conversion would result in significant data loss or physical nonsense.
  - **Moral Verdict:** Evaluates the proposal against the inventory for health, scale, and context.
  - **Leader-Internal Retry (Best-Effort):** If the AI response is malformed or fails the subsequent Registry Firewall (Layer 4), the Leader may perform **at most one** automatic retry with the AI Node. If the second attempt also fails, the request must proceed to a definitive **Veto**.
- **Outbound:** `EvaluateProposalResponse` (Resolved Key, Unit, Category, Multiplier, Verdict).

### Layer 4: The Leader-Postprocess (Resolution & Finalization)

- **Responsibility:** Deterministic validation of AI-provided data and result finalization.
- **Logic:**
  - **Registry Firewall:** The Leader MUST verify AI-provided metadata against hardcoded system registries (ADR 008). Any proposal containing an unauthorized `category` or `unit` must be marked as **Vetoed** to prevent AI hallucinations.
  - **Physical Invariant Check:** Rejects conversions between incompatible dimensions (e.g., Mass to Volume); failures result in a **Veto**.
  - **Deterministic Math:** For approved mutations, calculates the final absolute quantity in the **Internal Standardized Format** using fixed-point arithmetic.
  - **Outcome Packaging:** The Leader constructs a `CommittedMutation` record containing the final status (`APPROVED` or `VETOED`) and the `moral_justification`.
- **Outbound:** Finalized ledger entry (The Unified Record).

### Layer 5: The Consensus-Commit (Immutable Fact)

- **Responsibility:** Distributed agreement and state machine application.
- **Logic:**
  - **Raft Replication:** Appends the outcome record (Success or Veto) to the WAL and replicates to Followers. The Raft engine remains **domain-agnostic**, treating the payload as opaque bytes.
  - **The State Machine Boundary:** Upon commit, the Raft engine calls the `apply` method on the `StateMachine` trait.
  - **Universal Session Update:** The `LactoStore` updates the persistent **Session Table** for every entry, recording the `sequence_id` and the outcome to ensure contiguous linearizability and support client-side deduplication.
  - **Conditional State Update:** ONLY for entries with `APPROVED` status:
    - **Internal State Stabilization**: Applies the absolute numeric result to the inventory tree.
    - **State Convergence**: The `resolved_item_key` (Canonical Slug) acts as the **exclusive Primary Key**.
    - **Metadata Evolution (LWW)**: Mutable metadata (e.g., `category`, `display_name`) is updated to match the latest committed entry.
  - **Cleanup:** Releases the `MutationLock`.
- **Outbound:** `MUTATION_STATUS_COMMITTED` or `MUTATION_STATUS_VETOED` (including `state_version`) to the client.

## Rationale

- **Safety Over Liveness:** We prioritize a "Fail-Stop" model where malformed data is rejected before it can corrupt the grocery ledger.
- **Architectural Purity:** Using a `StateMachine` trait enforces a "Clean Architecture" boundary. The Raft engine "pushes" committed facts to the application, ensuring that consensus logic never leaks into grocery validation.
- **Idempotency:** Storing the absolute result in the log ensures that nodes always recover to an identical state without re-running non-deterministic logic.
- **Linearizability:** Sequential processing via the `MutationLock` ensures the AI Oracle always has a perfectly up-to-date view of the inventory.

## Consequences

### Pros

- **High Data Integrity:** Zero risk of "Unit Mismatch" or "Duplicate Aliases" in the list.
- **Robust Recovery:** Simple log replay due to absolute-state entries in a standardized format.
- **Professional Auditability:** Every entry carries the raw input, the conversion rationale, and the AI justification.
- **Testability:** The `LactoStore` (the State Machine) can be tested by feeding it raw bytes independently of the Raft cluster.

### Cons

- **Throughput Latency:** Sequential AI processing limits the system to one concurrent mutation per cluster.
- **Complexity:** Requires sophisticated state machine logic to handle multi-layered validation and the decoupling of traits.
- **Lock Contention:** The `MutationLock` limits throughput to the latency of a single AI call.

## Operational Impact

- **Latency:** Mutation throughput is strictly bound by AI inference time due to sequential locking.
- **Availability:** Failure of the AI Veto Node halts all mutations; the system remains in a "Read-Only" state for queries.
- **Observability:** Audit metadata in the log allows operators to definitively diagnose semantic rejections and unit conversion errors.
