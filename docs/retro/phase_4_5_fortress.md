# Retrospective: Phase 4.5 - Fortress Hardening

## 🗓 Date: 2026-04-22

## 🎯 Scope: Centralized Identity, Durable WAL, and Resilient Client Logic

---

### 🏛 Summary of Achievements

1. **Centralized Identity Guard (ADR 004/005):** Transitioned from manual identity checks to centralized gRPC Interceptors. All inbound traffic is now strictly validated against `cluster_id` and `target_node_id` before reaching business logic.
2. **Exactly-Once Semantics (ADR 006):** Implemented a durable client-side Write-Ahead Log (WAL) using `sled`. Mutation intents are now persisted _before_ network egress and cleared only upon terminal acknowledgment (`COMMITTED` or `VETOED`).
3. **Startup Recovery Logic (ADR 001):** Introduced a synchronous startup reconciliation phase. The client now automatically re-proposes pending WAL intents before allowing new user input, maintaining absolute temporal ordering across crashes.
4. **Disciplined Retries (ADR 003):** Engineered an exponential backoff orchestrator with $\pm 20\%$ randomized jitter. This ensures cluster stability by preventing "thundering herd" scenarios during Raft election cycles.
5. **Architectural Deduplication:** Refactored the `LactoClient` to use a unified `reconcile_routing_failure` handler, synchronizing the resilience logic for both linearizable reads and mutations.
6. **Time-Dilation Verification:** Optimized the test suite by making backoff parameters configurable. This allowed 32 integration tests to execute in under **0.2 seconds** while still exercising the full failure-path logic.

---

### ✅ What Went Well

- **Middleware as Fortress:** Moving identity validation into Interceptors significantly reduced "Legacy Debt" and ensured that security boundaries are identical for all node roles.
- **Test-First Math:** Verifying the exponential backoff invariants in isolation before integration caught several edge-case overflows and rounding errors early.
- **terminal vs. Non-Terminal States:** Explicitly distinguishing between `VETOED` (terminal failure) and transport errors (retryable) prevented redundant AI evaluation calls during recovery.
- **High-Speed Integration:** The decision to refactor `LactoClient` for zero-delay testing proved to be a major win for developer velocity, transforming a 30-second test suite into a sub-second artifact.

---

### ❌ Challenges & Mistakes

- **Virtual Time Deadlock:** Attempted to use Tokio's `start_paused` feature for tests involving real network I/O via `MockIngressService`. This resulted in deadlocks because virtual time does not advance during actual TCP handshakes.
  - _Correction:_ Refactored the client to support **Configurable Timing Parameters** (Dependency Injection), allowing tests to set delays to zero without manipulating the global clock.
- **Sled Database Locks:** Encountered "Resource temporarily unavailable" errors in tests where multiple `IntentWal` instances attempted to access the same directory simultaneously.
  - _Correction:_ Hardened test teardown to explicitly `drop()` instances and release file locks before re-initializing the client.
- **Redundant Dispatch Logic:** Initially implemented identical retry logic in both `dispatch_mutation` and `dispatch_query`.
  - _Correction:_ Abstracted the logic into a meaningful `reconcile_routing_failure` helper, adhering to the "Logic Orchestration" mandate.

---

### 🧠 Lessons for Phase 5 (The Store & The Sled)

1. **Dependency Inject for Timing:** Never hardcode `sleep` or `timeout` durations. Making them configurable is the only way to ensure both production resilience and sub-second test suites.
2. **Identity is Protocol:** `x-cluster-id` and `target_node_id` are not just headers; they are fundamental protocol invariants. They should be the very first bytes checked upon receipt of any packet.
3. **The Terminality Principle:** Every request path must have a clear exit strategy for terminal failures. A WAL that doesn't understand "Vetoed" is a WAL that causes infinite retry loops.

---

### 📈 Phase 4.5 Grade: A+

_Exceptional alignment with the "Fortress" mandates. The transition to centralized interceptors and a durable WAL has transformed the system into a truly production-grade distributed ledger._
