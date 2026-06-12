# Lact-O-Sensus

A distributed ledger for grocery inventory management, powered by the Raft consensus protocol.

**Status:** Verified through Phase 8 (Pre-Vote Integrity). The cluster maintains safety under asymmetric network partitions and guarantees linearizability via a sled-backed replicated FSM. Phase 9 (Elastic membership, security, multi-tenancy) is pending.

## What it does

Lact-O-Sensus treats physical grocery state with roughly the same rigor as financial transactions. Operators submit mutation intents through a CLI client (e.g. `add milk 10 liter`). The cluster resolves them through an AI oracle for semantic and moral evaluation, reaches consensus across nodes, and commits the result to a replicated sled-backed ledger — with exactly-once semantics and SI-normalized units.

## Architecture

The project is organized as a Rust workspace with 9 crates:

| Crate | Role |
|---|---|
| `common` | Shared types, Protobuf contracts, SI Unit Registry |
| `common-rpc` | gRPC middleware (identity interceptors) |
| `raft-engine` | Domain-agnostic Raft (election, replication, snapshots) |
| `lacto-fsm` | Business logic state machine (sled persistence, session table) |
| `gateway` | gRPC delivery layer with the 5-layer "Defense Onion" |
| `ai-veto` | AI oracle binary (CLI-configured, local Llama via Ollama) |
| `mock-veto` | Lightweight mock AI oracle for testing |
| `client-cli` | Interactive REPL with local WAL and leader discovery |
| `node-server` | Binary that wires everything together |

## Quick start

```bash
# Build everything
cargo build --release

# Start a 3-node cluster (three terminals)
cargo run --release -p node-server -- --config crates/node-server/configs/node_1.toml
cargo run --release -p node-server -- --config crates/node-server/configs/node_2.toml
cargo run --release -p node-server -- --config crates/node-server/configs/node_3.toml

# Start the AI oracle (requires Ollama + a pulled model)
cargo run --release -p ai-veto -- --port 50060 --model qwen3.5:4b

# Or use the mock oracle for testing
cargo run --release -p mock-veto -- --port 50070

# Connect with the client REPL
cargo run --release -p client-cli
```

## Verification

```bash
cargo +nightly fmt --all && cargo test --all-features && cargo clippy --all-targets -- -D warnings && python3 scripts/smoke_test.py
```

The smoke test suite covers leader election, failover, identity enforcement, AI egress, persistence recovery, snapshot installation, and read-your-writes guarantees.

## Project structure

```
crates/            # 9 Rust crates
docs/              # ADRs, checklists, retro notes, task plans, roadmap
scripts/           # Integration test harness
data/              # Runtime sled databases (gitignored)
```

## Further reading

- [Roadmap](docs/roadmap.md) — completed and upcoming phases
- [Architectural Decision Records](docs/adrs/) — 11 ADRs covering failure model, network topology, exactly-once semantics, and more
- [Project Overview](docs/project_overview.md) — extended vision and core mandates

## License

Not yet chosen.
