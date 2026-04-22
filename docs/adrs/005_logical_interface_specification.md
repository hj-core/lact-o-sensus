# ADR 005: System-Wide Logical Interface Specification

## Metadata

- **Date:** 2026-04-09
- **Status:** Proposed
- **Scope:** Logical RPC Contracts and Service Definitions
- **Primary Goal:** Define a consistent, typed interface for all inter-node communication, ensuring cluster isolation and semantic integrity.
- **Last Updated:** 2026-04-22

## Context

Lact-O-Sensus consists of three distinct interaction domains: internal consensus, external user ingress, and policy egress. Per ADR 007 and ADR 008, our interface must support AI-driven semantic resolution and internal SI stabilization while maintaining the strict cluster identity mandates of ADR 004.

## Decision

We will define three logical services with strict contracts, decoupled into two distinct protobuf definitions to separate the generic consensus engine from the grocery application logic. All messages must include the `cluster_id` to prevent cross-environment contamination.

### 1. The Generic Consensus Interface (`raft.proto`)

Used exclusively for Raft peer-to-peer communication. This interface is domain-agnostic and replicates opaque byte payloads.

- **`ConsensusService` (Package: `raft.v1`)**:
  - **`RequestVote`**:
    - **Input:** `cluster_id`, `target_node_id`, `term`, `candidate_id`, `last_log_index`, `last_log_term`.
    - **Output:** `cluster_id`, `node_id` (Responder), `term`, `vote_granted`.
  - **`AppendEntries`**:
    - **Input:** `cluster_id`, `target_node_id`, `term`, `leader_id`, `prev_log_index`, `prev_log_term`, `entries[]`, `leader_commit`.
    - **Output:** `cluster_id`, `node_id` (Responder), `term`, `success`, `last_log_index`.
- **`LogEntry`**:
  - **Structure:** `term`, `index`, `data` (Opaque `bytes` containing a serialized application-level entry).

### 2. The Application Interface (`app.proto`)

Used for external client ingress, policy egress, and the internal state machine representation.

#### A. The Ingress Service (Client-to-Leader)

Used for user mutations and queries. (Package: `lacto_sensus.v1`)

- **`ProposeMutation`**:
  - **Input:** `cluster_id`, `target_node_id`, `client_id`, `sequence_id`, `MutationIntent`.
  - **Output:** `cluster_id`, `node_id` (Leader), `status` (Committed/Rejected/Vetoed), `state_version`, `leader_hint`, `error_message`.
- **`QueryState`**:
  - **Input:** `cluster_id`, `target_node_id`, `query_filter` (optional), `min_state_version` (optional).
  - **Output:** `cluster_id`, `node_id` (Responder), `item_list[]` (of `GroceryItem`), `current_state_version`, `status`, `leader_hint`, `error_message`.

#### B. The Policy Service (Leader-to-AI)

Used for semantic resolution and physical verification. (Package: `lacto_sensus.v1`)

- **`EvaluateProposal`**:
  - **Input:** `cluster_id`, `target_node_id`, `client_id`, `normalized_intent`, `current_inventory[]`, `request_context`.
  - **Output:**
    - `cluster_id`, `node_id` (The AI Node ID), `is_approved`, `moral_justification`.
    - **Semantic Data:** `resolved_item_key`, `suggested_display_name`, `category_assignment`, `resolved_unit`.
    - **Conversion Data:** `conversion_multiplier_to_base` (Decimal string).

#### C. The Replicated Ledger Entry (`CommittedMutation`)

The serialized binary format stored as `bytes` within the Raft `LogEntry`. This represents the "Final Truth" of the grocery state.

- **Mandate (Absolute State):** The Leader is exclusively responsible for performing all physical arithmetic (Base SI * Multiplier). The log entry must record the **Absolute Result** (not the delta) to ensure state machine idempotency.
- **Precision Policy:** All numeric quantities and multipliers MUST be transmitted and stored as **Stringified Fixed-Point Decimals** to avoid IEEE 754 non-determinism across different architectures.

- **Identity:** `resolved_item_key` (Canonical Slug).
- **Display:** `suggested_display_name` (UI Metadata).
- **State:** `updated_base_quantity` (Absolute Result in SI as Decimal string), `base_unit` (Canonical SI Symbol), `display_unit` (User-preferred symbol), `updated_category` (Metadata).
- **Session:** `client_id`, `sequence_id` (For Exactly-Once Semantics).
- **Audit:** `raw_user_input` (Original Intent), `moral_justification` (AI Rationale), `event_time` (Timestamp).

## Rationale

- **Protocol Reusability:** By splitting `raft.proto` from `app.proto`, the consensus engine becomes a domain-agnostic "black box." This ensures the core Raft implementation can be reused for any distributed state machine project.
- **Contractual Clarity:** Developers working on the Raft core only need to understand the simple consensus state machine, while application developers focus on the rich grocery schema.
- **Identity Guarding:** Mandating `cluster_id` in every RPC ensures that nodes and clients never accidentally process traffic from a foreign cluster.
- **Separation of Concerns:** The `MutationIntent` captures human ambiguity, while the `CommittedMutation` captures deterministic physical and taxonomic state.

## Consequences

### Pros

- **Engine Portability:** The Raft implementation is now 100% domain-agnostic and reusable.
- **Parallel Evolution:** The consensus protocol and the grocery schema can evolve independently.
- **Auditability:** Every ledger entry carries its complete causal history (raw input + AI reasoning).

### Cons

- **Serialization Overhead:** Mutations require "Double Serialization" (Application -> Bytes -> Raft LogEntry) and corresponding deserialization on followers.
- **Rigidity:** Schema changes across the split boundary require coordinated updates across all node types.

### Operational Impact

- **Schema Evolution:** Since the ledger format (`CommittedMutation`) is persistent, any changes to the logical interface must support backward compatibility (e.g., Protobuf optional fields) to allow older logs to be replayed by newer binaries.
- **Observability:** The inclusion of audit metadata (original intent and AI rationale) significantly simplifies debugging but requires monitoring for log-induced disk pressure.
- **Protocol Drift:** Strict validation of `cluster_id` and `node_id` simplifies the isolation of environmental issues but requires rigorous configuration management during cluster deployment.
