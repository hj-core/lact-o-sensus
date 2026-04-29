#!/usr/bin/env python3
import datetime
import json
import os
import subprocess
import sys
import time
from typing import Dict, List, Optional, TypedDict, TextIO, Generator

# Lact-O-Sensus: Consensus Verification Suite
# Verifies the Raft "Consensus Heart" and AI Egress logic via isolated test cases.


class NodeConfig(TypedDict):
    id: int
    port: int
    config: str
    log: str


NODES: List[NodeConfig] = [
    {
        "id": 1,
        "port": 50051,
        "config": "crates/raft-node/configs/node_1.toml",
        "log": "node_1.log",
    },
    {
        "id": 2,
        "port": 50052,
        "config": "crates/raft-node/configs/node_2.toml",
        "log": "node_2.log",
    },
    {
        "id": 3,
        "port": 50053,
        "config": "crates/raft-node/configs/node_3.toml",
        "log": "node_3.log",
    },
]

VETO_PORT = 50060
VETO_LOG = "ai_veto.log"


# --- Helper Library ---


def now_ms() -> float:
    """Returns current wall-clock time in milliseconds."""
    return time.time() * 1000


class ClusterManager:
    """Manages the lifecycle of a local 3-node Raft cluster and an AI Veto Node."""

    processes: Dict[int, subprocess.Popen]
    log_files: Dict[int, TextIO]
    veto_process: Optional[subprocess.Popen]
    veto_log: Optional[TextIO]

    def __init__(self) -> None:
        self.processes = {}
        self.log_files = {}
        self.veto_process = None
        self.veto_log = None

    def start_all(self, start_veto: bool = False) -> None:
        """Starts all nodes defined in NODES and optionally the AI Veto Node."""
        print(f"--- Starting cluster (AI Veto: {start_veto}) ---")

        # Capture and prepare environment once to ensure consistency across the cluster.
        cluster_env = os.environ.copy()
        cluster_env["RUST_LOG"] = "info"

        # 1. Start AI Veto Node if requested
        if start_veto:
            if os.path.exists(VETO_LOG):
                os.remove(VETO_LOG)

            self.veto_log = open(VETO_LOG, "w", encoding="utf-8")
            self.veto_process = subprocess.Popen(
                [
                    "cargo",
                    "run",
                    "-p",
                    "ai-veto",
                    "--",
                    "--port",
                    str(VETO_PORT),
                ],
                stdout=self.veto_log,
                stderr=subprocess.STDOUT,
                env=cluster_env,
            )

        # 2. Start Raft Nodes
        for node in NODES:
            if os.path.exists(node["log"]):
                os.remove(node["log"])

            log_file = open(node["log"], "w", encoding="utf-8")
            self.log_files[node["id"]] = log_file

            cmd = [
                "cargo",
                "run",
                "-p",
                "raft-node",
                "--",
                "--config",
                node["config"],
            ]
            p = subprocess.Popen(
                cmd,
                stdout=log_file,
                stderr=subprocess.STDOUT,
                env=cluster_env,
            )
            self.processes[node["id"]] = p

        # Give nodes time to initialize and Cargo to finish building if necessary
        time.sleep(2)

    def kill_node(self, node_id: int) -> float:
        """Kills a specific node and returns kill timestamp in ms."""
        if node_id in self.processes:
            p = self.processes[node_id]
            print(f"Action: Killing Node {node_id} (PID {p.pid})...")
            kill_time = now_ms()
            p.kill()
            p.wait()
            del self.processes[node_id]
            if node_id in self.log_files:
                self.log_files[node_id].close()
                del self.log_files[node_id]
            return kill_time
        return 0.0

    def cleanup(self) -> None:
        """
        Performs deterministic resource reclamation for all nodes.
        """
        print("--- Cleaning up cluster ---")

        # Cleanup AI Veto
        if self.veto_process:
            self.veto_process.terminate()
            try:
                self.veto_process.wait(timeout=2)
            except subprocess.SubprocessError:
                self.veto_process.kill()

        if self.veto_log:
            try:
                self.veto_log.close()
            except (OSError, IOError):
                pass

        # Cleanup Raft Nodes
        for p in self.processes.values():
            p.terminate()
        for p in self.processes.values():
            try:
                p.wait(timeout=2)
            except (
                subprocess.TimeoutExpired,
                subprocess.SubprocessError,
            ):
                p.kill()

        for f in self.log_files.values():
            try:
                f.close()
            except (OSError, IOError):
                pass

        self.processes.clear()
        self.log_files.clear()
        self.veto_process = None
        self.veto_log = None


def get_complete_lines(
    log_path: str, offset: int = 0
) -> Generator[str, None, int]:
    """Yields only complete lines from a log file."""
    if not os.path.exists(log_path):
        return offset
    with open(log_path, "r", encoding="utf-8") as f:
        f.seek(offset)
        while True:
            line = f.readline()
            if not line or not line.endswith("\n"):
                break
            yield line
            offset = f.tell()
    return offset


