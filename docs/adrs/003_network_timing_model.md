# ADR 003: Network Timing and Synchrony Model

## Metadata

- **Date:** 2026-04-09
- **Status:** Accepted
- **Scope:** Timing constants, heartbeat-to-election ratio, network synchrony model, and timeout invariants. Excludes transport encryption, DNS resolution, clock synchronization (NTP), and gRPC channel configuration.
- **Primary Goal:** Ensure cluster stability during leader elections and provide predictable failover times.
- **Last Updated:** 2026-06-09

## Context

A distributed consensus system's Liveness (ability to make progress) and Safety (data integrity) depend on its timing and network assumptions. Per the DLS result (Dwork, Lynch, Stockmeyer, 1988), consensus is impossible in a fully asynchronous system — some timing bound is required for liveness. Raft (Ongaro & Ousterhout, USENIX ATC 2014) operates under a **Partially Synchronous** model: the system may be asynchronous for arbitrary periods, but must eventually provide bounded message delivery and processing speeds during stable operation.

The core tension is between MTTR (Mean Time to Recover) and election stability. A shorter heartbeat interval detects leader failure faster but increases network chatter and risks false elections under load; a longer interval reduces chatter but delays failover. The ratio between heartbeat interval and election timeout must be large enough to tolerate a few dropped heartbeats without triggering an election, but small enough to keep failover within an acceptable window for a responsive grocery application. These constants also affect the AI Veto interaction (ADR 002): the Leader must remain responsive to heartbeat timers while awaiting slow AI responses (potentially up to 120s), requiring a strict decoupling of the consensus tick loop from AI evaluation.

We must define a specific timing model and a set of validated constant ranges to ensure cluster stability during leader elections, provide predictable failover, and prevent the consensus timer loop from being blocked by external policy resolution.

## Options Considered

### Option A: Synchronous Timing Model

Assume bounded message delay and processing time; set tight, fixed timeouts.

- **Safety**: Weak — a single delayed message violates the synchrony assumption and can cause spurious elections or permanent loss of liveness.
- **MTTR**: Lowest — deterministic failover bounds.
- **Complexity**: Lowest — no randomization needed.
- **Verdict**: Rejected — real-world networks (especially cloud deployments) cannot guarantee bounded delay.

### Option B: Partially Synchronous with 1:3–1:6 Ratio (Chosen)

Assume the network is generally asynchronous but provide a stable window for leader election via randomized timeouts. Heartbeat-to-election ratio between 1:3 and 1:6.

- **Safety**: Strong — Raft's safety proof holds independently of timing; only liveness depends on these bounds.
- **MTTR**: Sub-second failover (~340–380ms with pre-vote), acceptable for grocery-scale workloads.
- **Complexity**: Moderate — requires randomized timeouts and config validation invariants.
- **Verdict**: Chosen — proven by the Raft paper and consistent with ADR 001's crash-recovery model.

### Option C: Partially Synchronous with Wide Ratio (>1:10)

Use a conservative heartbeat-to-election ratio of >1:10 (e.g., 50ms heartbeat, 500ms+ election timeout).

- **Safety**: Same as Option B — safety is timing-independent.
- **MTTR**: Higher failover latency (~1s+), potentially noticeable during leader crashes.
- **Complexity**: Same as Option B.
- **Verdict**: Rejected — unnecessary latency penalty with no safety benefit.

### Option D: Do Nothing

Use operating-system defaults or leave timeouts undefined.

- **Safety**: Raft's safety holds, but liveness is unpredictable — split-vote livelocks and cascading leader changes are probable.
- **MTTR**: Unbounded.
- **Complexity**: None — but no guarantees.
- **Verdict**: Rejected — unacceptable operational risk for a clinical-grade system.

## Decision

We will adopt a **Partially Synchronous** timing model and assume a **Fair-Loss** network that provides ordered delivery over individual RPC streams.

### Assumptions & Constraints

