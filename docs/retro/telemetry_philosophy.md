# Clinical Telemetry: Design Philosophy & Standards

This document formalizes the "Clinical Telemetry" model established during the Lact-O-Sensus implementation. This philosophy moves away from "logging as a debugging byproduct" toward **"telemetry as a first-class architectural layer."**

## Core Pillars

### 1. The Single Source of Truth (SSOT) Registry

Never use raw string literals for log categories or targets.

- **Standard:** Every valid telemetry category must be centralized in a dedicated registry (e.g., the `ClinicalTarget` enum in `common`).
- **Application:** For `#[instrument]`, use the literal string value from the registry. For every `info!`, `warn!`, or `error!`, use the `registry.as_str()` method. This ensures a single change in the registry updates the entire system's "audit map."

### 2. Single Decision, Single Span

Eliminate "Contextual Echo" by instrumenting the **Orchestrator**, not the **Implementer**.

- **Standard:** A span must represent a semantic decision or a high-impact operation, not just a function call.
- **Application:** Instrument the _Logical Layer_ (where consensus decisions happen). Avoid instrumenting the _Physical Layer_ (mechanical side-effects of decisions). This prevents redundant nested spans (e.g., `transition:transition`) and keeps logs readable.

### 3. Causal Birth & Trace Propagation

Distributed systems must have a traceable lineage for every request.

- **Standard:** Every request has a "Clinical Birth" at the system boundary. That identity (`trace_id`) is sacred and must be propagated across every thread, async task, and network RPC.
- **Application:** Use gRPC Interceptors for automatic extraction/injection. In internal logic, always use `.instrument(span)` when spawning tasks to ensure the causal chain is never broken by the task scheduler.

### 4. Semantic Redaction (Information Opacity)

Logs must tell you _what_ happened and _how_ long it took without leaking _who_ did it or _why_ (if "why" is sensitive).

- **Standard:** Treat log files as potential PII leaks. Redact by default at `INFO` levels.
- **Application:**
  - **RIDs over Data:** Log `LogIndex` and `Term` (Raft Coordinates) but relegate raw user payloads or AI justifications to the `TRACE` level.
  - **Truncated Identifiers:** Never log full UUIDs for Client IDs. Use "Correlation-Safe Truncation" (e.g., first 8 characters) to link events without storing sensitive identity data.

### 5. Structured Failure Forensics

The "Halt Mandate" must be forensic, not just a crash.

- **Standard:** A `panic!` is a failure of the runtime; a structured `error!` event before the panic is a diagnostic gift.
- **Application:** Every path leading to a `panic!` must emit a structured event containing the physical state (e.g., last committed index, peer ID, error code). This ensures the "Flight Recorder" captures the black-box state at the moment of death.

---

## The "Clinical" Audit Checklist

Use this checklist when reviewing any new instrumentation:

1. **Is it Registered?** Does the target exist in the `ClinicalTarget` registry?
2. **Is it a Decision?** Is the span representing a semantic choice or just a function call?
3. **Is the Chain Intact?** Does this span/event inherit the current `trace_id`?
4. **Is it Opaque?** Are you logging a coordinate/slug or a piece of PII?
5. **Is the Failure Forensic?** If this crashes, will the logs tell you the _exact_ internal state?
