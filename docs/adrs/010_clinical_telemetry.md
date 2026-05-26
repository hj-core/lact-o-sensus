# ADR 010: Clinical Telemetry & Structured Observability

## Metadata

- **Date:** 2026-05-18
- **Status:** Proposed
- **Scope:** System-wide Instrumentation and Diagnostics
- **Primary Goal:** Transition from unstructured logging to a clinical-grade structured telemetry framework that enables deterministic reconstruction of distributed events while strictly preserving information opacity.
- **Last Updated:** 2026-05-27

## Context

Current instrumentation relies on unstructured `info!`, `warn!`, and `error!` macros. While sufficient for basic debugging, this approach fails the "Clinical Rigor" test in three ways:

1. **Causal Ambiguity:** It is difficult to link network RPCs to internal state transitions or subsequent state machine mutations across node boundaries.
2. **Physical Obscurity:** "Physical Truth" (ADR 008) is often logged as stringified blobs, making it impossible for automated tools to audit stabilization or rounding logic.
3. **Implicit Latency:** There is no standard way to measure the "Moral Evaluation" overhead (ADR 005) or Raft role durations without manual timer injections.
4. **Information Disclosure:** Standard logging of domain objects often leaks PII (e.g., full Client IDs, raw user input, or AI moral justifications) into persistent log files, violating the "Information Opacity" mandate (ADR 006).

## Decision

We will adopt a **Structured Event & Lifecycle Span** model powered by the `tracing` ecosystem, with mandatory redaction layers.

### 1. The "Clinical Event" Schema

All events must utilize a standardized set of `target` and `kind` fields:

- **`raft::foundation`**: Core consensus state (Term advances, Role transitions, VotedFor persistence).
- **`raft::replication`**: Log maintenance (AppendEntries reconciliation, Commit index advances).
- **`raft::compaction`**: Log truncation, snapshot generation, and physical disk reclamation (ADR 011).
- **`clinical::ingress`**: The 5-Layer Defense Onion (ADR 007) lifecycle.
- **`clinical::fsm`**: Physical state mutations and stabilization (ADR 008).
- **`clinical::foundation`**: Node startup, identity verification (ADR 004), storage initialization, and lifecycle management.
- **`clinical::recovery`**: State machine synchronization and log replay during node startup.
- **`clinical::oracle`**: Semantic resolution, policy evaluation, and LLM latency tracking.
- **`clinical::telemetry`**: Trace verification, Byzantine grafting detection, and protocol integrity checks.

### 2. Standardized Field Naming

Mandatory fields for telemetry correlation:

- `trace_id`: Distributed trace identifier (UUID v7) for causal correlation.
- `cluster_id`, `node_id`: Identity correlation.
- `term`, `index`: Raft coordinates.
- `last_included_index`, `last_included_term`: Snapshot coordinates for log compaction auditing.
- `client_id`, `seq`: Session/Linearizability coordinates (ADR 006).
- `resolution`: Outcomes of the Semantic Oracle (e.g., `Approved`, `Vetoed`).

### 3. Lifecycle Spans (Duration & Context)

- **Role Spans**: Each Raft state (`Follower`, `Candidate`, `Leader`) will be a span.
- **RPC Spans**: Every gRPC handler will enter a span containing the `trace_id` and `sender_id`.
- **Evaluation Spans**: The AI Veto relay will wrap its calls in a span to track "Moral Latency," inheriting the `trace_id`.

### 4. Data Redaction & Clinical Sealing

To preserve information opacity, the following redaction rules are MANDATORY:

- **Client ID Redaction**: `client_id` must never be logged in its raw form in the `message` or as a full string in structured fields. It MUST be logged as a **Correlation-Safe Truncation** (the first 8 characters of the UUID).
- **Moral Advocate Output (PII)**: Full AI-generated `moral_justification` and `raw_user_input` strings are classified as PII. They MUST ONLY be logged in full at the `TRACE` level. At `INFO` or `WARN` levels, these fields must be omitted or replaced with a generic "PII-Redacted" placeholder.
- **Registry Slugs over Raw Strings**: At the `INFO` level, physical mutations must be logged using their canonical **Registry Slugs** (e.g., `inventory.dairy.milk`) rather than the raw user-provided item keys to prevent leak of intent.

### 5. Distributed Trace Propagation & Authority

We will implement a `TraceInterceptor` to propagate `trace_id` via gRPC metadata, linking a client's mutation intent through the gateway, AI Veto relay, and Raft consensus log.

- **Gateway Authority:** To adhere to the Byzantine Client model (ADR 001), the Gateway node is the **Authoritative Generator** of the `trace_id` (UUID v7). Any `x-trace-id` provided by an external client MUST be ignored.
- **Clinical Birth:** The moment a request enters the Ingress Service, it is assigned a `trace_id` which defines its "Clinical Birth." This ID is then propagated to internal cluster services (AI Veto, Raft Engine) to ensure causal correlation.
- **Trace Feedback:** The generated `trace_id` should be returned to the client in the gRPC response headers to provide a correlation handle for external troubleshooting.

## Rationale

- **Clinical Auditability:** Standardized fields allow a "Black Box" flight recorder to reconstruct event chains without leaking sensitive user data.
- **Security-by-Default:** By classifying AI output and raw input as `TRACE`-only, we ensure that standard production logs do not become a source of PII leakage.
- **Alignment with ADR 006/008:** Respects both the "Information Opacity" (redaction) and "Physical Truth" (structured stabilization data) mandates.

## Consequences

### Pros

- **Deterministic Reconstruction:** Enables full-trace analysis of clinical events.
- **PII Protection:** Strict redaction rules prevent accidental leakage of user intent into logs.
- **Performance Insight:** Identifies bottlenecks in the AI Veto egress or Log I/O.

### Cons

- **Debugging Friction:** Debugging complex AI vetoes in production may require elevating the log level to `TRACE`, potentially increasing log volume.
- **Implementation Rigor:** Requires manual adherence to redaction rules in every `event!` call.
