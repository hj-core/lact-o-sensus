# ADR 003: Network Timing and Synchrony Model

## Metadata

* **Date:** 2026-04-09
* **Status:** Proposed
* **Scope:** System Liveness and RPC Timing
* **Primary Goal:** Ensure cluster stability during leader elections and provide predictable failover times.

## Context

A distributed system's ability to make progress (Liveness) and maintain data integrity (Safety) depends on its timing and network assumptions. Raft requires a **Partially Synchronous** timing model, where the system is generally asynchronous but provides windows of stability for leader election. We must define specific timing constants to balance the "Mean Time to Recover" (MTTR) against the risk of "Split-Vote" live-locks.

## Decision

We will adopt a **Partially Synchronous** timing model and assume a **Fair-Loss** network that provides ordered delivery over individual RPC streams.

### 1. Timing Constants (The "Heartbeat-to-Election" Ratio)

To ensure stability, we will maintain a ratio of approximately 1:3 to 1:6 between heartbeats and election timeouts:

* **`HEARTBEAT_INTERVAL`:** 50ms.
* **`ELECTION_TIMEOUT_MIN`:** 150ms.
* **`ELECTION_TIMEOUT_MAX`:** 300ms.
* **`RPC_TIMEOUT`:** 40ms. (Internal peer-to-peer calls).

### 2. External Timing Constraints (Ingress & Egress)

To ensure system liveness when interacting with external actors:

* **`AI_VETO_TIMEOUT`:** 5000ms. LLM calls (especially local models) are significantly slower than Raft heartbeats. The Leader will wait up to 5 seconds for a "Veto."
* **`CLIENT_REQUEST_TIMEOUT`:** 10000ms. The total time a client waits for a "Committed" response. This must be `> (AI_VETO_TIMEOUT + ELECTION_TIMEOUT_MAX)` to allow for a full election cycle and an AI query within a single user request.
* **`CLIENT_RETRY_BACKOFF`:** Exponential (Start at 100ms, max 2000ms). Prevents a "Thundering Herd" of clients from overwhelming a newly elected leader.

### 3. Randomized Election Timeouts

To prevent multiple candidates from starting elections simultaneously (Split-Vote), every node will pick a random duration between `ELECTION_TIMEOUT_MIN` and `ELECTION_TIMEOUT_MAX` whenever it resets its election timer.

### 3. Network Model: Fair-Loss with Reordering

* **Assumptions:** Messages may be dropped, delayed, or reordered by the network.
* **Mitigation:** We leverage the chosen RPC transport (e.g., TCP-based streams) for point-to-point reliability and ordering within a connection. However, the application logic must handle **Network Partitions** where a majority cannot be reached.
* **Replay Protection:** The system handles replayed or delayed messages via the **Session Table (`sequence_id`)** at the state machine level.

## Rationale

* **MTTR vs. Stability:** A 150ms–300ms election range provides a "Mean Time to Recover" of under 500ms in most failure scenarios, which is suitable for a responsive grocery application.
* **Network Jitter:** A 50ms heartbeat provides enough "slack" (3x) for a single dropped packet or minor network spike without triggering a disruptive re-election.
* **Deterministic Safety:** While Liveness depends on these timers, **Safety is never dependent on time**. Even if all timers fail, the Raft protocol ensures that no two nodes will ever commit different values for the same log index.

## Consequences

### Pros

* **High Availability:** Rapid failover (sub-second) ensures the system remains responsive even if the leader crashes.
* **Election Efficiency:** Randomized timeouts statistically guarantee that a single leader will emerge quickly in most scenarios.
* **Simplified Debugging:** Fixed timing constants make it easier to simulate and reproduce "Race Conditions" during development.

### Cons

* **CPU Overhead:** A 50ms heartbeat creates constant "chatter" on the network and keeps the CPU slightly active even when the system is idle.
* **Sensitivity to Load:** If the host machine experiences high "Stop-the-World" pauses (e.g., heavy GC or high CPU steal), it may trigger "False Elections."

### Operational Impact

* **Monitoring:** We should track "Election Frequency" as a key health metric. A high frequency indicates that the network is too unstable for the chosen 50ms/150ms constants.
