# Lact-O-Sensus Implementation Planning Checklist

This checklist defines the mandatory structure and quality standards for all implementation plans regarding major features or refactoring. Plans must be approved before any code is modified.

## 1. Task (Commit) Atomicity

- Every task in the plan must map directly to exactly **one atomic git commit**.
- Each task should target a single logical change (ideally < 300 lines).
- Tasks should be ordered foundationally (e.g., `common` -> `engine` -> `fsm`).

## 2. Task Detail Requirements

For every task (commit) in the plan, the following fields must be explicitly defined:

- **Goal**: A clear, concise statement of the clinical outcome of this commit.
- **Affected Files**: A comprehensive list of files to be created or modified.
- **Major Steps**: The specific implementation actions required.
- **Acceptance Tests**:
  - Specific new BDD-style test cases to be implemented.
  - Identification of existing tests that must be modified to reflect new behavior.
  - Expected "Green" state criteria.
- **Consequences**: Impact on downstream components, architectural invariants, or public contracts.
- **Caveats**: Known risks, assumptions, or transient state inconsistencies introduced by this commit.

## 3. Verification & Standards

- Every task must explicitly include running the full project-specific verification sequence (`fmt`, `test`, `clippy`, `smoke_test`).
- Every task must include a draft commit message (e.g., `feat(raft): implement pre-vote handler`).
- Every task must follow the orchestration pattern (delegating to sub-functions).

## 4. BDD Alignment

- The plan should ensure that new behaviors are defined via failing tests (Red) before implementation (Green).
- The test structure should follow the mandatory nested module hierarchy.
