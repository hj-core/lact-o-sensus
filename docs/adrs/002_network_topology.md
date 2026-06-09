# ADR 002: Network Topology and Communication Constraints

## Metadata

- **Date:** 2026-04-09
- **Status:** Accepted
- **Scope:** Communication paths between Raft cluster nodes, client nodes, and the AI Veto node. Excludes transport-layer security, DNS-based discovery, connection pooling, and port numbering (operational concerns).
- **Primary Goal:** Minimize communication complexity and ensure deterministic state machine transitions.
- **Last Updated:** 2026-06-09

## Context

Lact-O-Sensus comprises three node types — Raft cluster nodes, client nodes, and an external AI Veto Node — each in a different trust and failure domain (per ADR 001). The Raft protocol guarantees total order of commands within the cluster, but only if commands enter through a single sequencing point (the Leader). The AI Veto Node is non-deterministic by nature (LLM-based); if two different cluster nodes independently query the AI for the same mutation and receive different verdicts, those conflicting responses could enter the Raft log at different positions, violating linearizability.

Consider this failure scenario: a Follower receives a client mutation, independently queries the AI Veto, logs the response, and proposes it to Raft. Meanwhile, the Leader has already sequenced a different AI response for the same mutation. The Raft log now contains conflicting entries for the same logical intent — a direct violation of the exactly-once semantics mandated by ADR 006.

To maintain linearizability and a single source of truth, we must strictly define which nodes are allowed to communicate with which other nodes, and through which paths.

## Options Considered

### Option A: Full Mesh for All Nodes (Chosen for Internal, Rejected for External)

Allow every node (cluster, client, AI) to communicate directly with every other node.

- **Consistency**: Weak — multiple nodes could independently query the AI and obtain conflicting responses, breaking linearizability (violates ADR 006).
- **Performance**: Highest throughput — no single-node bottleneck; clients can submit to any node.
- **Complexity**: Highest — every node must manage N² connection states and N RPC service definitions.
- **Safety**: Low — no central sequencing point; non-deterministic AI responses could enter the log out of order.

### Option B: Leader-Centric Hub-and-Spoke (Chosen)

All external traffic (client mutations and AI queries) routes through the Raft Leader. Internal cluster traffic remains full-mesh.

