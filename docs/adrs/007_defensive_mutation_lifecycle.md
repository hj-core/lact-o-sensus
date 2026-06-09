# ADR 007: Defensive Mutation Lifecycle and Semantic Resolution

## Metadata

- **Date:** 2026-04-19
- **Status:** Accepted
- **Scope:** Mutation Request Lifecycle (Client, Leader, AI-Veto); excludes Gateway transport layer, Raft consensus internals, and client-side WAL.
- **Primary Goal:** Transform ambiguous human intent into immutable, deterministic consensus data via a multi-layered defensive pipeline.
- **Last Updated:** 2026-06-09

## Context

Lact-O-Sensus accepts natural-language inventory mutations from human operators. Human input is inherently ambiguous ("a few bags of apples"), and the AI Oracle is treated as a Byzantine component per ADR 001. These two sources of non-determinism demand a structured pipeline that sanitizes, resolves, and validates each intent before it reaches the replicated state machine.

Two preconditions constrain the design:

1. **Client identity is established before mutation** (ADR 004) — every mutation request carries a verifiable `(client_id, sequence_id)` pair.
2. **Exactly-once semantics are guaranteed by a replicated session table** (ADR 006) — the system must detect and absorb duplicate proposals without re-executing non-deterministic work.

Without a defensive pipeline, an ambiguous or malicious mutation could corrupt the inventory ledger before any human or automated check has a chance to intercept it.

## Options Considered

- **Option A: Client-only validation.** The client validates input before sending. Rejected because the client is outside the trust boundary; a compromised or buggy client could inject malformed data directly into consensus.
- **Option B: Single trusted validator.** A single server-side component handles all validation. Rejected because it creates a monolith that conflates syntactic correctness, semantic resolution, and physical consistency — violating separation of concerns and making each concern untestable in isolation.
- **Option C: Pass-through (no pipeline).** Raw natural language is committed directly and resolved lazily at read time. Rejected because it would require re-executing non-deterministic AI resolution on every read, breaking linearizability and making audit trails dependent on oracle availability.
- **Option D (chosen): 5-Layer Defense Onion.** Each layer addresses a distinct concern: structural, syntactic, semantic, physical, and consensus. Layers are independently testable, and a failure at any layer produces a deterministic veto rather than silent corruption.

## Decision

We will implement a **5-Layer Defensive Pipeline** for all mutation requests. A request must survive every layer to be committed. The physical behavior of quantities and units is governed by an external **Physical Invariant Policy** (ADR 008).

### Layer 1: The Client-Node (Structural Intent)

- **Responsibility:** Structural validation and session binding.
- **Logic:**
  - **Ambiguity Filter:** Rejects semantically conflicting commands (e.g., `DELETE` with a quantity) and prompts the user for specific intent (`SUBTRACT`).
  - **Session Tagging:** Injects persistent `client_id` and increments monotonic `sequence_id`.

### Layer 2: The Leader-Preprocess (Syntactic Fortress)

- **Responsibility:** Syntactic normalization and linearizable sequence control.
- **Logic:**
  - **Session Deduplication (CQRS):** Verifies `sequence_id` against the authoritative state machine. This decouples application-level standing from the generic consensus pipe. Returns the cached logical outcome for retries.
  - **Bootstrap Enforcement:** Rejects any initial connection from a new `client_id` that does not originate at the first sequence number.
  - **Syntactic Scrubbing:** Normalizes `item_key`, `unit`, and user-supplied `category` hints to a canonical form.
  - **Taxonomy Guard:** Validates that user-supplied category hints exist in the authorized registry; rejects unknown categories.
  - **Strict Serialization:** Acquires a leader-local lock to ensure that the AI resolution and consensus proposal for a specific item happen sequentially, preventing race conditions in quantity stabilization.

### Layer 3: The AI-Resolution (Semantic Oracle)

- **Responsibility:** Semantic mapping and moral evaluation.
- **Logic:**
  - **Identity Resolution:** Maps synonyms (e.g., "oj") to a unique Canonical Slug.
  - **Taxonomic Mapping:** Assigns items to the authorized taxonomy (overriding user hints if inaccurate).
  - **Unit Canonicalization:** Maps variations to symbols and provides Conversion Multipliers according to the Physical Invariant Policy.
  - **Conversion Priority:** The AI shall prioritize Standardized Conversion (SI or Units) whenever a reasonable heuristic exists (e.g., estimating the count in a "bunch"). Identity Splitting (ADR 008) must be reserved for cases where conversion would result in significant data loss or physical nonsense.
  - **Moral Verdict:** Evaluates the proposal against the inventory for health, scale, and context.
  - **Leader-Internal Retry (Best-Effort):** If the AI response is malformed or fails the subsequent Registry Firewall (Layer 4), the Leader may perform at most one automatic retry with the AI Node. If the second attempt also fails, the request must proceed to a definitive Veto.

