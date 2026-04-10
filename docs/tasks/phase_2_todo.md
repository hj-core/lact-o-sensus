# Phase 2 TODO: The Infrastructure (raft-node Skeleton)

## 🎯 Goal

Establish the network mesh, identity verification, and skeletal RPC handlers for the Raft cluster nodes.

---

## 🏗 Task Hierarchy

### 1. Node Identity & Environmental Context [ ]

- **1.1. Configuration Engine** [x]
  - *Status:* Completed
  - *Acceptance:* Load `cluster_id`, `node_id`, and peer addresses from a `config.toml`.
- **1.2. Identity Persistence (ADR 004)** [ ]
  - *Status:* Pending
  - *Acceptance:* Verify `(cluster_id, node_id)` matches the persisted state on disk.
- **1.3. Logging & Observability** [ ]
  - *Status:* Pending
  - *Acceptance:* Structured `tracing` output including the `node_id` in every log span.

### 2. gRPC Service Implementation (Skeletal Traits) [ ]

- **2.1. Internal Consensus Service (ADR 005)** [ ]
  - *Status:* Pending
  - *Acceptance:* Functional `RequestVote` and `AppendEntries` stubs returning valid gRPC responses.
- **2.2. External Ingress Service (ADR 002/005)** [ ]
  - *Status:* Pending
  - *Acceptance:* Functional `ProposeMutation` and `QueryState` stubs returning `NotImplemented` or `Follower` status.
- **2.3. Veto Relay Stub** [ ]
  - *Status:* Pending
  - *Acceptance:* Internal trait/bridge defined for future `ai-veto` communication.

### 3. Transport & Peer Orchestration [ ]

- **3.1. Async Server Listener** [ ]
  - *Status:* Pending
  - *Acceptance:* `tonic` server successfully binds to the configured port using `tokio`.
- **3.2. Outbound Client Registry** [ ]
  - *Status:* Pending
  - *Acceptance:* Lazy-initialized gRPC channels to all configured peer nodes.
- **3.3. Graceful Shutdown** [ ]
  - *Status:* Pending
  - *Acceptance:* Node handles SIGTERM/SIGINT by closing active connections and flushing logs.

### 4. Cluster Validation & "Smoke Tests" [ ]

- **4.1. Network Mesh Verification** [ ]
  - *Status:* Pending
  - *Acceptance:* A multi-node startup script confirms all nodes can reach each other via gRPC.
- **4.2. Protocol Invariant Verification** [ ]
  - *Status:* Pending
  - *Acceptance:* Nodes reject requests containing an incorrect `cluster_id`.

---

## 📈 Completion Status

- **Total Progress:** 8%
- **Current Focus:** 1.2. Identity Persistence (ADR 004)