- **Consistency**: Strong — the Leader sequences all mutations and all AI queries, guaranteeing total order (satisfies ADR 006).
- **Performance**: Moderate — single-node bottleneck, but acceptable for grocery-scale workloads (sub-ms AI latency, low request rate).
- **Complexity**: Moderate — clients must implement leader discovery and redirection; cluster nodes remain simple.
- **Safety**: High — the Leader acts as a natural validation gateway (per ADR 007's Defense Onion) and the sole AI interaction point.

### Option C: Client-Direct-to-AI with Async Replication

Clients query the AI directly and submit both the mutation and the AI response to any cluster node, which then replicates asynchronously.

- **Consistency**: Weak — different clients may submit conflicting AI responses for the same item; the cluster has no way to reconcile them deterministically.
- **Performance**: Highest — no central bottleneck; clients parallelize AI queries.
- **Complexity**: Low for the cluster, high for clients — clients must manage AI interaction and response submission.
- **Safety**: Low — the AI response is untrusted by consensus; the cluster becomes a passive log with no deterministic resolution.

### Option D: Do Nothing

Continue without defined communication paths; handle routing and determinism reactively.

- **Consistency**: None — no guarantees.
- **Performance**: Baseline — no constraints imposed.
- **Complexity**: None — but guarantees are undefined.
- **Safety**: Unacceptable — the system cannot guarantee linearizable semantics without topology constraints.

## Decision

We will adopt a **Leader-Centric Hub-and-Spoke** topology for all external interactions, while maintaining a **Full Mesh** for internal consensus.

### Assumptions & Constraints

- **Single Leader at any time**: Raft guarantees at most one valid Leader per term; the topology depends on this property and is invalid under a multi-leader model.
- **Crash-Recovery model** (per ADR 001): Cluster nodes are non-Byzantine; a malicious Leader could exfiltrate data to the AI, but this is outside the threat model.
- **Cluster size**: Expected ≤ 7 nodes; leader bottleneck becomes significant beyond ~15–20 nodes.
- **AI Veto is passive**: The AI Node only responds to requests; it never initiates connections or pushes data to the cluster.
- **Clients support redirect**: Clients must implement leader discovery and `leader_hint` retry logic; clients that cannot redirect (e.g., read-only observers) are restricted to follower reads.
- **Network reliability**: Assumes reliable FIFO delivery (TCP); message corruption or reordering is not handled at the topology layer.

### 1. Communication Zones

The system is divided into three distinct communication zones:

- **Zone A (Internal Mesh):** All Raft cluster nodes maintain full-mesh connectivity for heartbeats, pre-vote coordination, elections, and log replication — spanning all role states (Follower, PreCandidate, Candidate, Leader).
- **Zone B (Ingress):** Client Nodes may contact any cluster participant for leader discovery, but mutation and query requests are **logically processed** exclusively by the current Raft Leader.
- **Zone C (Egress):** The Raft Leader is the **exclusive initiator** of requests to the AI Veto Node.

### 2. Restricted Paths

- **Client to AI:** Strictly forbidden. Clients cannot bypass the consensus group to query the "Moral Advocate" directly.
- **Non-Leader to AI:** Strictly forbidden. To ensure cluster-wide determinism, only the current Leader may query the AI. The AI's response is then replicated as a "fact" in the Raft log.
- **AI to Cluster:** The AI Node is a passive service; it only responds to requests initiated by the Raft Leader.

### 3. Leader Redirection Mechanism

If a Client attempts to initiate a mutation or query with a Non-Leader node (Follower or Candidate):

- The node must reject the request.
- The node must provide a **`leader_hint`**—a logical redirection token containing the metadata required for the Client to re-route its request to the current Leader.
- The Client is responsible for processing the `leader_hint` to update its internal routing state and retrying the request.

## Rationale

- **Determinism:** Raft guarantees total order of commands only when all commands originate from a single Leader (Ongaro & Ousterhout, USENIX ATC 2014). Allowing AI responses to enter at any node would inject non-deterministic inputs outside the Leader's sequencing, creating a source of conflicting log entries for the same logical intent. By making the Leader the exclusive AI interlocutor, every AI response enters the log as a first-class Raft entry at a deterministic position.
- **Linearizability** (per ADR 006): Exactly-once semantics require that all mutations be applied in a single, globally agreed order. The Leader provides this sequencing point; allowing clients to submit mutations to any node would require additional coordination (forwarding, two-phase commit) that reintroduces complexity without benefit.
- **Security:** Centralizing all external traffic through the Leader reduces the attack surface for AI-related RPCs from O(N) to O(1) — a single node must be secured rather than N. This is consistent with ADR 007's Defense Onion, where the Leader enforces mutation validation (Layers 1–2 syntactic scrubbing) before any egress to the AI Veto.
- **Simplicity:** The full-mesh topology within the cluster (heartbeats, log replication) is already O(N²) by necessity. Extending that mesh to clients and the AI would multiply the number of RPC service definitions each node must export and the connection states each node must maintain. Restricting external paths to the Leader limits this growth to O(1) per external node.

## Consequences

### Pros

- **Consistency:** Guaranteed cluster-wide agreement on the "Moral Veto" status of an item.
- **Reduced Surface Area:** Fewer open ports and permitted communication paths simplify debugging and potential security hardening.
- **Architectural Clarity:** Clear separation between "Consensus Logic" and "External Policy."

### Cons

- **Leader Bottleneck:** All external traffic (Client and AI) flows through a single node, which could limit throughput in high-load scenarios (though negligible for a grocery list).
- **Client Complexity:** Clients must implement "Leader Discovery" and "Redirection" logic rather than simply "Fire and Forget."
- **Higher Latency:** Mutations require an extra network hop (Leader -> AI -> Leader) before the consensus phase begins.

### Operational Impact

- **Discovery:** Clients are configured with a seed list of known node addresses at startup. Leader discovery is implicit: the client contacts any known node, and if that node is not the leader, it receives a `leader_hint` redirect response containing the current leader's address. The client updates its internal routing state and retries against the hinted leader. A retry loop with exponential backoff and node rotation handles cascading redirects and transport failures.

## Follow-Up

- **gRPC `leader_hint` field:** Define the `leader_hint` metadata in the Gateway's gRPC response envelope (protobuf or trailing metadata).
- **Client redirect logic:** Implement `leader_hint` processing and automatic retry in the `client-cli` crate.
- **Redirect monitoring:** Add a counter metric for redirect events in the Gateway to detect network instability or frequent leader changes.
- **Follower read support:** If read-heavy workloads emerge, consider allowing read-only queries on Followers (with staleness awareness) to reduce Leader bottleneck.
