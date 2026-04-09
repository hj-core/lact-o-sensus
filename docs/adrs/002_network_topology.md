# ADR 002: Network Topology and Communication Constraints

## Metadata

* **Date:** 2026-04-09
* **Status:** Proposed
* **Scope:** Network Architecture and Node Inter-communication
* **Primary Goal:** Minimize communication complexity and ensure deterministic state machine transitions.

## Context

In a distributed system like Lact-O-Sensus, allowing "any-to-any" communication increases the risk of non-deterministic behavior, especially when integrating a non-deterministic external service like an AI Node. To maintain linearizability and a single source of truth, we must strictly define which nodes are allowed to talk to each other.

## Decision

We will adopt a **Leader-Centric Hub-and-Spoke** topology for all external interactions, while maintaining a **Full Mesh** for internal consensus.

### 1. Communication Zones

The system is divided into three distinct communication zones:

* **Zone A (Internal Mesh):** All Raft Cluster Nodes (Leader and Followers) communicate with each other for heartbeats, elections, and log replication.
* **Zone B (Ingress):** Client Nodes initiate communication **only** with the current Raft Leader.
* **Zone C (Egress):** The Raft Leader initiates communication **only** with the AI Veto Node.

### 2. Restricted Paths

* **Client to AI:** Strictly forbidden. Clients cannot bypass the consensus group to query the "Moral Advocate" directly.
* **Follower to AI:** Strictly forbidden. To ensure cluster-wide determinism, only the Leader may query the AI. The AI's response is then replicated as a "fact" in the Raft log.
* **AI to Cluster:** The AI Node is a passive service; it only responds to requests initiated by the Raft Leader.

### 3. Leader Redirection Mechanism

If a Client attempts to communicate with a Follower:

* The Follower must reject the mutation request.
* The Follower should provide a "Hint" containing the `last_known_leader_id` and its network address.
* The Client is responsible for updating its internal "Current Leader" pointer and retrying the request.

## Rationale

* **Determinism:** Restricting AI queries to the Leader prevents "Split-Brain" scenarios where different nodes receive different AI responses for the same item.
* **Linearizability:** Forcing clients through the Leader ensures that all mutations are sequenced in a single, global order.
* **Security:** The Leader acts as a natural "Security Gateway," centralizing input validation and session management.
* **Simplicity:** By forbidding "Any-to-Any" paths, we reduce the number of RPC service definitions and connection states each node must manage.

## Consequences

### Pros

* **Consistency:** Guaranteed cluster-wide agreement on the "Moral Veto" status of an item.
* **Reduced Surface Area:** Fewer open ports and permitted communication paths simplify debugging and potential security hardening.
* **Architectural Clarity:** Clear separation between "Consensus Logic" and "External Policy."

### Cons

* **Leader Bottleneck:** All external traffic (Client and AI) flows through a single node, which could limit throughput in high-load scenarios (though negligible for a grocery list).
* **Client Complexity:** Clients must implement "Leader Discovery" and "Redirection" logic rather than simply "Fire and Forget."
* **Higher Latency:** Mutations require an extra network hop (Leader -> AI -> Leader) before the consensus phase begins.

### Operational Impact

* **Discovery:** The system requires a mechanism for nodes and clients to initiallly find at least one member of the cluster (to be discussed in a future ADR).
