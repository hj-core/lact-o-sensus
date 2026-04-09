# Project Overview: **Lact-O-Sensus**

## *The Distributed, AI-Vetted Grocery Consensus Engine*

**Lact-O-Sensus** is an over-engineered distributed system designed to manage a household grocery list with the same level of data integrity and fault tolerance as a global financial database. Built in **Rust**, it utilizes a consensus-based architecture to ensure that no item is ever lost or duplicated without both technical agreement from the cluster and moral agreement from an AI "Moral Advocate."

---

## 🎯 The Vision

The goal is to transform a simple "Grocery List" into a **Replicated State Machine**. The system aims to provide a unified and consistent experience where a cluster of independent nodes serves as a reliable source of truth for the user.

---

## 🏗 The Three Pillars (Roles & Responsibilities)

### 1. The Client Node (The "Interface")

* **Role:** The entry point for user interaction (CLI, Mobile, or Web).
* **Responsibility:** Captures user intent and formats it for cluster consumption.
* **Mandate:** Must maintain session integrity to support exactly-once semantics.

### 2. The Raft Cluster Node (The "Consensus")

* **Role:** A participant in the distributed consensus group.
* **Responsibility:** Manages the **Write-Ahead Log (WAL)** and the **State Machine**.
* **Mandate:** Replicates and commits data updates across the cluster to ensure the integrity and persistence of the grocery list.

### 3. The AI Veto Node (The "Policy")

* **Role:** An external validation service.
* **Responsibility:** Provides an automated evaluation of proposed grocery entries based on health, safety, or quantity heuristics.
* **Mandate:** Acts as a specialized advisor to the cluster, ensuring that committed data meets pre-defined standards.

---

## 📝 The Data Model (The "Truth")

To ensure both business accuracy and distributed safety, the state machine maintains two distinct schemas within the replicated log. Every operation is scoped to a unique **`cluster_id`** to ensure cluster isolation.

### 1. The Item Store (Grocery Inventory)

This represents the current state of the grocery list that all nodes must agree on:

* **item_key (`String`):** A unique, normalized identifier for the item (e.g., "oat_milk").
* **quantity (`Decimal`):** A precision-safe numeric amount.
* **unit (`String`):** The unit of measurement (e.g., "liters", "grams").
* **category (`Enum`):** Mapping to the **12-Point Authorized Taxonomy**.
* **last_modifier_id (`String`):** The identifier of the client who performed the last mutation.
* **state_version (`u64`):** The monotonic version of the state machine (Raft log index) at the time of the mutation.

### 2. The Session Table (Exactly-Once Semantics)

This represents the metadata required to ensure linearizability and deduplication:

* **client_id (`UUID`):** Unique identifier for a specific client session.
* **last_sequence_id (`u64`):** The sequence number of the **most recently processed** request from this client.
* **cached_response (`Bytes`):** The full response (status, version, or error) of the last operation, used to handle retries.
* **last_activity (`Timestamp`):** Metadata used for deterministic session expiration (TTL).

---

## ⚖️ The 12-Point Authorized Taxonomy

To ensure deterministic classification and provide the "Moral Advocate" with high-leverage metadata, all items must be mapped to exactly one of the following clinical categories:

1. **Primary Flora:** Unprocessed botanical matter (Fruits, Vegetables).
2. **Animal Secretions:** Liquid and solid animal-derived proteins (Dairy, Eggs).
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

*Note: Storage state (e.g., "Frozen") is treated as secondary metadata. Content-type always dictates the primary category.*

---

## 🔬 Related Fields

Lact-O-Sensus sits at the intersection of several advanced computer science domains:

* **Distributed Systems:** Consensus algorithms (Raft), linearizability, and fault tolerance.
* **Database Internals:** Write-Ahead Logging (WAL), Log-Structured Merge (LSM) principles, and ACID compliance.
* **Systems Programming:** Memory safety, zero-cost abstractions, and high-concurrency async I/O.
* **Applied AI:** Integrating non-deterministic LLMs into deterministic state machines via proposal-phase filtering.

---

## 🛠 Technical Stack

* **Language:** Rust (2024 Edition).
* **Concurrency:** `tokio` (Async runtime).
* **Communication:** `tonic` & `prost` (gRPC / Protocol Buffers).
* **Persistence:** `sled` (Embedded KV store).
* **AI Integration:** OpenAI API or local Llama via `ollama-rs`.

---

## 🎓 Learning Goals

By the end of this project, we will have achieved deep expertise in the following domains:

* **Distributed Consensus:** Building the **Raft Protocol** from the ground up, including Leader Election, Log Replication, and Safety invariants.
* **Linearizability & Semantics:** Implementing **Exactly-Once Semantics** and session tracking to ensure a consistent view of the world across clients.
* **Systems Programming in Rust:** Mastering high-performance, asynchronous networking and memory-safe shared state using `tokio` and `Arc/Mutex` patterns.
* **Persistence & Recovery:** Designing a robust **Write-Ahead Log (WAL)** and **State Machine** snapshots to survive system crashes and reboots.
* **Security & AI Ethics:** Integrating LLMs as active participants in a state machine, including defenses against prompt injection and defining automated failure policies.
