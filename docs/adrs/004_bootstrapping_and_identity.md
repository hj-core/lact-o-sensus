# ADR 004: Bootstrapping and Cluster Identity

## Metadata

- **Date:** 2026-04-09
- **Status:** Accepted
- **Scope:** Logical identity system (`cluster_id`, `node_id`, `client_id`), bootstrapping phases, identity persistence and verification, gRPC-level identity enforcement. Excludes runtime membership changes (Phase 3), DNS-based discovery implementation, TLS certificate management, and configuration file schema.
- **Primary Goal:** Provide a safe, multi-phase path from static configuration to dynamic node discovery.
- **Last Updated:** 2026-06-09

## Context

Distributed consensus protocols depend on a stable notion of node identity. In containerized environments (Kubernetes, Docker), IP addresses change on every restart; relying on IPs for node identity is brittle — a node restarting with a new IP could be mistaken for a different node. Raft's core algorithm (Ongaro & Ousterhout, USENIX ATC 2014, §5) assumes a fixed set of participants identified by a persistent logical identifier, not a transient network address. Raft also supports dynamic membership changes via joint consensus (§6), but this adds significant complexity — a transitional configuration epoch that doubles quorum requirements and increases the verification surface.

A second concern is cross-cluster contamination. If multiple clusters exist in the same network (e.g., staging and production), a node from one cluster must never participate in another cluster's Raft quorum. Without a namespacing mechanism, a misconfigured peer could inadvertently join the wrong consensus group, leading to log corruption or split-brain scenarios.

These constraints are downstream of the crash-recovery model (ADR 001) and the network topology (ADR 002): every gRPC request must carry identity metadata to enable interceptor-level enforcement (per ADR 005). We must define a logical identity system and a phased bootstrapping roadmap that safely evolves from static configuration to dynamic discovery without compromising safety.

## Options Considered

### Option A: IP-Based Identity with DNS Discovery

Nodes are identified by IP address; peer discovery uses DNS resolution of a headless service.

- **Safety**: Weak — IP reuse across restarts or container rescheduling can cause identity confusion; no cross-cluster isolation mechanism.
- **Complexity**: Lowest — no custom identity management; relies on existing infrastructure (DNS).
- **Flexibility**: Moderate — DNS-based discovery handles transient IPs, but cannot distinguish between clusters or prevent cross-cluster contamination.
- **Verdict**: Rejected — insufficient safety guarantees for a clinical-grade system.

### Option B: Logical Identity Tuple with Static Membership (Chosen)

Nodes are identified by `(cluster_id, node_id)` NewTypes; membership is statically configured and evolves through defined phases.

- **Safety**: Strong — cluster_id prevents cross-cluster contamination; NewTypes prevent primitive obsession; identity check on restart enforces crash-safety (Halt Mandate per ADR 009).
- **Complexity**: Moderate — requires gRPC interceptor middleware (ADR 005) and identity persistence, but avoids joint consensus in Phase 1.
- **Flexibility**: Phased — starts with static discovery (Phase 1), enables dynamic discovery (Phase 2), and eventually dynamic membership (Phase 3).
- **Verdict**: Chosen — balances safety and incremental complexity.

### Option C: Shared Secret / Token-Based Identity

Nodes authenticate via a pre-shared secret; the secret serves as both identity and proof of cluster membership.

- **Safety**: Moderate — prevents unauthorized nodes but does not distinguish between node roles (Raft vs. AI vs. Client) nor support logical addressing for routing.
- **Complexity**: Moderate — requires secure secret distribution and rotation.
- **Flexibility**: Low — a shared secret cannot express `node_id` for leader redirection (ADR 002) or `client_id` for exactly-once semantics (ADR 006).
- **Verdict**: Rejected — insufficient granularity for the required routing and deduplication use cases.

### Option D: Do Nothing

Use raw IP addresses with no logical identity or cross-cluster isolation.

- **Safety**: None — no protection against cross-cluster contamination or identity confusion.
- **Complexity**: None — no additional infrastructure.
- **Flexibility**: None — cannot support dynamic IPs or multi-cluster deployments.
- **Verdict**: Rejected — unacceptable operational risk.

## Decision

We will separate logical identity from physical networking and adopt a three-phase bootstrapping roadmap.

### Assumptions & Constraints

