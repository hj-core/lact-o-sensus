# ADR 004: Bootstrapping and Cluster Identity

## Metadata

- **Date:** 2026-04-09
- **Status:** Proposed
- **Scope:** Cluster Initialization and Membership
- **Primary Goal:** Provide a safe, multi-phase path from static configuration to dynamic node discovery.
- **Last Updated:** 2026-04-20

## Context

In distributed environments, IP addresses are often transient (especially in containerized setups). Relying solely on IPs for identity is fragile. Furthermore, in environments with multiple clusters, there is a risk of "Cross-Cluster Contamination" where a node from one cluster accidentally interacts with another. We need a robust identity system and a clear roadmap for how nodes find each other.

## Decision

We will separate logical identity from physical networking and adopt a three-phase bootstrapping roadmap.

### 1. Logical Identity Tuple

Every **System Node** (including Raft participants and AI Veto Nodes) is uniquely identified by the tuple `(cluster_id, node_id)`. **Client Actors** are identified by the tuple `(cluster_id, client_id)`. To prevent **primitive obsession**, core domain identifiers must be implemented as distinct **NewTypes** (`ClusterId`, `NodeId`, `ClientId`, `Term`, `LogIndex`) with self-validating constructors.

- **`cluster_id` (`ClusterId`):** A unique namespace for the entire consensus group (e.g., "lacto-prod-01").
- **`node_id` (`NodeId`):** A unique identifier for a system node (Raft or AI).
- **`client_id` (`ClientId`):** A unique logical identifier for a client session.
- **Mandate (Identity Guard):** Every gRPC request (Raft peer-to-peer, Client-to-Leader, and Leader-to-AI) must include the `cluster_id` and the `target_node_id` (the intended logical recipient).
- **Enforcement:** Validation MUST be implemented at the **Middleware/Interceptor layer**. Nodes must reject any message where the `cluster_id` or `target_node_id` does not match their local configuration before it reaches the application logic.

### 2. Bootstrapping Roadmap

We will implement the system in three evolutionary phases:

- **Phase 1: Static Membership, Static Discovery (MVP)**
  - Identity and IP mappings are hardcoded in a local `config.toml`.
  - Membership is fixed; the **Quorum Size** (`(N/2)+1`) is calculated once at startup and remains immutable throughout the node's lifecycle.
- **Phase 2: Static Membership, Dynamic Discovery**
  - The set of `node_id`s in the cluster remains fixed.
  - Nodes use a **Seed List** (a subset of known IPs) to "Discover" the current IP addresses of their peers.
  - Allows nodes to restart on different IPs without re-configuring the entire cluster.
- **Phase 3: Dynamic Membership, Dynamic Discovery**
  - Nodes can be added to or removed from the cluster at runtime.
  - Requires implementing **Raft Cluster Membership Changes** (Joint Consensus) to safely transition quorum sizes.

### 3. Identity Persistence

The `(cluster_id, node_id)` tuple must be persisted to local **stable storage** during the first initialization.

- **Mandate (Identity Integrity Check):** Subsequent restarts must load this identity from disk and compare it against the current `config.toml`.
- **The Halt Mandate:** If the persisted identity does not match the configured identity, the node **MUST panic immediately** to prevent log contamination or "phantom node" behavior.

## Rationale

- **Cluster Isolation:** The `cluster_id` prevents split-brain or corruption scenarios caused by misdirected traffic between separate environments.
- **Transient IP Support:** Separating `node_id` from IP allows the system to survive in modern cloud/K8s environments where pods are frequently rescheduled.
- **Scope Management:** By starting with "Static Membership" (Phase 1 & 2), we avoid the extreme complexity of Raft's joint consensus protocol while still building a "production-ready" discovery mechanism.

## Consequences

### Pros

- **Safety:** Zero risk of cluster contamination.
- **Flexibility:** Nodes can move between IP addresses (Phase 2+) without manual intervention.
- **Traceability:** Audit trails and logs will use logical `node_id`s, making them much easier to read than raw IPs.

### Cons

- **Configuration Overhead:** Users must manage a `cluster_id` and ensure it is consistent across all members.
- **Bootstrap Delay:** Dynamic discovery (Phase 2+) introduces a small delay at startup as nodes "Gossip" to find their peers.

### Operational Impact

- **Storage:** A small amount of persistent disk space is required to store the node's identity.
- **Deployment:** Deployment scripts must ensure each node is assigned a unique `node_id` within the `cluster_id`.
