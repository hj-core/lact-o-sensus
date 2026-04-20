# ADR 005: System-Wide Logical Interface Specification

## Metadata

- **Date:** 2026-04-09
- **Status:** Proposed
- **Scope:** Logical RPC Contracts and Service Definitions
- **Primary Goal:** Define a consistent, typed interface for all inter-node communication, ensuring cluster isolation and semantic integrity.
- **Last Updated:** 2026-04-20

## Context

Lact-O-Sensus consists of three distinct interaction domains: internal consensus, external user ingress, and policy egress. Per ADR 007 and ADR 008, our interface must support AI-driven semantic resolution and internal SI stabilization while maintaining the strict cluster identity mandates of ADR 004.

## Decision

We will define three logical services with strict contracts. All messages must include the `cluster_id` to prevent cross-environment contamination.

### 1. The Consensus Service (Internal Mesh)

Used for Raft peer-to-peer communication.

- **`RequestVote`**:
  - **Input:** `cluster_id`, `target_node_id`, `term`, `candidate_id`, `last_log_index`, `last_log_term`.
  - **Output:** `cluster_id`, `node_id` (Responder), `term`, `vote_granted`.
- **`AppendEntries`**:
  - **Input:** `cluster_id`, `target_node_id`, `term`, `leader_id`, `prev_log_index`, `prev_log_term`, `entries[]`, `leader_commit`.
  - **Output:** `cluster_id`, `node_id` (Responder), `term`, `success`, `last_log_index`.
- **`LogEntry`**:
  - **Structure:** `term`, `index`, `data` (Serialized `CommittedMutation`).

### 2. The Ingress Service (Client-to-Leader)

Used for user mutations and queries.

- **`ProposeMutation`**:
  - **Input:** `cluster_id`, `target_node_id`, `client_id`, `sequence_id`, `MutationIntent`.
  - **Output:** `cluster_id`, `node_id` (Leader), `status` (Committed/Rejected/Vetoed), `state_version`, `leader_hint`, `error_message`.
- **`QueryState`**:
  - **Input:** `cluster_id`, `target_node_id`, `query_filter` (optional), `min_state_version` (optional).
  - **Output:** `cluster_id`, `node_id` (Responder), `item_list[]` (of `GroceryItem`), `current_state_version`, `status`, `leader_hint`, `error_message`.

### 3. The Policy Service (Leader-to-AI)

Used for semantic resolution and physical verification.

- **`EvaluateProposal`**:
  - **Input:** `cluster_id`, `target_node_id`, `client_id`, `normalized_intent`, `current_inventory[]`, `request_context`.
  - **Output:**
    - `cluster_id`, `node_id` (The AI Node ID), `is_approved`, `moral_justification`.
    - **Semantic Data:** `resolved_item_key`, `suggested_display_name`, `category_assignment`, `resolved_unit`.
    - **Conversion Data:** `conversion_multiplier_to_base` (Decimal string).

### 4. The Replicated Ledger Entry (`CommittedMutation`)

The serialized binary format stored in the Raft log and database. This represents the "Final Truth."

- **Mandate (Absolute State):** The Leader is exclusively responsible for performing all physical arithmetic (Base SI \* Multiplier). The log entry must record the **Absolute Result** (not the delta) to ensure state machine idempotency.
- **Precision Policy:** All numeric quantities and multipliers MUST be transmitted and stored as **Stringified Fixed-Point Decimals** to avoid IEEE 754 non-determinism across different architectures.

- **Identity:** `resolved_item_key` (Canonical Slug).
- **Display:** `suggested_display_name` (UI Metadata).
- **State:** `updated_base_quantity` (Absolute Result in SI as Decimal string), `base_unit` (Canonical SI Symbol), `display_unit` (User-preferred symbol), `updated_category` (Metadata).
- **Session:** `client_id`, `sequence_id` (For Exactly-Once Semantics).
- **Audit:** `raw_user_input` (Original Intent), `moral_justification` (AI Rationale), `event_time` (Timestamp).

## Rationale

- **Identity Guarding:** Mandating `cluster_id` in every RPC ensures that nodes and clients never accidentally process traffic from a foreign cluster.
- **Separation of Concerns:** The `MutationIntent` captures human ambiguity, while the `CommittedMutation` captures deterministic physical and taxonomic state.
- **Semantic Oracle Integration:** The Policy Service is the only point where non-determinism (the AI) is allowed to influence the state before it is codified as a log entry.

## Consequences

### Pros

- **Auditability:** Every ledger entry carries its complete causal history (raw input + AI reasoning).
- **Interoperability:** Decoupled interfaces allow different client or AI implementations to be swapped as long as they follow the contract.
- **Protocol Safety:** Full Raft RPC definitions prevent ambiguity during election or replication phases.

### Cons

- **Rigidity:** Schema changes require coordinated updates across all node types.
- **Overhead:** Large log entries (due to raw strings and justifications) increase disk I/O and storage requirements.

### Operational Impact

- **Schema Evolution:** Since the ledger format (`CommittedMutation`) is persistent, any changes to the logical interface must support backward compatibility (e.g., Protobuf optional fields) to allow older logs to be replayed by newer binaries.
- **Observability:** The inclusion of audit metadata (original intent and AI rationale) significantly simplifies debugging but requires monitoring for log-induced disk pressure.
- **Protocol Drift:** Strict validation of `cluster_id` and `node_id` simplifies the isolation of environmental issues but requires rigorous configuration management during cluster deployment.
