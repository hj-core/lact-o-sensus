# ADR 001: Node Failure Model and Reliability Guarantees

## Metadata

* **Date:** 2026-04-09
* **Status:** Proposed
* **Scope:** Node Reliability and Failure Tolerance
* **Primary Goal:** Ensure system integrity across crashes and provide linearizable semantics to clients.

## Context

Lact-O-Sensus is a distributed system managing a replicated state machine (grocery list). To ensure data integrity and system liveness, we must define the failure modes each node type is expected to handle. The three node types—Client, Raft Cluster, and AI Veto—operate in different trust and failure domains.

## Decision

We will adopt a layered failure model, transitioning from a "Honest Crash-Recovery" baseline to a "Fortress" model that handles external Byzantine behavior.

### 1. Raft Cluster Nodes: Crash-Recovery (CR)

* **Model:** Non-Byzantine, Crash-Recovery.
* **Assumption:** Nodes follow the Raft protocol but may stop and restart with persistent state intact.
* **Mandate:**
  * Persist critical state (`currentTerm`, `votedFor`, `log[]`) to stable storage before responding to RPCs.
  * Replay the persistent log on recovery to reconstruct the State Machine.

### 2. Client Nodes: Stateful Recovery & Linearizability

* **Model:** Initially Crash-Recovery, evolving to Byzantine-Robust.
* **Mandate:**
  * Provide `client_id` and monotonic `sequence_id` for deduplication.
  * Persist pending requests locally to handle client crashes.

### 3. AI Veto Node: Byzantine Oracle

* **Model:** Byzantine-Faulty (Non-Deterministic).
* **Mandate:**
  * Treat as an "Unreliable Oracle" due to LLM non-determinism.
  * Only the Raft Leader interacts with the AI Node to ensure cluster-wide determinism.

### 4. Input Validation (The "Fortress" State Machine)

* **Mandate:** The Raft Leader performs strict schema and "moral" validation on all inputs before proposing them to the cluster.

## Rationale

* **Raft Compatibility:** The Raft protocol is mathematically proven for the Crash-Recovery model. Straying into BFT for the core consensus would exceed the 3-month project scope.
* **LLM Reality:** LLMs are inherently non-deterministic. Treating the AI Node as Byzantine protects the deterministic nature of the state machine.
* **End-to-End Reliability:** Standard "Best Effort" clients cannot guarantee exactly-once semantics if the client itself crashes. Local persistence is required for true linearizability.

## Consequences

### Pros

* **High Academic Rigor:** Adheres strictly to the Raft paper's safety requirements.
* **Robustness:** System survives standard network partitions and hardware reboots without data loss.
* **Modular Security:** By treating external nodes as Byzantine, we create a secure perimeter around the consensus group.

### Cons

* **Performance Latency:** Synchronous disk I/O ("Sync-before-ACK") will significantly slow down throughput.
* **Implementation Complexity:** Requires building a robust local WAL for the client and a recovery manager for the cluster nodes.
* **Internal Vulnerability:** The system remains vulnerable to "traitor" cluster nodes (malicious Raft participants).

### Operational Impact

* **Storage:** Requires reliable filesystem access for all node types.
* **Testing:** Necessitates "Chaos Testing" to verify recovery logic.
