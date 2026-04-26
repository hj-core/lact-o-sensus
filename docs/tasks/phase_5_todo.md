# Phase 5 Task List: The AI Moral Advocate (Semantic Oracle)

## 🎯 Goal

Implement the 5-Layer Defensive Onion (ADR 007) and Semantic Resolution while completely decoupling the generic Raft consensus engine from the Lact-O-Sensus grocery application logic, culminating in a real relational AI evaluation engine.

---

## 🏗️ Task Hierarchy & Git Commit Strategy

### Step 1: Documentation Update [x]

**Commit:** `docs: update ADRs to reflect decoupled oracle architecture`

- **Description:** Update existing Architecture Decision Records (ADRs) and design documentation to formally reflect the "Decoupled Oracle" refinement.
- **Changes:**
  - [x] Update `docs/adrs/005_logical_interface_specification.md` to document the split between internal consensus (`raft.proto`) and external application (`app.proto`).
  - [x] Update `docs/adrs/007_defensive_mutation_lifecycle.md` to mention the generic `StateMachine` trait and how the Application Logic wraps the consensus engine.
- **Acceptance Tests:**
  - [x] Review of markdown files for clarity and alignment with the new architectural direction.

### Step 2: The Network Boundary (Protobuf Refactoring) [x]

**Commit:** `refactor(common): split proto into raft and application contracts`

- **Description:** Split the monolith protobuf into internal consensus and external application contracts.
- **Changes:**
  - [x] Split `crates/common/proto/lacto_sensus.proto` into `raft.proto` and `app.proto`.
  - [x] Update `crates/common/build.rs` to compile both.
  - [x] Update `crates/common/src/proto.rs` to export the namespaced generated code.
- **Acceptance Tests (TDD):**
  - [x] `cargo check` passes across all crates.
  - [x] Existing `smoke_test.py` passes (verifying backward-compatible wire format despite the split).

### Step 3: The Rust Boundary (State Machine Trait) [x]

**Commit:** `feat(raft): implement StateMachine trait and decouple node logic`

- **Description:** Extract the generic State Machine interface and decouple the Raft node.
- **Changes:**
  - [x] Create `crates/raft-node/src/fsm.rs` defining `StateMachine` trait.
  - [x] Update `RaftNode` to hold `Arc<dyn StateMachine>` and change `log` to store `Vec<u8>` payloads.
  - [x] Modify `set_commit_index` to iterate and call `state_machine.apply(entry.data)` (via `apply_to_state_machine` orchestrator).
  - [x] Create `crates/raft-node/src/store.rs` defining `LactoStore` (implements `StateMachine`).
- **Acceptance Tests (TDD):**
  - [x] Write a unit test in `node.rs` verifying that `apply` is called with the correct bytes when `commit_index` advances. (Verified via integration and existing tests adapted to new fsm).
  - [x] `cargo test` passes.

### Step 4: The Universal Unit Registry (ADR 008) [x]

**Commit:** `feat(common): implement universal unit registry and SI stabilization`

- **Description:** Implement the physical domain types and the internal SI stabilization model.
- **Changes:**
  - [x] Create `crates/common/src/units.rs`.
  - [x] Implement dimensional models (Mass, Volume, Count, Anomalous) using self-validating NewTypes.
  - [x] Implement conversion logic using `rust_decimal::Decimal` and Banker's Rounding.
- **Acceptance Tests (TDD):**
  - [x] Unit tests in `units.rs` verify conversions (e.g., `lb` to `g`, `gal` to `ml`).
  - [x] Test verifying that Banker's rounding correctly prevents cumulative bias.
  - [x] Test verifying the "Dimensional Fence" (adding `g` to `L` returns an error).

### Step 5: The Gateway Extraction (Architectural Decoupling) [x]

**Commit:** `refactor(gateway): extract ingress and veto logic to dedicated crate`

- **Description:** Move grocery-specific logic out of `raft-node` to enforce domain isolation.
- **Changes:**
  - [x] Create `crates/gateway` and migrate `ingress.rs` and `veto.rs` from `raft-node`.
  - [x] Define generic `RaftHandle` trait in `crates/common/src/raft_api.rs`.
  - [x] Implement `RaftHandle` in `raft-node` and update `gateway` to consume it.
  - [x] Wire up the `gateway` services in `raft-node/src/main.rs`.
