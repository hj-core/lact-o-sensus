# Phase 5 Task List: The Semantic Oracle (Resolution & Defense)

## 🎯 Goal

Implement the 5-Layer Defensive Onion (ADR 007) and Semantic Resolution while completely decoupling the generic Raft consensus engine from the Lact-O-Sensus grocery application logic.

---

## 🏗️ Task Hierarchy & Git Commit Strategy

### Step 1: Documentation Update [x]

**Commit:** `docs: update ADRs to reflect decoupled oracle architecture`

* **Description:** Update existing Architecture Decision Records (ADRs) and design documentation to formally reflect the "Decoupled Oracle" refinement.
* **Changes:**
  * [x] Update `docs/adrs/005_logical_interface_specification.md` to document the split between internal consensus (`raft.proto`) and external application (`app.proto`).
  * [x] Update `docs/adrs/007_defensive_mutation_lifecycle.md` to mention the generic `StateMachine` trait and how the Application Logic wraps the consensus engine.
* **Acceptance Tests:**
  * [x] Review of markdown files for clarity and alignment with the new architectural direction.

### Step 2: The Network Boundary (Protobuf Refactoring) [ ]

**Commit:** `refactor(common): split proto into raft and application contracts`

* **Description:** Split the monolith protobuf into internal consensus and external application contracts.
* **Changes:**
  * [ ] Split `crates/common/proto/lacto_sensus.proto` into `raft.proto` and `app.proto`.
  * [ ] Update `crates/common/build.rs` to compile both.
  * [ ] Update `crates/common/src/proto.rs` to export the namespaced generated code.
* **Acceptance Tests (TDD):**
  * [ ] `cargo check` passes across all crates.
  * [ ] Existing `smoke_test.py` passes (verifying backward-compatible wire format despite the split).

### Step 3: The Rust Boundary (State Machine Trait) [ ]

**Commit:** `feat(raft): implement StateMachine trait and decouple node logic`

* **Description:** Extract the generic State Machine interface and decouple the Raft node.
* **Changes:**
  * [ ] Create `crates/raft-node/src/fsm.rs` defining `StateMachine` trait.
  * [ ] Update `RaftNode` to hold `Arc<dyn StateMachine>` and change `log` to store `Vec<u8>` payloads.
  * [ ] Modify `set_commit_index` to iterate and call `state_machine.apply(entry.data)`.
  * [ ] Create `crates/raft-node/src/store.rs` defining `LactoStore` (implements `StateMachine`).
* **Acceptance Tests (TDD):**
  * [ ] Write a unit test in `node.rs` verifying that `apply` is called with the correct bytes when `commit_index` advances.
  * [ ] `cargo test` passes.

### Step 4: The Universal Unit Registry (ADR 008) [ ]

**Commit:** `feat(common): implement universal unit registry and SI stabilization`

* **Description:** Implement the physical domain types and the internal SI stabilization model.
* **Changes:**
  * [ ] Create `crates/common/src/units.rs`.
  * [ ] Implement dimensional models (Mass, Volume, Count, Anomalous) using self-validating NewTypes.
  * [ ] Implement conversion logic using `rust_decimal::Decimal` and Banker's Rounding.
* **Acceptance Tests (TDD):**
  * [ ] Unit tests in `units.rs` verify conversions (e.g., `lb` to `g`, `gal` to `ml`).
  * [ ] Test verifying that Banker's rounding correctly prevents cumulative bias.
  * [ ] Test verifying the "Dimensional Fence" (adding `g` to `L` returns an error).

### Step 5: The 5-Layer Defensive Pipeline (ADR 007) [ ]

**Commit:** `feat(raft): implement MutationLock and 5-layer defensive pipeline`

* **Description:** Implement the `MutationLock` and the syntactic/semantic validation layers in the leader.
* **Changes:**
  * [ ] Embed `Arc<tokio::sync::Mutex<()>>` inside the `Leader` state in `ingress.rs`.
  * [ ] Implement Layer 2 (Syntactic Scrubbing & Taxonomy Guard).
  * [ ] Implement Layer 4 (Registry Firewall & Physical Invariant Check).
  * [ ] Update serialization in Layer 5 to propose the validated `CommittedMutation` as `Vec<u8>`.
* **Acceptance Tests (TDD):**
  * [ ] Unit tests in `ingress.rs` verifying that malformed input is rejected _before_ acquiring the `MutationLock`.
  * [ ] Unit tests verifying that unauthorized AI metadata (bad category/unit) triggers a Veto.

### Step 6: The Semantic Oracle (Mock Integration) [ ]

**Commit:** `feat(ai-veto): update mock to compliant semantic resolution`

* **Description:** Update the mock AI node to satisfy the new Layer 4 strict validation.
* **Changes:**
  * [ ] Update `crates/ai-veto/src/main.rs` to return compliant semantic data.
* **Acceptance Tests (TDD):**
  * [ ] `smoke_test.py` passes full integration: Client -> Leader -> Mock AI -> Consensus -> `LactoStore`.
