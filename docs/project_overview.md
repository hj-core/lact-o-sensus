# Project Overview: Lact-O-Sensus

## 🎯 Vision

**Lact-O-Sensus** is a clinical, leader-centric distributed ledger designed for high-fidelity grocery inventory management. It treats physical grocery state with the same rigor as financial transactions, utilizing a Domain-Agnostic Replicated State Machine (RSM) powered by the Raft consensus protocol.

The system is built on a **Clean Architecture** (Hexagonal Architecture) to ensure that the consensus mechanism, business logic, and delivery mechanisms are strictly decoupled and independently testable.

---

## 🏗️ System Architecture (Crate Hierarchy)

The project is structured as a multi-crate Cargo workspace to enforce strict boundary defense and dependency inversion.

### 1. `common` (The System Contract)
- **Role:** Foundational types and shared interfaces.
- **Key Components:**
    - **Domain Primitives:** `LogIndex`, `Term`, `SequenceId`, `ClientId`, `NodeId`.
    - **System Contract:** `RaftHandle` (mutation path), `InventorySource` (query path), and `StateMachine` (boundary trait).
    - **Physicality:** Universal SI Unit Registry and stabilization logic.
    - **Protocol:** Compiled Protobufs for both internal consensus and external application layers.

### 2. `raft-engine` (The Infrastructure)
- **Role:** A generic, domain-agnostic implementation of the Raft Consensus Protocol.
- **Responsibility:**
    - Manages leader election and heartbeat orchestration.
    - Replicates opaque byte payloads across a quorum of nodes.
    - Enforces sequential log application via the generic `StateMachine` trait.
    - **Clean Boundary:** Operates without any knowledge of "Groceries" or application-specific logic.

### 3. `lacto-fsm` (The Business Logic)
- **Role:** The application-specific State Machine implementation.
- **Responsibility:**
    - Implements the `StateMachine` trait to interpret the replicated log.
    - Manages the persistent grocery inventory using **`sled`**.
    - Implements the `InventorySource` trait to provide authoritative query results.
    - **Physical Truth:** Enforces SI stabilization and the "Dimensional Fence" for all inventory updates.

### 4. `gateway` (The Delivery Layer)
- **Role:** External application interface and policy enforcement.
- **Responsibility:**
    - Implements the gRPC `IngressService` for client communication.
    - **The Defensive Onion (ADR 007):** Orchestrates the 5-layer pipeline from user intent to consensus proposal.

### 5. `ai-veto` (The Oracle)
- **Role:** Relational AI evaluation engine.
- **Responsibility:**
    - Resolves natural language intents to clinical slugs and SI base units.
    - Performs context-aware moral evaluation of mutations.

### 6. `client-cli` (The Consumer)
- **Role:** Interactive REPL for human operators.
- **Responsibility:**
    - Manages a local Write-Ahead Log (WAL) for client-side linearizability.
    - Implements resilient retry loops with exponential backoff.

### 7. `node-server` (The Composition Root)
- **Role:** The binary entry point that wires the system together.
- **Responsibility:**
    - Parses configuration and initializes all local services.
    - Instantiates the `raft-engine`, `lacto-fsm`, and `gateway`.
    - Performs **Dependency Injection** to bind the layers together into a functional cluster node.

---

## 🛡️ Core Mandates

### 1. Poison-then-Panic (ADR 009)
To mitigate structural fragility, any detection of invariant violation (protocol errors, data corruption) triggers a transition to a `Poisoned` state followed by an immediate `panic!`. This prevents "Zombie Nodes" from participating in consensus.

### 2. Exactly-Once Semantics (ADR 006)
The system guarantees linearizability through a replicated Session Table. Every mutation (Success or Veto) is logged as a consensus event, and client sequence IDs are strictly enforced (`seq == last_seen + 1`).

### 3. Registry Firewall (ADR 007)
All AI-provided metadata is verified against clinical system registries. The system acts as a **Clinical Notary**, ensuring that the AI cannot redefine physical laws or unregistered taxonomies.

### 4. SI Stabilization (ADR 008)
All physical quantities are stabilized to canonical SI base units (grams, milliliters) using **Banker's Rounding** to eliminate cumulative numeric bias across the cluster.