### Layer 4: The Leader-Postprocess (Resolution & Finalization)

- **Responsibility:** Deterministic validation of AI-provided data and result finalization.
- **Logic:**
  - **Registry Firewall:** The Leader MUST verify AI-provided metadata against hardcoded system registries (ADR 008). Any proposal containing an unauthorized `category` or `unit` must be marked as Vetoed to prevent AI hallucinations.
  - **Physical Invariant Check:** Rejects conversions between incompatible dimensions (e.g., Mass to Volume); failures result in a Veto.
  - **Deterministic Math:** For approved mutations, calculates the final absolute quantity in the Internal Standardized Format using fixed-point arithmetic.
  - **Outcome Packaging:** The Leader constructs a final record containing the status (`APPROVED` or `VETOED`) and the moral justification.

### Layer 5: The Consensus-Commit (Immutable Fact)

- **Responsibility:** Distributed agreement and state machine application.
- **Logic:**
  - **Raft Replication:** Appends the outcome record to the WAL and replicates to Followers. The Raft engine remains domain-agnostic, treating the payload as opaque bytes.
  - **State Machine Application:** Upon commit, the state machine receives the outcome and updates the canonical inventory tree and the persistent Session Table to ensure contiguous linearizability.
  - **Conditional State Update:** Only entries with `APPROVED` status mutate the inventory; vetoed entries still update the Session Table to prevent replay.
  - **Cleanup:** Releases the leader-local lock.

## Rationale

- **Safety Over Liveness:** We prioritize a "Fail-Stop" model where malformed data is rejected before it can corrupt the grocery ledger. In a prototype, a single unit hallucination (e.g., interpreting "bag" as "gram") propagated through to committed state before a firewall was added — this ADR ensures no such error survives past Layer 4.
- **Architectural Purity:** The state machine boundary enforces a "Clean Architecture" separation. The consensus engine pushes committed facts to the application, ensuring that consensus logic never leaks into grocery validation.
- **Idempotency:** Storing the absolute result in the log ensures that nodes always recover to an identical state without re-running non-deterministic AI logic.
- **Linearizability:** Sequential processing via the leader-local lock ensures the AI Oracle always has a perfectly up-to-date view of the inventory — Option A (no lock) would allow interleaved proposals to observe stale state, producing race-condition-dependent outcomes.

## Assumptions and Constraints

- AI inference latency is the dominant bottleneck; the pipeline is designed around a single sequential AI call per mutation.
- The full inventory fits within the AI Oracle's context window. If this ceases to hold, Layer 3's identity and unit resolution may degrade.
- The cluster is small (≤7 nodes), so leader-local locking does not create a system-wide throughput bottleneck.

## Consequences

### Pros

- **High Data Integrity:** Zero risk of "Unit Mismatch" or "Duplicate Aliases" in the list.
- **Robust Recovery:** Simple log replay due to absolute-state entries in a standardized format.
- **Professional Auditability:** Every entry carries the raw input, the conversion rationale, and the AI justification.
- **Testability:** The state machine can be tested by feeding it raw entries independently of the consensus cluster.

### Cons

- **Throughput Latency:** Sequential AI processing limits the system to one concurrent mutation per cluster.
- **Audit Bloat:** Storing raw input and justification in every ledger entry increases the storage footprint compared to a "Quantity-Only" ledger.
- **Linear Context Decay:** Sending the full inventory as context to the AI Oracle creates an O(N) scaling bottleneck.
- **Complexity:** Requires sophisticated multi-layer validation logic.
- **Lock Contention:** The leader-local lock limits throughput to the latency of a single AI call.

### Operational Impact

- **Latency:** Mutation throughput is strictly bound by AI inference time due to sequential locking.
- **Inference Scaling:** Operators must monitor the token count of mutation requests. As the inventory grows, AI inference time will increase, directly reducing the cluster's maximum mutation throughput.
- **Availability:** Failure of the AI Veto Node halts all mutations; the system remains in a "Read-Only" state for queries.
- **Observability:** Audit metadata in the log allows operators to definitively diagnose semantic rejections and unit conversion errors.

## Follow-Up

- Monitor AI context window utilization to detect approaching limits before they cause resolution failures at Layer 3.
- Track per-layer retry and veto rates to identify which pipeline stage introduces the most friction.
- Evaluate parallelization strategies (e.g., per-item sharding) if lock contention becomes a bottleneck in larger clusters.

## References

- Raft paper (Ongaro & Ousterhout, 2014) — foundation for Layer 5's consensus protocol.
- Clean Architecture (Martin, 2017) — guiding principle for the state machine boundary.
- ADR 001 — Byzantine treatment of the AI Oracle motivates Layer 4's Registry Firewall.
- ADR 004 — client identity preconditions inform Layer 1's session tagging.
- ADR 006 — exactly-once semantics inform Layer 2's deduplication.
- ADR 008 — Physical Invariant Policy governs Layer 4's conversion validation.