- **Crash-Recovery model** (per ADR 001): Nodes fail only by stopping, not byzantine behavior. Timing is only relevant to liveness, never to safety.
- **Cluster size ≤ 7 nodes**: Timer values are tuned for small clusters; larger clusters may require adjusted heartbeats to avoid congestion.
- **Network fairness**: The network may drop, delay, or reorder messages, but individual TCP streams provide ordered delivery within a session.
- **Tick abstraction**: All timing invariants are expressed in discrete ticks (multiples of a base tick interval), not wall-clock milliseconds, to shield safety from OS timer non-determinism.
- **Non-Byzantine AI Veto**: The AI node may be slow (up to 120s) but is not malicious; the system must not allow AI latency to stall the consensus timer loop.

### 1. Heartbeat-to-Election Ratio

The system maintains a fixed ratio between the heartbeat interval and the election timeout range, between 1:3 and 1:6. The heartbeat interval determines how often the leader emits keep-alives; the election timeout range (randomized per node) determines how long a follower waits before triggering an election. The 1:3 lower bound ensures at least three heartbeats can be lost before a follower times out; the 1:6 upper bound prevents election timeouts from being so long that failover becomes noticeable.

Default values (within the ratio) are expressed as multiples of a base tick interval — approximately 5 ticks for heartbeat, 15–30 ticks for election timeout. These are configurable at deployment time, provided the ratio invariants are maintained.

### 2. External Timing Constraints (Ingress & Egress)

To ensure system liveness when interacting with external actors:

- **Non-Blocking Egress (Mandate):** The Leader MUST decouple heartbeat emission from the AI evaluation pipeline. A slow AI response (potentially up to 120s per ADR 002) must never block the heartbeat cycle. The consensus tick loop and AI evaluation run on separate execution contexts; AI evaluation may hold a mutation-scope lock but never the consensus write lock (per ADR 009).
- **Consensus timeout:** An upper bound on the end-to-end mutation lifecycle (including AI evaluation, consensus replication, and state machine application). Set to accommodate Strict Serialization (ADR 007) while requests queue for the mutation lock. Defined once in both the cluster config and the client config to ensure alignment.
- **Client retry backoff:** Clients MUST use exponential backoff with jitter when retrying requests after a redirect or timeout, with a minimum initial delay and a maximum cap, to prevent thundering-herd storms during leader failover.

### 3. Randomized Election Timeouts

To prevent split-vote livelocks, every node MUST randomize its election timeout within a configured range whenever the election timer is reset. The timeout is re-randomized on every transition into Follower and every transition into Candidate. The randomization range is bounded by the 1:3–1:6 ratio constraint.

### 4. Network Model: Asynchronous with Omission Faults

- The network is asynchronous — messages may be delayed, lost, or reordered.
- TCP/gRPC provides point-to-point reliable FIFO delivery within a single stream, but the application must tolerate network partitions where a majority quorum cannot be reached.
- Deduplication is handled at the state machine level via the Session Table (ADR 006), not at the transport layer.

### 5. Pre-Vote Campaign Timeout

To support the Pre-Vote mechanism, a campaign timeout is introduced for the PreCandidate role. This timeout must be long enough for a full round-trip of PreVote requests to all peers (~2× the P2P RPC timeout) but short enough to not significantly delay the overall election MTTR. The Pre-Vote mechanism adds approximately one RTT to the critical failover path, keeping worst-case recovery within the sub-second target.

### 6. Config Validation Invariants

The following architectural invariants MUST be enforced at startup to maintain timing safety:

- **Congestion Invariant:** The per-RPC timeout must be strictly shorter than the heartbeat interval. This prevents heartbeat stacking when RPCs are delayed by network congestion — if an RPC timeout matches or exceeds the heartbeat interval, a blocked RPC could delay the leader's heartbeat cycle.
- **SLA Invariant:** The consensus timeout must be at least as long as the per-RPC timeout. Otherwise, the overall mutation could time out before a single RPC attempt completes.
- **Election Ratio Lower Bound:** The minimum election timeout must be at least 3× the heartbeat interval (enforcing the 1:3 minimum ratio).
- **Election Ratio Upper Bound (Warning):** A warning SHOULD be emitted when the maximum election timeout exceeds 6× the heartbeat interval, indicating the ratio exceeds the recommended 1:6 maximum.