def parse_log_timestamp(line: str) -> float:
    try:
        ts_str = line.split(" ")[0]
        ts = datetime.datetime.fromisoformat(
            ts_str.replace("Z", "+00:00")
        )
        return ts.timestamp() * 1000
    except (ValueError, IndexError):
        return 0.0


def find_current_leader() -> Optional[int]:
    """Robust leader discovery using most recent election event."""
    leader_id: Optional[int] = None
    latest_ts: float = -1.0
    for node in NODES:
        for line in get_complete_lines(node["log"], 0):
            ts = parse_log_timestamp(line)
            if "Transitioning to Leader" in line and ts > latest_ts:
                latest_ts, leader_id = ts, node["id"]
            if (
                "Demoting to Follower" in line
                and ts >= latest_ts
                and leader_id == node["id"]
            ):
                leader_id = None
    return leader_id


def wait_for_leader(timeout: float = 15.0) -> int:
    """Helper to wait for a leader to emerge."""
    print(
        f"Waiting for leader to emerge (max {timeout}s)...",
        end="",
        flush=True,
    )
    start = time.time()
    while (time.time() - start) < timeout:
        leader_id = find_current_leader()
        if leader_id:
            print(f" OK (Node {leader_id})")
            return leader_id
        time.sleep(0.5)
    print(" FAILED")
    raise RuntimeError(f"No leader emerged within {timeout}s.")


def count_elections() -> int:
    return sum(
        1
        for node in NODES
        for line in get_complete_lines(node["log"], 0)
        if "Transitioning to Leader" in line
    )


def print_cluster_logs(lines: int = 5) -> None:
    print(f"\n--- Diagnostic Tail (Last {lines} lines) ---")
    for node in NODES:
        if os.path.exists(node["log"]):
            print(f"\n--- Node {node['id']} ---")
            subprocess.run(
                ["tail", "-n", str(lines), node["log"]], check=False
            )


def check_connectivity(
    target_node_id: int,
    port: int,
    cluster_id: str = "probe-unauthorized",
) -> bool:
    """Side-effect free probe via Identity Guard."""
    peer_id = 2 if target_node_id == 1 else 1
    cmd = [
        "grpcurl",
        "-plaintext",
        "-import-path",
        "crates/common/proto",
        "-proto",
        "raft.proto",
        "-H",
        f"x-cluster-id: {cluster_id}",
        "-H",
        f"x-target-node-id: {target_node_id}",
        "-d",
        json.dumps(
            {
                "term": 1,
                "candidate_id": str(peer_id),
            }
        ),
        f"127.0.0.1:{port}",
        "raft.v1.ConsensusService/RequestVote",
    ]
    result = subprocess.run(
        cmd, capture_output=True, text=True, check=False
    )
    if cluster_id == "lacto-dev-01":
        return result.returncode == 0
    else:
        return (
            "Cluster identity mismatch" in result.stderr
            or "Unauthenticated" in result.stderr
        )


# --- Test Cases ---


def test_leader_election() -> None:
    """Explicitly tests that a leader can be elected."""
    wait_for_leader()


def test_leadership_stability() -> None:
    """Verifies that heartbeats maintain a stable leader without re-elections."""
    wait_for_leader()
    initial_count = count_elections()
    print("Verifying stability for 3s...")
    time.sleep(3)
    if count_elections() > initial_count:
        raise RuntimeError(
            "Leadership was unstable (unnecessary re-election detected)."
        )
    print("SUCCESS: Leadership stable.")


def test_leader_failover(cluster: ClusterManager) -> None:
    """Verifies that killing the leader triggers a successful re-election."""
    leader_id = wait_for_leader()
    base_count = count_elections()

    log_offsets = {
        n["id"]: (
            os.path.getsize(n["log"]) if os.path.exists(n["log"]) else 0
        )
        for n in NODES
    }
    kill_time = cluster.kill_node(leader_id)

    print("Waiting for re-election...")
    max_wait, elapsed = 10.0, 0.0
    while elapsed < max_wait:
        time.sleep(0.1)
        elapsed += 0.1
        if count_elections() > base_count:
            for node in NODES:
                if node["id"] == leader_id:
                    continue
                for line in get_complete_lines(
                    node["log"], log_offsets.get(node["id"], 0)
                ):
                    if "Transitioning to Leader" in line:
                        log_ts = parse_log_timestamp(line)
                        duration = int(
                            (log_ts if log_ts > 0 else now_ms())
                            - kill_time
                        )
                        print(
                            f"SUCCESS: New leader (Node {node['id']}) elected in {duration}ms."
                        )
                        return
    raise RuntimeError("No re-election occurred after failover.")


def test_identity_guard() -> None:
    """Verifies that the Identity Guard (ADR 004) rejects unauthorized cluster IDs."""
    wait_for_leader()
    if check_connectivity(1, 50051, cluster_id="wrong-cluster"):
        print(
            "SUCCESS: Identity Guard correctly rejected unauthorized request."
        )
    else:
        raise RuntimeError(
            "Identity Guard failed to reject unauthorized request."
        )


