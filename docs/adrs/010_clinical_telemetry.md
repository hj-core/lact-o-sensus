# ADR 010: Clinical Telemetry & Structured Observability

## Metadata

- **Date:** 2026-05-18
- **Status:** Accepted
- **Scope:** System-wide instrumentation and diagnostics; excludes client-side logging, third-party monitoring integrations, and audit-log retention policies.
- **Primary Goal:** Transition from unstructured logging to a clinical-grade structured telemetry framework that enables deterministic reconstruction of distributed events while strictly preserving information opacity.
- **Last Updated:** 2026-06-09

## Context

Current instrumentation relies on unstructured text logs. While sufficient for basic debugging, this approach fails clinical rigor in four ways:

1. **Causal Ambiguity:** It is difficult to link network RPCs to internal state transitions or subsequent state machine mutations across node boundaries.
2. **Physical Obscurity:** Physical quantities (ADR 008) are often logged as stringified blobs, making it impossible for automated tools to audit stabilization or rounding logic.
3. **Implicit Latency:** There is no standard way to measure the Defense Onion pipeline overhead (ADR 007) or Raft role durations without manual timer injection.
4. **Information Disclosure (ADR 006):** Standard logging of domain objects leaks sensitive data — full client IDs, raw user input, AI moral justifications — into persistent log files, violating the information opacity mandate.

These four problems share a root cause: telemetry lacks structure, so it cannot be filtered, correlated, or redacted programmatically.

## Options Considered

- **Option A: Unstructured logging (baseline).** Continue with free-text `info!`/`warn!`/`error!` calls. Rejected because it provides no mechanism for causal correlation across nodes, no structured fields for automated audit tooling, and no enforceable redaction layer.
- **Option B: Structured event log with mandatory fields.** Add a structured event schema with required correlation fields (`trace_id`, `node_id`, `term`, etc.) but no distributed trace propagation. Rejected because it still cannot link causally related events across node boundaries — a mutation's path from Gateway through Leader to AI Oracle and back remains opaque.
- **Option C: Distributed trace propagation with structured events (chosen).** Combine a structured event schema with a propagated trace identifier, allowing end-to-end reconstruction of a mutation's lifecycle. Structured fields enable automated redaction and audit tooling.
- **Option D: Full observability platform (OpenTelemetry).** Adopt OpenTelemetry for traces, metrics, and logs. Rejected as over-engineering for a ≤7-node cluster; the operational burden of running an OTEL collector and backend exceeds the benefit.

## Decision

We will adopt a **structured event model with distributed trace propagation**, where every significant system event records a standardized set of correlation fields and a propagated trace identifier links causally related events across node boundaries. All domain-specific payloads are subject to mandatory redaction rules.

### 1. Event Schema

All events must carry a standardized set of correlation fields:

- **Trace identifier:** A unique value for distributed causal correlation, generated at the system boundary.
- **Node identity:** Cluster and node identifiers for topological correlation.
- **Raft coordinates:** Term and index values for consensus-level correlation.
- **Session coordinates:** Client identifier and sequence number for linearizability correlation (ADR 006).
- **Outcome:** The resolution status of any evaluation or mutation (e.g., approved, vetoed).

Events are grouped into high-level categories corresponding to system domains: consensus core (term advances, role transitions, log replication); the Defense Onion pipeline (ADR 007); physical state mutations (ADR 008); node lifecycle; and AI Oracle interactions.

### 2. Lifecycle Spans (Duration & Context)

- **Role spans:** Each Raft role tenure (Follower, Candidate, Leader) is a time-bounded span for duration tracking.
- **RPC spans:** Every inbound gRPC handler enters a span linked to the request's trace identifier.
- **Evaluation spans:** AI Oracle relay calls are wrapped in spans to track semantic resolution latency, inheriting the caller's trace identifier.

### 3. Data Redaction & Clinical Sealing

To preserve information opacity (ADR 006), the following redaction rules are mandatory:

- **Client identity:** Full client identifiers must never appear in log output. They must be truncated to a correlation-safe prefix.
- **Domain payloads (PII):** Full AI moral justifications and raw user input strings are classified as sensitive. They must only be logged in full at the highest verbosity level. At standard operational levels, these fields must be omitted or replaced with a redaction placeholder.
- **Canonical identifiers over raw input:** At standard verbosity, physical mutations must be logged using their canonical registry slugs rather than the raw user-provided item keys.

### 4. Distributed Trace Propagation & Authority

- **Propagation:** A unique trace identifier is propagated via gRPC metadata, linking a client's mutation intent through the Gateway, AI Oracle relay, and Raft consensus log.
- **Gateway Authority:** To adhere to the Byzantine Client model (ADR 001), the Gateway node is the authoritative generator of the trace identifier. Any trace identifier provided by an external client must be ignored.
- **Clinical Birth:** The moment a request enters the Gateway's ingress, it is assigned a trace identifier which defines its causal origin. This identifier is then propagated to internal services (AI Oracle, Raft Engine) for end-to-end correlation.
- **Feedback:** The assigned trace identifier should be returned to the client in the gRPC response headers to provide a correlation handle for external troubleshooting.

## Rationale

- **Clinical Auditability:** Standardized correlation fields allow a "Black Box" flight recorder to reconstruct event chains without leaking sensitive user data. Option A provides no such capability.
- **Security-by-Default:** By classifying domain payloads as sensitive by default, we ensure that standard operational logs do not become a source of data leakage. Under Option A, every log line is a potential exposure.
- **Alignment with ADR 006/008:** The redaction rules respect the information opacity mandate, and the structured physical-state fields enable automated verification of ADR 008's rounding and stabilization logic.

## Assumptions and Constraints

- The cluster size is small (≤7 nodes), so the overhead of trace propagation (one additional gRPC metadata header per RPC) is negligible.
- Trace identifiers are generated by the Gateway only; internal nodes never originate a new trace. If a component needs to initiate work independent of a client request, a separate correlation mechanism is required.
- Redaction rules rely on developers correctly tagging sensitive fields at the call site; there is no automated static analysis to enforce compliance.

## Consequences

### Pros

- **Deterministic Reconstruction:** Enables full-trace analysis of clinical events across node boundaries.
- **PII Protection:** Strict redaction rules prevent accidental leakage of user intent into operational logs.
- **Performance Insight:** Identifies bottlenecks in the AI Oracle egress or log I/O via duration spans.

### Cons

- **Debugging Friction:** Debugging complex AI Oracle interactions may require elevating verbosity to include sensitive fields, increasing log volume and requiring careful access control.
- **Implementation Rigor:** Requires manual adherence to redaction rules in every event emission; a missed redaction creates a PII leak vector.

### Operational Impact

- **Log Volume:** Structured events are larger than unstructured text messages per event, but the ability to filter by category and field reduces the total volume ingested by downstream tooling.
- **Monitoring Setup:** Downstream observability tooling must be configured to index the structured fields for effective querying.

## Follow-Up

- Implement a linting rule or pre-commit hook that flags event emissions lacking the required correlation fields.
- Establish a periodic audit process for log samples to verify redaction rule compliance.
- Revisit the trace-propagation approach if the cluster grows beyond 7 nodes and the O(N) overhead of per-RPC propagation becomes measurable.

## References

- ADR 001 — Byzantine Client model motivating Gateway authority over trace identifiers.
- ADR 006 — Information opacity mandate informing the redaction rules.
- ADR 007 — Defense Onion pipeline whose lifecycle is captured by the event schema categories.
- ADR 008 — Physical state representation whose stabilization logic is auditable via structured fields.