- **Acceptance Tests (TDD):**
  - [x] `cargo check` passes across the workspace.
  - [x] `smoke_test.py` passes (verifying identical external behavior).

### Step 6: Internal Onion Refactor (ADR 009) [x]

**Commit:** `refactor(raft): align node engine with tri-layered onion model`

- **Description:** Refactor the node engine to strictly separate Physical, Logical, and Execution layers and implement the Poison-then-Panic mandate.
- **Changes:**
  - [x] Update `crates/raft-node/src/engine.rs` to implement the "Poison-then-Panic" sequence for all invariant violations.
  - [x] Refactor `crates/raft-node/src/state.rs` (Execution Shell) to ensure Lock-Signal Atomicity.
  - [x] Audit all `panic!` calls in `crates/raft-node/src/node.rs` to ensure they are trapped by the Logical layer.
- **Acceptance Tests (TDD):**
  - [x] Unit test in `engine.rs` verifying that a node transitioned to `Poisoned` panics on any subsequent access.
  - [x] Integration test verifying that a task panic does not leave a "Zombie Node" accessible to other tasks.

### Step 7: The Semantic Contract (Protobuf v2) [x]

**Commit:** `feat(common): update app.proto for resolved semantic data`

- **Description:** Update the application contract to support the rich ledger entries required by ADR 005/007.
- **Changes:**
  - [x] Update `app.proto` to include `CommittedMutation` with resolved slugs, SI units, and AI rationale.
  - [x] Update `crates/common/src/proto.rs` factory methods to support the new schema.
- **Acceptance Tests (TDD):**
  - [x] `cargo check` passes.
  - [x] Unit tests for `CommittedMutation` serialization/deserialization.

### Step 8: The 5-Layer Defensive Pipeline (ADR 007) [ ]

**Commit:** `feat(gateway): implement MutationLock and 5-layer defensive pipeline`

- **Description:** Implement the `MutationLock` and the syntactic/semantic validation layers in the gateway component.
- **Changes:**
  - [ ] Implement transient `MutationLock` in `IngressDispatcher` (Layer 2).
  - [ ] Implement Layer 2 (Syntactic Scrubbing & Taxonomy Guard).
  - [ ] Implement Layer 4 (Registry Firewall & Physical Invariant Check).
  - [ ] Implement Layer 5 serialization (proposing validated `CommittedMutation` as `Vec<u8>`).
- **Acceptance Tests (TDD):**
  - [ ] Unit tests in `gateway` verifying malformed input rejection.
  - [ ] Unit tests verifying that unauthorized AI metadata triggers a Veto.

### Step 9: The Semantic Oracle (Robustness & Retry) [ ]

**Commit:** `feat(raft): implement leader-side retry logic for AI resolution`

- **Description:** Implement leader-side retry logic for resolution failures and harden the defensive wall.
- **Changes:**
  - [ ] Implement **Leader-Internal Retries** (Best-Effort) in the `VetoRelay` (Layer 3).
- **Acceptance Tests (TDD):**
  - [ ] Integration test verifying that a transient AI failure triggers exactly one retry before a Veto.

### Step 10: The Moral Advocate (Real AI Integration) [ ]

**Commit:** `feat(ai-veto): integrate LLM engine and moral heuristics`

- **Description:** Replace the mock resolution logic with a real relational evaluation engine using a Large Language Model (LLM).
- **Changes:**
  - [ ] Integrate OpenAI API or local Llama via `ollama-rs`.
  - [ ] Implement the prompt engineering required for the **Moral Advocate** persona.
  - [ ] Implement context-aware evaluation (passing `current_inventory` to the LLM for relational judgement).
  - [ ] Verify: The LLM correctly resolves "OJ" to `orange_juice` and rejects items based on health/moral heuristics.
- **Acceptance Tests (TDD):**
  - [ ] Integration tests in `crates/ai-veto` verifying specific resolution scenarios.
  - [ ] `smoke_test.py` passes using a live (or local) AI engine.
