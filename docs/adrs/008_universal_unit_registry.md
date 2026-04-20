# ADR 008: The Universal Unit Registry and Internal SI Conversion

## Metadata

- **Date:** 2026-04-19
- **Status:** Proposed
- **Scope:** Physical State Representation and Unit Arithmetic
- **Primary Goal:** Establish a deterministic, high-precision system for physical measurement and unit conversion using an LLM-friendly hierarchical taxonomy.
- **Last Updated:** 2026-04-20

## Context

Lact-O-Sensus manages physical groceries measured in diverse units (g, kg, L, ml, lbs, units). To maintain a consistent and idempotent state machine, we adopt a "Universal Language" for internal storage (SI Units) while supporting user-friendly display preferences.

## Decision

We will implement a **Standardized Unit Registry** and adopt an **Internal SI Stabilization** model. All units are organized into a hierarchical "Dimensional Taxonomy."

### 1. The Dimensional Taxonomy

#### A. Mass Dimension (Weight)

- **Base Unit:** Grams (`g`)
- **Authorized Conversions:**
  - `kg` (Kilograms): Multiplier "1000"
  - `lb` (Pounds): Multiplier "453.59237"
  - `oz` (Ounces): Multiplier "28.34952"
- **Logic:** The State Machine always stores the total in `g`.

#### B. Volume Dimension (Liquid)

- **Base Unit:** Milliliters (`ml`)
- **Authorized Conversions:**
  - `L` (Liters): Multiplier "1000"
  - `gal` (Gallons): Multiplier "3785.41178"
  - `fl_oz` (Fluid Ounces): Multiplier "29.57353"
- **Logic:** The State Machine always stores the total in `ml`.

#### C. Count Dimension (Discrete)

- **Base Unit:** Units (`units`)
- **Authorized Conversions:**
  - `dozens`: Multiplier "12"
  - `packs`: Multiplier "1" (Default)
- **Logic:** The AI may provide context-specific multipliers for `packs`. In such cases, the multiplier MUST be recorded in the `CommittedMutation` audit trail.

#### D. Anomalous Dimension (Informal)

- **Base Unit:** Misc (`misc`)
- **Authorized Conversions:** None.
- **Logic:** Non-convertible units (e.g., "handful", "bunch") are treated as distinct identities.

### 2. The Internal SI Stabilization Model

- **Storage Layer:** The `base_quantity` is a high-precision `rust_decimal::Decimal` normalized to the dimension's Base Unit.
- **Rounding Policy:** All conversions MUST use **Banker's Rounding** (Half-to-Even) to prevent cumulative arithmetic bias.
- **Metadata Layer:** The user's last-provided unit is stored as `display_unit` to ensure consistent unit representation during the Query/Display phase.
- **Consensus Log:** Every `CommittedMutation` records the **Absolute Result in the Base Unit** for idempotency, while simultaneously persisting the `display_unit` to preserve user intent.

### 3. Dimensional Invariance (The Fence)

- **Rule:** `ADD/SUBTRACT` operations are only permitted within the same dimension.
- **Leader Enforcement:** If a mutation attempts a cross-dimensional transition (e.g., adding `L` to `g`), the Leader issues an immediate **Veto**.
- **Exception:** A `SET` operation allows the user to redefine the item's dimension.
- **Semantic Check:** Cross-dimensional `SET` operations MUST be evaluated by the AI Oracle to ensure the transition is physically meaningful.

### 4. The Anomalous Identity Split

- **Rule:** If the AI cannot resolve an anomalous unit to a standard base, it must generate a distinct `item_key` by appending the unit slug (e.g., `apple_handful`).

## Rationale

- **LLM Semantic Clarity:** Using a hierarchical list structure minimizes token-drift and ensures the AI correctly identifies the relationship between units and dimensions.
- **Arithmetic Precision:** Normalizing to the smallest standard base (`g`, `ml`) protects the ledger from cumulative rounding errors.
- **Idempotency:** Logging the absolute SI result allows Followers to recover the state without re-running conversion logic.

## Consequences

### Pros

- **Uniform State:** All cluster nodes see a mathematically identical "Source of Truth."
- **System Flexibility:** Easy to add new units to the Registry without changing the Protobuf schema.
- **High Integrity:** The "Dimensional Fence" prevents non-sensical grocery arithmetic.

### Cons

- **Query Latency:** Requires a division step (Base / Display Multiplier) during `QueryState`.
- **Complexity:** Requires strict defensive validation in the Leader's Layer 4 to prevent AI-multiplication errors.

## Operational Impact

- **Registry Synchronization:** Changes to authorized units or multipliers require a coordinated code update; mismatched registries between nodes will trigger the **Halt Mandate** during log replay.
- **Computational Cost:** Every `QueryState` request incurs a "Reverse Math" penalty (Base SI / Display Multiplier) to satisfy user display preferences.
- **Data Fragmentation:** Extensive use of non-standard units (Anomalous Dimension) will lead to fragmented item identities (e.g., `apple_handful` vs `apple_units`), increasing the complexity of manual inventory audits.
- **Mathematical Monitoring:** External observability tools must implement **Banker's Rounding** to accurately reconcile the cluster state with external data sources.
