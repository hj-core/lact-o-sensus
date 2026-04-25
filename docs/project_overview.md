# Project Overview: **Lact-O-Sensus**

## _The Distributed, AI-Vetted Grocery Consensus Engine_

**Lact-O-Sensus** is an over-engineered distributed system designed to manage a household grocery list with the same level of data integrity and fault tolerance as a global financial database. Built in **Rust**, it utilizes a consensus-based architecture to ensure that no item is ever lost or duplicated without both technical agreement from the cluster and semantic resolution from an AI "Moral Advocate."

---

## 🎯 The Vision

The goal is to transform a simple "Grocery List" into a **Replicated State Machine**. The system aims to provide a unified and consistent experience where a cluster of independent nodes serves as a reliable source of truth for the user, bridging the gap between ambiguous human intent and deterministic system state.

---

## 🏗 The Three Pillars (Roles & Responsibilities)

### 1. The Client Node (The "Interface")

- **Role:** The entry point for user interaction (CLI, Mobile, or Web).
- **Responsibility:** Captures raw user intent and performs initial structural validation.
- **Mandate:** Must maintain session integrity to support **Exactly-Once Semantics (EOS)**.

### 2. The Raft Cluster Node (The "Consensus")

- **Role:** A participant in the distributed consensus group.
- **Responsibility:** Manages the **Write-Ahead Log (WAL)**, the **State Machine**, and the **Defensive Onion**.
- **Internal Architecture:** Utilizes a **Tri-Layered "Onion" Model** (ADR 009) to strictly isolate protocol logic (Physical), role orchestration (Logical), and concurrent signaling (Execution).
- **Mandate:** Acts as the "Syntactic Fortress," ensuring that only validated, AI-resolved ledger facts are committed to the replicated log.

### 3. The AI Veto Node (The "Semantic Oracle")

- **Role:** An external resolution and validation service.
- **Responsibility:** Maps ambiguous user strings to **Canonical Identities**, performs unit conversions, and provides taxonomic classification.
- **Mandate:** Acts as a specialized advisor, ensuring that committed data meets both physical logic (Dimensions) and moral standards (Veto).

---

## 📝 The Data Model (The "Truth")

The state machine maintains two distinct schemas within the replicated log. Every operation is scoped to a unique **`cluster_id`** to ensure cluster isolation.

### 1. The Item Store (Grocery Inventory)

This represents the current state of the grocery list, stored in a **Flat Identity Model** (ADR 007):

- **item_key (`CanonicalSlug`):** A unique, AI-resolved canonical slug (e.g., "orange_juice").
- **base_quantity (`BaseQuantity`):** A high-precision `rust_decimal::Decimal` normalized to the **SI Base Unit** (ADR 008).
- **base_unit (`UnitSymbol`):** The canonical SI symbol (`g`, `ml`, `units`) used for internal math.
- **display_unit (`UnitSymbol`):** The user's last-preferred unit symbol (e.g., "lb", "gal").
- **category (`Category`):** Mapping to the **12-Point Authorized Taxonomy**.
- **suggested_display_name (`String`):** A human-friendly label suggested by the AI.
- **last_modifier_id (`ClientId`):** The identifier of the client who performed the last mutation.
- **state_version (`LogIndex`):** The monotonic version of the state machine (Raft log index).

### 2. The Session Table (Exactly-Once Semantics)

This represents the metadata required to ensure linearizability and deduplication:

- **client_id (`ClientId`):** Unique logical identifier for a specific client session.
- **last_sequence_id (`SequenceId`):** The sequence number of the **most recently processed** request.
- **cached_response (`LogicalResponse`):** The structured outcome (Status, Version, Error) of the last operation (ADR 006).
- **last_activity_effective_time (`Timestamp`):** The state-machine-derived time of the last activity, used for deterministic purging.

---

## ⚖️ The 12-Point Authorized Taxonomy

To ensure deterministic classification, all items must be mapped to exactly one of the following clinical categories. Items falling into the "Anomalous" category trigger an **Anomalous Identity Split** (ADR 008), resulting in distinct item identities per unit.

1. **Primary Flora:** Unprocessed botanical matter (Fruits, Vegetables).
   ...
2. **Anomalous Inputs:** Items failing systematic classification (Miscellaneous).
3. **Flesh & Marrow:** Carcass-based nutritional inputs (Meat, Seafood).
4. **Shelf-Stable Carbohydrates:** Dry pantry staples (Grains, Pasta, Legumes).
5. **Cultured Doughs:** Yeast-risen or unleavened grain products (Bakery).
6. **Liquefied Hydration:** Non-alcoholic hydration solutions (Water, Tea, Coffee).
7. **Condiments & Catalysts:** Flavor enhancers and cooking mediums (Oils, Spices, Sauces).
8. **Nutrient-Sparse Commodities:** High-caloric, low-utility snacks (Sweets, Chips, "Junk Food").
9. **Ethanol Solutions:** Fermented or distilled recreational fluids (Alcohol).
10. **Biomedical Maintenance:** Supplements and medicinal agents (Health, First Aid).
11. **Sanitization & Utility:** Non-consumable environmental maintenance (Cleaning, Toiletries).
12. **Anomalous Inputs:** Items failing systematic classification (Miscellaneous).

---

## 🎓 Learning Goals

By the end of this project, we will have achieved practical expertise in the following domains:

- **Consensus Systems:** Building the **Raft Protocol** from the ground up, including leader election and replication.
- **Architectural Patterns:** Implementing the **Internal Onion Model** to strictly decouple consensus logic from system-level concurrency and reactive signaling.
- **Semantic State Machines:** Integrating non-deterministic AI Oracles into deterministic consensus logs via a multi-layered defensive pipeline.
- **System Integrity:** Implementing **Internal SI Stabilization** and **Exactly-Once Semantics** to ensure data precision and linearizability.
- **Persistence & Recovery:** Mastering the interaction between WAL updates and stable storage (`sled`).
