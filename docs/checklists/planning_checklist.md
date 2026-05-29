# Lact-O-Sensus Implementation Planning Checklist

## Instruction to the Planning LLM

You will generate a directional, highly detailed engineering plan for a specific development phase or major feature. First, read this checklist completely. Every implementation plan you generate must strictly adhere to the requirements under these sections. For your output, structure the phase plan around these rules and explicitly cite the Rule IDs (e.g., `[EXEC-01]`, `[BDD-02]`) to prove compliance.

---

## 1. Task Atomicity & Execution Sequence [EXEC]

### [EXEC-01] Atomic Task Chunking

- **Target Scope**: Commit Structure
- **Severity**: CRITICAL
- **DO**: Map every task in the plan directly to exactly **one atomic git commit**. Design each task to target a single logical change (ideally < 300 lines of code).
- **DO NOT**: Outline sprawling, multi-objective tasks that combine unrelated structural changes or violate atomic commit principles.

### [EXEC-02] Foundational Ordering

- **Target Scope**: Implementation Sequence
- **Severity**: STYLE
- **DO**: Order tasks foundationally, building from inner architectural layers outward (e.g., `common` -> `raft-engine` -> `lacto-fsm` -> `gateway`).
- **DO NOT**: Plan external delivery interfaces before defining, verifying, and testing the underlying domain contracts and consensus mechanisms.

---

## 2. Task Detail Requirements [DETAIL]

### [DETAIL-01] Comprehensive Task Specification

- **Target Scope**: Task Definitions
- **Severity**: CRITICAL
- **DO**: Explicitly define the following fields for every task:
  - **Goal**: A clear, concise statement of the clinical outcome.
  - **Affected Files**: A comprehensive list of files to be created or modified.
  - **Major Steps**: Specific implementation actions required.
  - **Consequences**: Impact on downstream components, architectural invariants, or public contracts.
  - **Caveats**: Known risks, assumptions, or transient state inconsistencies introduced.
- **DO NOT**: Leave task definitions vague or omit the analysis of downstream consequences and architectural risks.

---

## 3. Verification & Clinical Standards [VERIFY]

### [VERIFY-01] Mandatory CI Pipeline

- **Target Scope**: Task Verification
- **Severity**: CRITICAL
- **DO**: Explicitly include the execution of the full clinical verification sequence (`cargo +nightly fmt`, `cargo test`, `cargo clippy`, `python3 scripts/smoke_test.py`) as a mandatory final step for every task.
- **DO NOT**: Assume code correctness without running the established automated clinical checks.

### [VERIFY-02] Draft Commit Messaging

- **Target Scope**: Task Completion
- **Severity**: WARNING
- **DO**: Provide a draft Conventional Commit message (e.g., `feat(raft): implement pre-vote handler`) for each task.
- **DO NOT**: Leave the git commit strategy ambiguous or unformatted.

### [VERIFY-03] Orchestration Pattern

- **Target Scope**: Implementation Strategy
- **Severity**: WARNING
- **DO**: Design tasks so that major functions follow the orchestration pattern, delegating implementation details to specialized sub-functions.
- **DO NOT**: Plan monolithic functions that pack dense procedural logic into a single scope.

---

## 4. Behavior-Driven Development (BDD) Alignment [BDD]

### [BDD-01] Red-Green Protocol

- **Target Scope**: Testing Strategy
- **Severity**: CRITICAL
- **DO**: Plan to define new behaviors via failing tests (Red) _before_ implementing the structural solution (Green).
- **DO NOT**: Defer test creation until after the feature logic is already written.

### [BDD-02] Test Detail Matrix

- **Target Scope**: Acceptance Tests
- **Severity**: CRITICAL
- **DO**: Specify the exact new BDD-style test cases to be implemented, identify existing tests that must be modified to reflect new behavior, and define the criteria for a "Green" state.
- **DO NOT**: Provide generic testing goals like "write tests for this feature" without identifying the specific behavioral scenarios.

### [BDD-03] Hierarchical Test Structure

- **Target Scope**: Test Organization
- **Severity**: WARNING
- **DO**: Ensure planned test structures follow the mandatory nested BDD-style module hierarchy (`Target Method -> Specific Behavioral Scenario -> Expected Outcome Condition`).
- **DO NOT**: Structure planned tests as flat, unorganized lists of independent statements.