- **Crash-Recovery model** (per ADR 001): Nodes may stop and restart; identity must survive across restarts and be verified on recovery.
- **Cluster size ≤ 7 nodes**: Membership is small enough that static configuration (Phase 1) is ergonomic; larger clusters would require Phase 3 sooner.
- **Containerized deployment**: IPs are expected to be transient; identity must be independent of network address.
- **Manual cluster_id management**: The `cluster_id` is assigned by an operator and must be globally unique within the deployment; no automatic namespace registration is provided.
- **TCP/gRPC transport** (per ADR 002): Identity metadata flows through gRPC interceptors; the transport layer provides reliable FIFO delivery within a stream.
- **NewType enforcement**: Core domain identifiers (`ClusterId`, `NodeId`, `ClientId`) must be self-validating NewTypes at module boundaries; inline formulas (e.g., quorum calculation) may use primitives internally but must not leak across module boundaries.

### 1. Logical Identity Tuple

Every **System Node** (including Raft participants and AI Veto Nodes) is uniquely identified by the tuple `(cluster_id, node_id)`. **Client Actors** are identified by the tuple `(cluster_id, client_id)`. To prevent **primitive obsession**, core domain identifiers must be implemented as distinct **NewTypes** (`ClusterId`, `NodeId`, `ClientId`) with self-validating constructors.

- **`ClusterId`:** A unique namespace for the entire consensus group (e.g., "lacto-prod-01").
- **`NodeId`:** A unique identifier for a system node (Raft or AI).
- **`ClientId`:** A unique logical identifier for a client session.
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

- **Mandate (Identity Integrity Check):** Subsequent restarts must load this identity from disk and compare it against the current configuration.
- **The Halt Mandate:** If the persisted identity does not match the configured identity, the node **MUST halt immediately** to prevent log contamination or "phantom node" behavior. At startup, this is enforced as a clean error through the composition root, aborting the process. At runtime (e.g., state machine divergence), the stricter "Poison-then-Panic" protocol from ADR 009 applies.

## Rationale

- **Cluster Isolation:** The `cluster_id` prevents split-brain or corruption scenarios caused by misdirected traffic between separate environments. This is enforced at the gRPC interceptor layer (per ADR 005), ensuring that identity validation occurs before any application logic executes — a defensive measure against cross-environment misconfiguration.
- **NewType safety:** By implementing `ClusterId`, `NodeId`, and `ClientId` as self-validating NewTypes (per ADR 004's own anti-primitive-obsession mandate), we prevent identifier confusion at module boundaries — a `NodeId` cannot accidentally be used where a `ClientId` is expected, and invalid values are caught at construction time rather than at runtime.
- **Transient IP Support:** Separating `node_id` from IP address allows the system to survive in modern cloud/K8s environments where pods are frequently rescheduled. Logical identity is orthogonal to network addressing, which is consistent with the network topology model in ADR 002.
- **Scope Management** (per Raft paper §6): Joint consensus (Raft §6; Ongaro, 2014) introduces a transitional configuration epoch that doubles quorum requirements during membership changes. By starting with static membership (Phase 1 & 2), we defer this complexity until the system has proven operational stability. Phase 3 may adopt joint consensus when runtime membership changes become necessary.
- **Halt Mandate** (per ADR 001, ADR 009): Verifying persisted identity against configuration on every restart prevents phantom nodes — a node that has been cloned, migrated to a different cluster, or had its configuration changed without a corresponding storage wipe cannot silently rejoin the cluster. This is a direct application of ADR 001's crash-recovery safety guarantees and ADR 009's Poison-then-Panic protocol for unrecoverable invariant violations.

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

## Follow-Up

- **Phase 2 implementation:** Implement dynamic discovery (seed list-based) before migrating to ephemeral IP environments (K8s). Trigger: when manual node reconfiguration exceeds operational tolerance.
- **Phase 3 evaluation:** Revisit joint consensus (Raft §6) when cluster growth or node churn makes static membership untenable. Trigger: cluster size exceeds 7 nodes or nodes are replaced more frequently than once per week.
- **Identity check audit:** Validate the Halt Mandate behavior in CI via integration tests that simulate configuration mismatch scenarios.
- **Phased roadmap update:** Update this ADR when Phase 2 or Phase 3 is implemented, noting the operational experience gained from each phase.