def test_ai_veto_egress() -> None:
    """Verifies that the Leader can successfully call out to the AI Veto Node."""
    leader_id = wait_for_leader()
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

    print(
        f"Action: Sending mutation to Leader (Node {leader_id}) on port {leader_port}..."
    )
    cmd = [
        "grpcurl",
        "-plaintext",
        "-import-path",
        "crates/common/proto",
        "-proto",
        "app.proto",
        "-H",
        "x-cluster-id: lacto-dev-01",
        "-H",
        f"x-target-node-id: {leader_id}",
        "-d",
        json.dumps(
            {
                "client_id": "550e8400-e29b-41d4-a716-446655440000",
                "sequence_id": 1,
                "intent": {"item_key": "oat_milk", "quantity": "2"},
            }
        ),
        f"127.0.0.1:{leader_port}",
        "lacto_sensus.v1.IngressService/ProposeMutation",
    ]
    result = subprocess.run(
        cmd, capture_output=True, text=True, check=False
    )

    if "MUTATION_STATUS_COMMITTED" in result.stdout:
        print(
            "SUCCESS: Leader successfully called out to AI Veto Node and committed the entry."
        )
    else:
        print(
            f"FAILURE: Unexpected response from leader: {result.stdout} {result.stderr}"
        )
        raise RuntimeError(
            "Leader failed to trigger AI evaluation or received error."
        )


def test_client_cli_round_trip() -> None:
    """Verifies that the Smart Client can redirect from a Follower and commit via the Leader."""
    leader_id = wait_for_leader()

    # Give followers time to receive the first heartbeat and learn the leader's ID
    print("Stabilizing cluster (2s)...")
    time.sleep(2)

    follower_port = next(
        n["port"] for n in NODES if n["id"] != leader_id
    )

    print(
        f"Action: Starting client-cli seeded with Follower (port {follower_port})..."
    )

    # Clean up any lingering state
    state_file = ".client_state.json"
    wal_dir = ".client_wal"
    if os.path.exists(state_file):
        os.remove(state_file)
    if os.path.exists(wal_dir):
        import shutil

        shutil.rmtree(wal_dir)

    cmd = [
        "cargo",
        "run",
        "-q",  # Quiet cargo output to make parsing stdout easier
        "-p",
        "client-cli",
        "--",
        "--cluster-id",
        "lacto-dev-01",
        "--seed",
        f"127.0.0.1:{follower_port}",
    ]

    p = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )

    try:
        # Send a mutation command and then exit
        if p.stdin is None:
            raise RuntimeError("Failed to open stdin for client-cli")

        p.stdin.write(
            'add "oat milk" 2 cartons AnimalSecretions\nexit\n'
        )
        p.stdin.flush()

        stdout, _ = p.communicate(timeout=10)

        if "SUCCESS: Committed at version" in stdout:
            print(
                "SUCCESS: Client successfully navigated cluster and committed mutation."
            )
        else:
            print(f"FAILURE: Unexpected client output:\n{stdout}")
            raise RuntimeError(
                "Client failed to commit mutation via CLI."
            )
    except subprocess.TimeoutExpired as exc:
        p.kill()
        raise RuntimeError("Client CLI timed out.") from exc
    finally:
        if os.path.exists(state_file):
            os.remove(state_file)


# --- Runner Logic ---


def main() -> None:
    print("=== Lact-O-Sensus Consensus & Integration Suite ===")

    # Allow filtering tests by name via command line
    filter_arg = sys.argv[1] if len(sys.argv) > 1 else None

    tests = [
        ("Leader Election", False, lambda c: test_leader_election()),
        (
            "Leadership Stability",
            False,
            lambda c: test_leadership_stability(),
        ),
        (
            "Chaos Failover",
            False,
            lambda c: test_leader_failover(c),  # pylint: disable=W0108
        ),
        (
            "Identity Guard (ADR 004)",
            False,
            lambda c: test_identity_guard(),
        ),
        ("AI Veto Egress", True, lambda c: test_ai_veto_egress()),
        (
            "Smart Client Round-Trip",
            True,
            lambda c: test_client_cli_round_trip(),
        ),
    ]

    passed = 0
    total_run = 0
    for name, needs_veto, test_func in tests:
        if filter_arg and filter_arg.lower() not in name.lower():
            continue

        total_run += 1
        print(f"\n[TEST] {name}")
        cluster = ClusterManager()
        try:
            cluster.start_all(start_veto=needs_veto)
            test_func(cluster)
            passed += 1
        except Exception as e:  # pylint: disable=broad-except
            print(f"RESULT: FAILED -> {e}")
            print_cluster_logs()
        finally:
            cluster.cleanup()
            time.sleep(1)

    print(f"\n=== Final Result: {passed}/{total_run} Tests Passed ===")
    if total_run > 0 and passed < total_run:
        sys.exit(1)


if __name__ == "__main__":
    main()
