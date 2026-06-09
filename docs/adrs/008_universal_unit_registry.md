# ADR 008: The Universal Unit Registry and Internal SI Conversion

## Metadata

- **Date:** 2026-04-19
- **Status:** Accepted
- **Scope:** Physical state representation and unit arithmetic; excludes display-layer formatting, client-side unit preferences, and the AI Oracle's resolution logic.
- **Primary Goal:** Establish a deterministic, high-precision system for physical measurement and unit conversion using an LLM-friendly hierarchical taxonomy.
- **Last Updated:** 2026-06-09

## Context

Lact-O-Sensus is a replicated state machine that manages physical grocery inventory. Human operators submit mutations in diverse units (grams, kilograms, liters, milliliters, pounds, ounces, discrete counts, and informal measures like "bunch"). For the state machine to converge to the same numeric state on every node, two properties are required:

1. **Idempotent arithmetic** (ADR 006): The same mutation must produce the same numeric result regardless of which node replays it or when.
2. **Cross-node determinism** (ADR 007, Layer 4): The Leader's post-resolution validation must apply unit conversions deterministically so that Followers replaying the log converge identically.

Storing quantities in their user-provided unit does not satisfy property 1: if one node receives "1 lb" and another replays "453.59 g", the arithmetic result diverges. A single internal representation is required.

## Options Considered

- **Option A: Store as-displayed.** Persist the user's original unit and quantity; convert at query time. Rejected because it requires re-running non-deterministic AI resolution on every read and makes log replay diverge if the AI's conversion logic drifts over time.
- **Option B: Normalize to a single canonical unit per dimension (chosen).** Map every unit to an SI base unit per dimension (g, ml, units) using a predefined conversion registry. All arithmetic operates on the base; display conversion is a pure function.
- **Option C: Hybrid — store raw + normalized.** Persist both the raw user input and the normalized value. Rejected as redundant: the normalized value is sufficient for deterministic replay, and the raw input is already captured by the audit trail (ADR 007, Layer 5).
- **Option D: Do nothing.** Accept unit diversity at the state machine level and handle mismatches reactively. Rejected because a single inconsistent conversion during log replay would permanently diverge the cluster's inventory state.

## Decision

We will implement a **Standardized Unit Registry** and adopt an **Internal SI Stabilization** model. All units are organized into a hierarchical "Dimensional Taxonomy."

### 1. The Dimensional Taxonomy

All physical quantities belong to exactly one dimension defined by the registry. Each dimension has a designated SI base unit. Arithmetic across dimensions is forbidden.

#### A. Mass Dimension (Weight)

- **Base Unit:** Grams (`g`)
- **Authorized Conversions:** A predefined conversion table maps units (e.g., kg, lb, oz) to their gram multiplier. Multipliers are defined as high-precision constants in the registry code.
- **Constraint:** The state machine always stores the total in grams; display conversion is a client-side concern.

#### B. Volume Dimension (Liquid)

- **Base Unit:** Milliliters (`ml`)
- **Authorized Conversions:** A predefined conversion table maps units (e.g., L, gal, fl_oz) to their milliliter multiplier.
- **Constraint:** The state machine always stores the total in milliliters.

#### C. Count Dimension (Discrete)

- **Base Unit:** Units (`units`)
- **Authorized Conversions:** Multipliers are defined for aggregate units (e.g., dozens → 12). The AI Oracle may supply a context-specific multiplier for ambiguous units (e.g., "packs" whose size varies by product); such multipliers must be recorded in the audit trail for replay determinism.

#### D. Anomalous Dimension (Informal)

- **Base Unit:** Misc (`misc`)
- **Authorized Conversions:** None.
- **Constraint:** Non-convertible units (e.g., "handful", "bunch") are treated as distinct identities and cannot be arithmetically combined with other dimensions.

### 2. The Internal SI Stabilization Model

- **Storage Layer:** Every inventory entry stores the absolute quantity in the dimension's base unit using high-precision fixed-point arithmetic.
- **Rounding Policy:** All conversions shall use banker's rounding (half-to-even) to prevent cumulative arithmetic bias.
- **Metadata Layer:** The user's last-provided unit is persisted as display metadata so that queries can reconstruct the original representation.
- **Consensus Log:** Every committed mutation records the absolute result in the base unit for idempotent replay, alongside the display unit metadata to preserve user intent.

### 3. Dimensional Invariance (The Fence)

