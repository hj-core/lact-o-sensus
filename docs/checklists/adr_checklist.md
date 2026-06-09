# Lact-O-Sensus ADR Content Checklist

## Instruction to the Authoring LLM

You will author a new Architectural Decision Record (ADR). First, read this checklist completely. Every ADR you generate must strictly adhere to the requirements under these sections. For your output, structure the ADR around these rules and explicitly cite the Rule IDs (e.g., `[CTX-01]`, `[DEC-02]`) to prove compliance. If a structural or content issue is NOT covered below, still address it using a descriptive placeholder Rule ID (e.g., `[CUSTOM-01]`).

---

## 1. Context & Problem Framing [CTX]

### [CTX-01] Explicit Problem Statement

- **Target Scope**: ADR "Context" section
- **Severity**: CRITICAL
- **DO**: State a specific, well-scoped architectural problem or motivation that drives the decision.
- **DO NOT**: Describe a solution before the problem is clearly defined, or use vague motivations like "improve maintainability" without supporting evidence.

### [CTX-02] Sufficient Background

- **Target Scope**: ADR "Context" section
- **Severity**: WARNING
- **DO**: Provide enough background information that a team member unfamiliar with the area can understand why the decision was necessary.
- **DO NOT**: Assume deep prior knowledge of the codebase or omit external references that would aid understanding.

### [CTX-03] Decision Driver Traceability

- **Target Scope**: ADR "Context" section
- **Severity**: WARNING
- **DO**: Reference the specific forces driving the decision — e.g., scalability constraints, safety requirements, compliance mandates, or prior ADRs being superseded.
- **DO NOT**: State the decision in isolation without linking it to the architectural forces that demand it.

---

## 2. Options & Trade-Off Analysis [OPT]

### [OPT-01] Viable Alternatives

- **Target Scope**: ADR "Options Considered" section
- **Severity**: CRITICAL
- **DO**: Document at least 2–3 viable alternatives, including "do nothing" as a baseline.
- **DO NOT**: Present a single option as a foregone conclusion without acknowledging alternatives.

### [OPT-02] Explicit Trade-Off Matrix

- **Target Scope**: ADR "Options Considered" section
- **Severity**: CRITICAL
- **DO**: List concrete trade-offs (cost, complexity, scalability, operational burden, safety) for each alternative.
- **DO NOT**: Use vague comparative statements like "Option A is better" without specifying the criteria and evidence.

---

## 3. Decision & Rationale [DEC]

### [DEC-01] Single, Clear Decision

- **Target Scope**: ADR "Decision" section
- **Severity**: CRITICAL
- **DO**: Select exactly one option as the decision and state it in unambiguous, affirmative language.
- **DO NOT**: Use hedging language ("we might", "probably", "consider") or leave the decision open-ended.

### [DEC-02] Rationale With Evidence

- **Target Scope**: ADR "Decision" section
- **Severity**: CRITICAL
- **DO**: Justify the chosen option with concrete reasoning tied to the trade-offs listed under [OPT-02]. Reference authoritative sources (papers, standards, prior ADRs) where applicable.
- **DO NOT**: Base the decision on personal preference, team politics, or unsupported assertions.

### [DEC-03] Assumptions & Constraints

- **Target Scope**: ADR "Decision" section
- **Severity**: WARNING
- **DO**: Document key assumptions made during the decision and constraints that bound the solution space (e.g., "we assume ≤ 7 nodes", "must support crash-recovery, not byzantine").
- **DO NOT**: Bury critical assumptions in prose without highlighting them as explicit decision dependencies.

---

## 4. Consequences & Follow-Up [CONSEQ]

### [CONSEQ-01] Positive & Negative Consequences

- **Target Scope**: ADR "Consequences" section
- **Severity**: CRITICAL
- **DO**: Enumerate both positive consequences (gains, simplifications, capabilities enabled) and negative consequences (technical debt, restrictions, migration/operational cost).
- **DO NOT**: List only benefits while omitting downsides, trade-offs deferred, or known limitations.

### [CONSEQ-02] Follow-Up Actions

- **Target Scope**: ADR "Consequences" section
- **Severity**: WARNING
- **DO**: Identify any follow-up actions, future work items, or conditions under which this decision should be revisited.
- **DO NOT**: Leave the reader wondering what must happen next for the decision to take effect.

---

## 5. Scope, Status & Governance [SCOPE]

### [SCOPE-01] Explicit Status

- **Target Scope**: ADR Header / Front Matter
- **Severity**: CRITICAL
- **DO**: Declare the current status — `Proposed`, `Accepted`, `Deprecated`, or `Superseded`.
- **DO NOT**: Publish or accept an ADR without an explicit status marker.

### [SCOPE-02] Boundary Definition

- **Target Scope**: ADR "Scope" section or header
- **Severity**: WARNING
- **DO**: Define which system boundaries, layers, or components the decision governs.
- **DO NOT**: Leave scope implicit; the reader should know exactly what is covered and what is not.

### [SCOPE-03] Supersession Chain

- **Target Scope**: ADR Header / Front Matter
- **Severity**: CRITICAL
- **DO**: If superseding a previous ADR, reference the prior ADR by document ID and briefly state why the new decision overrides it.
- **DO NOT**: Orphan superseded ADRs without a clear pointer to their replacement.

---

## 6. Quality & Readability [QUAL]

### [QUAL-01] Single Decision Scope

- **Target Scope**: Entire ADR
- **Severity**: CRITICAL
- **DO**: Scope the ADR to exactly one architectural decision. If multiple decisions are related, create separate ADRs.
- **DO NOT**: Combine multiple independent decisions into a single ADR (this is a design document, not an ADR).

### [QUAL-02] Concise Length

- **Target Scope**: Entire ADR
- **Severity**: WARNING
- **DO**: Keep the ADR short enough to be read and understood in under 10 minutes.
- **DO NOT**: Exceed ~500–800 words of prose unless the complexity genuinely requires it.

### [QUAL-03] Precise Language

- **Target Scope**: Entire ADR
- **Severity**: WARNING
- **DO**: Use precise, unambiguous language. Favor "shall", "will", and "must" over "should", "might", "could".
- **DO NOT**: Use vague or weasel words that obscure the commitment level of the decision.

### [QUAL-04] External Reference Integrity

- **Target Scope**: Entire ADR
- **Severity**: STYLE
- **DO**: Link to relevant external references (papers, standards, prior ADRs, specification documents) where they support the rationale.
- **DO NOT**: Reference external resources without explaining why they are relevant.

---

## 7. Anti-Patterns [AVOID]

### [AVOID-01] Implementation-Level Detail

- **Target Scope**: Entire ADR
- **Severity**: CRITICAL
- **DO**: Stay at the architectural boundary. Describe _what_ was decided, _why_, and _within which scope_.
- **DO NOT**: Prescribe implementation details — class names, function signatures, config file formats, library versions, or code snippets (unless essential to illustrate an interface contract).

### [AVOID-02] Design Document Drift

- **Target Scope**: Entire ADR
- **Severity**: CRITICAL
- **DO**: Restrict content to architectural rationale. Defer detailed design, API contracts, and migration plans to separate technical specification documents.
- **DO NOT**: Expand an ADR into a full design document covering how-to-implement instructions.

### [AVOID-03] Opinion Over Evidence

- **Target Scope**: Entire ADR
- **Severity**: WARNING
- **DO**: Base the decision on technical evidence, documented trade-offs, and measurable criteria.
- **DO NOT**: Rely on personal authority ("the lead architect prefers"), popularity ("widely used"), or unsubstantiated claims.
