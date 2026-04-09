# ADR 005: System-Wide Logical Interface Specification

## Metadata

* **Date:** 2026-04-09
* **Status:** Proposed
* **Scope:** Logical RPC Contracts and Service Definitions
* **Primary Goal:** Define a consistent, typed interface for all inter-node communication.

## Context

Lact-O-Sensus consists of three distinct interaction domains: internal consensus, external user ingress, and policy egress. To ensure cluster isolation and type safety, we must define a logical interface that all participants must adhere to. Per ADR 004, every message must include a `cluster_id` to prevent cross-cluster contamination.

## Decision

We will define three logical services, each with a strict request/response contract.

### 1. The Consensus Service (Internal Mesh)

Used for Raft peer-to-peer communication.

* **`RequestVote`**:
  * **Input**: `cluster_id`, `term`, `candidate_id`, `last_log_index`, `last_log_term`.
  * **Output**: `cluster_id`, `term`, `vote_granted`.
* **`AppendEntries`**:
  * **Input**: `cluster_id`, `term`, `leader_id`, `prev_log_index`, `prev_log_term`, `entries[]`, `leader_commit`.
  * **Output**: `cluster_id`, `term`, `success`.

### 2. The Ingress Service (Client-to-Leader)

Used for user mutations and queries.

* **`ProposeMutation`**:
  * **Input**: `cluster_id`, `client_id`, `sequence_id`, `operation`, `item_data`.
  * **Output**: `cluster_id`, `status` (Committed/Rejected/Vetoed), `state_version`, `leader_hint` (for redirection), `error_message`.
* **`QueryState`**:
  * **Input**: `cluster_id`, `query_type`, `filter`, `min_state_version` (optional).
  * **Output**: `cluster_id`, `item_list[]`, `current_state_version`.

### 3. The Policy Service (Leader-to-AI)

Used for automated evaluation and classification.

* **`EvaluateProposal`**:
  * **Input**: `cluster_id`, `item_data`, `request_context`.
  * **Output**: `cluster_id`, `is_approved`, `category_assignment`, `moral_justification`.

## Rationale

* **Cluster Isolation**: Mandating the `cluster_id` in every request/response provides a lightweight but effective security boundary at the application layer.
* **Protocol Completeness**: The Consensus Service provides the minimum necessary methods to implement the Raft protocol as defined in the original paper.
* **Leader-Centricity**: The Ingress and Policy services are designed to be handled primarily by the Leader, reinforcing the topology defined in ADR 002.
* **Logical Decoupling**: These definitions focus on "what" data is exchanged, remaining agnostic of the underlying serialization (e.g., Protobuf) or transport (e.g., gRPC).

## Consequences

### Pros

* **Strong Typing**: Clear definitions prevent malformed data from reaching the core consensus logic.
* **Auditability**: Every message carries the metadata (`cluster_id`, `client_id`, etc.) necessary for a full audit trail.
* **Interoperability**: Different implementations of a node (e.g., a CLI client vs. a mobile client) can interact with the cluster as long as they follow this specification.

### Cons

* **Message Overhead**: Including the `cluster_id` in every packet adds a small amount of redundant data to high-frequency heartbeats.
* **Rigidity**: Changing the interface requires coordinated updates (and potentially schema migration) across all nodes.

### Operational Impact

* **Service Discovery**: Clients must be aware of the `cluster_id` they are attempting to interact with.
* **Logging**: The `cluster_id` should be included in all structured logs for multi-tenant observability.