## Rationale

- **Partially Synchronous model (per DLS, 1988):** Consensus in an asynchronous system is impossible without *some* timing bound (the FLP impossibility result). The Partially Synchronous model — where the system is asynchronous but eventually provides bounded delay — is the least restrictive model sufficient for Raft's liveness. A Fully Synchronous model would be simpler but is unrealistic over TCP/IP; a Fully Asynchronous model cannot guarantee liveness.
- **1:3–1:6 ratio (per Raft paper §5.2, §9.6):** The Raft paper recommends an election timeout at least an order of magnitude larger than the heartbeat interval, with a randomized component. The 1:3–1:6 range is stricter than the paper's recommendation, chosen to guarantee sub-second failover for a responsive application. The 1:3 lower bound ensures at least three heartbeats can be dropped before a follower times out, tolerating transient packet loss without false elections. The 1:6 upper bound caps the maximum failover window at ~400ms (including pre-vote RTT), consistent with ADR 002's sub-second recovery target.
- **Randomized timeouts (per Raft paper §5.2):** Raft requires randomized election timeouts to prevent split-vote livelocks — without randomization, multiple followers could simultaneously become candidates and split the vote indefinitely, a well-known problem in leader-based consensus. The randomized range must be wide enough to make simultaneous timeouts statistically improbable for small clusters (≤7 nodes).
- **Non-blocking egress (per ADR 002, ADR 009):** The AI Veto can take up to 120s to respond. If the Leader held the consensus write lock during AI evaluation, heartbeats would stall and followers would trigger false elections. Decoupling the tick loop from AI evaluation is mandated by ADR 009's Internal Node Architecture (synchronous core vs. async execution shell).
- **Safety independence from timing (per Raft paper §5.4.2):** Raft's safety proof (Log Matching, Leader Completeness) depends only on the protocol's structural properties — quorum intersection, term monotonicity, and log matching — not on timing. Even if all heartbeat and election timers are misconfigured, the protocol will never commit conflicting entries; it may simply fail to make progress (liveness failure, not safety violation).

## Consequences

### Pros

- **High Availability:** Rapid failover (sub-second) ensures the system remains responsive even if the leader crashes.
- **Election Efficiency:** Randomized timeouts statistically guarantee that a single leader will emerge quickly in most scenarios.
- **Simplified Debugging:** Fixed timing constants make it easier to simulate and reproduce "Race Conditions" during development.

### Cons

- **CPU Overhead:** A 50ms heartbeat creates constant "chatter" on the network and keeps the CPU slightly active even when the system is idle.
- **Sensitivity to Load:** If the host machine experiences high "Stop-the-World" pauses (e.g., heavy GC or high CPU steal), it may trigger "False Elections."

### Operational Impact

- **Monitoring:** Track election frequency as a key health metric. A sustained high frequency indicates the network is too unstable for the configured heartbeat-to-election ratio, requiring either tighter network provisions or a wider ratio (at the cost of MTTR).

## Follow-Up

- **Baseline validation:** Validate the chosen 1:3–1:6 ratio via chaos-engineering experiments (network latency injection, packet loss) before production cutover to confirm false election rates remain below target.
- **Alert thresholds:** Establish alerting for election frequency spikes (e.g., >2 elections per minute) and consensus timeout violations.
- **Constant review:** Review and adjust timing constants after 30 days of production operation; publish operational guidance for selecting alternative values within the ratio invariants.
- **Config documentation:** Document the configurable timing parameters and their validation invariants in the deployment guide, including the formula for translating tick counts to wall-clock time.