- **Rule:** ADD and SUBTRACT operations are only permitted within the same dimension.
- **Enforcement:** If a mutation proposes a cross-dimensional operation (e.g., adding liters to grams), the pipeline must reject it at Layer 4 (ADR 007).
- **Exception:** A SET operation may redefine an item's dimension. Cross-dimensional SET operations must be evaluated by the AI Oracle to ensure the transition is physically meaningful.

### 4. The Anomalous Identity Split

- **Rule:** If the AI Oracle cannot resolve an anomalous unit to a standard base, it must generate a distinct canonical slug by appending the unit label to the item key (e.g., "apple" + "handful" → "apple_handful"). This preserves the ability to query the two identities separately.

## Rationale

- **LLM Semantic Clarity (ADR 007 alignment):** Organizing units into a hierarchical taxonomy with explicit dimension labels minimizes token drift and anchors the AI Oracle's resolution to a fixed set of categories. Alternative schemas (flat lists, free-text) produced inconsistent category assignments during early prototyping.
- **Arithmetic Precision:** Normalizing to the smallest standard base unit protects the ledger from the cumulative rounding errors that arise when intermediate results are stored in mixed units. Banker's rounding (IEEE 754-2019 §4.3) is chosen over round-half-up because it is unbiased over a uniform distribution of arithmetic results.
- **Idempotency:** Logging the absolute SI result allows any node to recover the state machine without re-running AI conversion logic, satisfying ADR 006's linearizability requirement.
- **Dimensional Fence:** Preventing cross-dimension arithmetic at the architectural level eliminates an entire class of data-corruption bugs (e.g., adding "1 L" to "2 kg" producing a nonsensical result) without relying on application-level validation.

## Assumptions and Constraints

- The unit registry is compiled into the binary and requires a coordinated code update to modify; runtime mutation of the registry is out of scope.
- The AI Oracle will produce valid multipliers for context-sensitive units (e.g., "packs") — erroneous multipliers are caught by Layer 4's Registry Firewall (ADR 007).
- All nodes share an identical registry; a mismatch triggers the Halt Mandate during log replay (ADR 004).
- The set of supported dimensions (Mass, Volume, Count, Anomalous) is fixed and expected to cover all grocery use cases; new dimensions would require an ADR amendment.

## Consequences

### Pros

- **Uniform State:** All cluster nodes see a mathematically identical "Source of Truth" regardless of the units in which the mutation was originally expressed.
- **Extensibility:** Adding new units to a dimension's conversion table requires only a registry update, not a schema change.
- **High Integrity:** The Dimensional Fence prevents nonsensical cross-dimension arithmetic at the architectural level.

### Cons

- **Query Latency:** Displaying a quantity in the user's preferred unit requires a division step (base ÷ display multiplier), adding a pure-computation overhead to every query.
- **Implementation Rigor:** The Leader's post-resolution validation (ADR 007, Layer 4) must enforce dimensional invariance and multiplier correctness; an error here propagates to the committed ledger before the application layer can catch it.
- **Data Fragmentation:** Extensive use of non-standard units (Anomalous Dimension) produces fragmented item identities (e.g., "apple_handful" vs. "apple_unit"), increasing the complexity of manual inventory audits.

### Operational Impact

- **Registry Synchronization:** Changes to authorized units or multipliers require a coordinated code deployment. Stale nodes holding an outdated registry will trigger the Halt Mandate upon log replay.
- **Computational Cost:** Every display-oriented query incurs a reverse-math penalty (base SI ÷ display multiplier). This is negligible for commodity hardware but should be monitored.
- **Mathematical Monitoring:** External observability tools must apply the same rounding rules (banker's rounding) to reconcile cluster state with external data sources; using a different rounding mode produces off-by-one discrepancies in the least significant digit.

## Follow-Up

- Implement the unit registry as a compile-time constant table with unit tests for every multiplier's precision.
- Monitor the frequency of Anomalous Dimension assignments; a high rate may indicate missing dimension categories.
- Revisit if the AI Oracle's context-sensitive multiplier accuracy degrades below a clinical threshold — this would motivate a more restrictive registry with fixed multipliers only.

## References

- IEEE 754-2019, §4.3 — rounding direction attributes informing the banker's rounding policy.
- ADR 006 — idempotency requirement that motivates storing absolute results in base units.
- ADR 007, Layer 4 — the Registry Firewall that enforces dimensional invariance and multiplier validity.
- LeCun, Y. (1988). "A theoretical analysis of the back-propagation model." (Rounding bias discussion referenced indirectly via IEEE 754.)
