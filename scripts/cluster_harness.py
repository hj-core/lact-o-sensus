import datetime
import json
import os
import re
import shutil
import subprocess
import time
from typing import (
    Dict,
    List,
    Optional,
    TypedDict,
    IO,
    Generator,
    Tuple,
    Callable,
    Any,
)

# Lact-O-Sensus: Cluster Test Harness
# Provides the core infrastructure for managing local Raft clusters, AI Veto Nodes, and Clients.


class NodeConfig(TypedDict):
    id: int
    port: int
    config: str
    log: str


NODES: List[NodeConfig] = [
    {
        "id": 1,
        "port": 50051,
        "config": "crates/node-server/configs/node_1.toml",
        "log": "node_1.log",
    },
    {
        "id": 2,
        "port": 50052,
        "config": "crates/node-server/configs/node_2.toml",
        "log": "node_2.log",
    },
    {
        "id": 3,
        "port": 50053,
        "config": "crates/node-server/configs/node_3.toml",
        "log": "node_3.log",
    },
]

VETO_PORT = 50060
VETO_LOG = "ai_veto.log"

ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")

# Binary Paths (relative to project root)
# Using release profile for better timing stability and performance.
TARGET_DIR = "target/release"
NODE_SERVER_BIN = os.path.join(TARGET_DIR, "raft-node")
AI_VETO_BIN = os.path.join(TARGET_DIR, "ai-veto")
CLIENT_CLI_BIN = os.path.join(TARGET_DIR, "client-cli")


class Registry:
    """
    Physical Truth Registry (ADR 4.10).
    Synchronizes test data with the internal Rust FSM hardcoded registries.
    """

    CATEGORIES = [
        "PrimaryFlora",
        "AnimalSecretions",
        "FleshAndMarrow",
        "ShelfStableCarbohydrates",
        "CulturedDoughs",
        "LiquefiedHydration",
        "CondimentsAndCatalysts",
        "NutrientSparseCommodities",
        "EthanolSolutions",
        "BiomedicalMaintenance",
        "SanitizationAndUtility",
        "AnomalousInputs",
    ]

    UNITS = {
        "Mass": ["g", "kg", "lb", "lbs", "oz"],
        "Volume": ["ml", "l", "gal", "fl_oz"],
        "Count": ["units", "unit", "pc", "pcs", "dozens", "dozen", "packs", "pack"],
        "Anomalous": ["misc", "handful", "bunch"],
    }

    # Standard items used across multiple smoke tests
    TEST_ITEMS = {
        "WATER": {"key": "water", "category": "LiquefiedHydration", "unit": "l"},
        "APPLE": {"key": "apple", "category": "PrimaryFlora", "unit": "units"},
        "MILK": {
            "key": "milk",
            "category": "LiquefiedHydration",
            "unit": "l",
        },  # Could also be AnimalSecretions
        "BANANA": {"key": "banana", "category": "PrimaryFlora", "unit": "units"},
        "CIGARETTES": {
            "key": "cigarettes",
            "category": "NutrientSparseCommodities",
            "unit": "pack",
        },
    }


def now_ms() -> float:
    """Returns current wall-clock time in milliseconds."""
    return time.time() * 1000


def build_binaries() -> None:
    """Compiles all required binaries exactly once using the release profile."""
    print("--- Compiling Lact-O-Sensus Binaries (Release) ---")
    cmd = [
        "cargo",
        "build",
        "--release",
        "-p",
        "node-server",
        "-p",
        "ai-veto",
        "-p",
        "client-cli",
    ]
    result = subprocess.run(cmd, check=False)
    if result.returncode != 0:
        raise RuntimeError("Cargo build failed.")
    print("SUCCESS: Binaries compiled.")


def poll_until(
    condition_fn: Callable[[], Any],
    timeout: float = 10.0,
    interval: float = 0.1,
    desc: Optional[str] = None,
) -> Any:
    """
    Generic polling helper. Repeatedly calls condition_fn until it returns a
    truthy value or timeout is reached.
    """
    start = time.time()
    while (time.time() - start) < timeout:
        res = condition_fn()
        if res:
            return res
        time.sleep(interval)
    raise RuntimeError(f"Timeout waiting for condition: {desc or 'unspecified'}")


class ClusterManager:
    """Manages the lifecycle of a local 3-node Raft cluster and an AI Veto Node."""

    processes: Dict[int, subprocess.Popen]
    log_files: Dict[int, IO]
    veto_process: Optional[subprocess.Popen]
    veto_log: Optional[IO]

    # Stateful Log Buffers
    node_logs: Dict[int, List[str]]
    node_offsets: Dict[int, int]

    def __init__(self) -> None:
        self.processes = {}
        self.log_files = {}
        self.veto_process = None
        self.veto_log = None
        self.node_logs = {n["id"]: [] for n in NODES}
        self.node_offsets = {n["id"]: 0 for n in NODES}

    def start_node(self, node_id: int, wipe_data: bool = False) -> None:
        """Starts or restarts a specific node using pre-compiled binary."""
        node = next(n for n in NODES if n["id"] == node_id)

        if wipe_data:
            # 1. Wipe diagnostic logs
            if os.path.exists(node["log"]):
                os.remove(node["log"])
            self.node_logs[node_id] = []
            self.node_offsets[node_id] = 0

            # 2. Wipe physical persistence directory (ADR 001/009)
            data_dir = f"data/node_{node_id}"
            if os.path.exists(data_dir):
                shutil.rmtree(data_dir)

        # Capture and prepare environment
        cluster_env = os.environ.copy()
        cluster_env["RUST_LOG"] = "info"

        mode = "w" if wipe_data else "a"
        log_file: IO = open(node["log"], mode, encoding="utf-8")
        self.log_files[node["id"]] = log_file

        # Execute compiled binary directly
        cmd = [
            NODE_SERVER_BIN,
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

    def start_all(self, start_veto: bool = False, wipe_data: bool = True) -> None:
        """Starts all nodes defined in NODES and optionally the AI Veto Node."""
        print(f"--- Starting cluster (AI Veto: {start_veto}, Wipe: {wipe_data}) ---")

        # 1. Start AI Veto Node if requested
        if start_veto:
            cluster_env = os.environ.copy()
            cluster_env["RUST_LOG"] = "info"
            if os.path.exists(VETO_LOG):
                os.remove(VETO_LOG)

            self.veto_log = open(VETO_LOG, "w", encoding="utf-8")
            self.veto_process = subprocess.Popen(
                [
                    AI_VETO_BIN,
                    "--port",
                    str(VETO_PORT),
                    "--model",
                    "qwen3.5:4b",
                ],
                stdout=self.veto_log,
                stderr=subprocess.STDOUT,
                env=cluster_env,
            )

        # 2. Start Raft Nodes (with data wipe if requested)
        for node in NODES:
            self.start_node(node["id"], wipe_data=wipe_data)

        # 3. Wait for all Raft nodes to bind their ports
        # (Replaces the static time.sleep(2))
        print("Action: Waiting for nodes to initialize...")
        for node in NODES:
            poll_until(
                lambda: self.check_node_alive(node["id"]),
                timeout=5,
                desc=f"Node {node['id']} startup",
            )

    def check_node_alive(self, node_id: int) -> bool:
        """Returns True if the node's process is running."""
        if node_id not in self.processes:
            return False
        return self.processes[node_id].poll() is None

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
        It shuts down processes and closes log files, but does NOT wipe
        persistent data directories.
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

    def wipe_node_fsm(self, node_id: int) -> None:
        """Surgically deletes only the FSM database for a specific node."""
        fsm_path = f"data/node_{node_id}/fsm"
        print(f"Action: Surgical Wipe of FSM at {fsm_path}")
        if os.path.exists(fsm_path):
            shutil.rmtree(fsm_path)
        else:
            raise RuntimeError(f"FSM path {fsm_path} not found for wiping.")

    def refresh_logs(self) -> None:
        """Reads new lines from all node logs and appends to in-memory buffers."""
        for node in NODES:
            nid = node["id"]
            path = node["log"]
            if not os.path.exists(path):
                continue

            with open(path, "r", encoding="utf-8") as f:
                f.seek(self.node_offsets[nid])
                while True:
                    line = f.readline()
                    if not line or not line.endswith("\n"):
                        break
                    self.node_logs[nid].append(ANSI_ESCAPE.sub("", line))
                    self.node_offsets[nid] = f.tell()


def get_complete_lines(log_path: str, offset: int = 0) -> Generator[str, None, int]:
    """
    Yields only complete lines from a log file.
    NOTE: Returns the new offset via StopIteration.value.
    """
    if not os.path.exists(log_path):
        return offset
    with open(log_path, "r", encoding="utf-8") as f:
        f.seek(offset)
        while True:
            line = f.readline()
            if not line or not line.endswith("\n"):
                break
            yield ANSI_ESCAPE.sub("", line)
            offset = f.tell()
    return offset


def parse_log_timestamp(line: str) -> float:
    try:
        ts_str = line.split(" ")[0]
        ts = datetime.datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
        return ts.timestamp() * 1000
    except (ValueError, IndexError):
        return 0.0


def find_current_leader(cluster: ClusterManager) -> Optional[int]:
    """Robust leader discovery using in-memory log buffers."""
    cluster.refresh_logs()
    leaders = []  # List of (term, timestamp, node_id)
    for node in NODES:
        for line in cluster.node_logs[node["id"]]:
            if "Transitioning to Leader" in line:
                ts = parse_log_timestamp(line)
                term_match = re.search(r"term[= ](\d+)", line)
                term = int(term_match.group(1)) if term_match else 0
                leaders.append((term, ts, node["id"]))

    if not leaders:
        return None

    # Sort by term first (Sovereignty), then timestamp
    leaders.sort(key=lambda x: (x[0], x[1]), reverse=True)
    best_leader = leaders[0]

    leader_term = best_leader[0]
    latest_ts = best_leader[1]
    leader_id = best_leader[2]

    # Verify this leader hasn't demoted in a later log entry
    for line in cluster.node_logs[leader_id]:
        if "Role Transition: -> Follower" in line:
            ts = parse_log_timestamp(line)
            term_match = re.search(r"term[= ](\d+)", line)
            demoted_term = int(term_match.group(1)) if term_match else 0

            # If demoted in the same or higher term, or if timestamp is newer
            if ts >= latest_ts and demoted_term >= leader_term:
                return None
    return leader_id


def wait_for_leader(cluster: ClusterManager, timeout: float = 15.0) -> int:
    """
    Helper to wait for a leader to emerge and remain stable for one heartbeat.
    (Harden Discovery mandate from Commit 4)
    """
    print(
        f"Waiting for leader to emerge (max {timeout}s)...",
        end="",
        flush=True,
    )

    def leader_is_stable() -> Optional[int]:
        lid = find_current_leader(cluster)
        if lid:
            # Wait for one heartbeat (50ms + safety buffer) to ensure it's not a flap
            time.sleep(0.1)
            if find_current_leader(cluster) == lid:
                return lid
        return None

    leader_id = poll_until(leader_is_stable, timeout=timeout, desc="Stable leader")
    print(f" OK (Node {leader_id})")
    return leader_id


def count_elections(cluster: ClusterManager) -> int:
    cluster.refresh_logs()
    return sum(
        1
        for nid in cluster.node_logs
        for line in cluster.node_logs[nid]
        if "Role Transition: -> Leader" in line
    )


def print_cluster_logs(lines: int = 5) -> None:
    print(f"\n--- Diagnostic Tail (Last {lines} lines) ---")
    for node in NODES:
        if os.path.exists(node["log"]):
            print(f"\n--- Node {node['id']} ---")
            subprocess.run(["tail", "-n", str(lines), node["log"]], check=False)


def check_connectivity(
    target_node_id: int,
    port: int,
    cluster_id: str = "probe-unauthorized",
) -> bool:
    """
    Side-effect free probe via Identity Guard.
    Harden Protocol mandate: Uses gRPC status evaluation (ADR 006).
    """
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
    result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if cluster_id == "lacto-dev-01":
        return result.returncode == 0
    else:
        # Standard grpcurl error output format for Unauthenticated status
        return "Code: Unauthenticated" in result.stderr


def run_client_command(command: str, seed_port: int, timeout: int = 120) -> str:
    """Helper to run a single command through the client-cli using pre-compiled binary."""
    state_file = ".client_state.json"
    wal_dir = ".client_wal"
    if os.path.exists(state_file):
        os.remove(state_file)
    if os.path.exists(wal_dir):
        shutil.rmtree(wal_dir)

    cmd = [
        CLIENT_CLI_BIN,
        "--cluster-id",
        "lacto-dev-01",
        "--seed",
        f"127.0.0.1:{seed_port}",
    ]

    p = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )

    try:
        if p.stdin is None:
            raise RuntimeError("Failed to open stdin for client-cli")
        p.stdin.write(f"{command}\nexit\n")
        p.stdin.flush()
        stdout, _ = p.communicate(timeout=timeout)
        return stdout
    except subprocess.TimeoutExpired as exc:
        p.kill()
        raise exc
    finally:
        if os.path.exists(state_file):
            os.remove(state_file)


def extract_version(output: str) -> int:
    """Extracts the state version from client-cli output."""
    match = re.search(r"version (\d+)", output)
    if match:
        return int(match.group(1))
    return 0


def verify_convergence(
    cluster: ClusterManager,
    index: int,
    status_str: str,
    timeout: float = 5.0,
) -> None:
    """Verifies that ALL nodes applied the mutation at the given index using log buffers."""
    print(f"Action: Verifying cluster convergence for index {index} ({status_str})..."
    )

    index_pattern = re.compile(rf"index={index}(\s|,|}})")
    status_pattern = re.compile(rf"status={status_str}(\s|,|}})")
    target_pattern = "clinical::fsm:"

    def check_all_converged() -> bool:
        cluster.refresh_logs()
        for nid in cluster.node_logs:
            found = False
            for line in cluster.node_logs[nid]:
                if (
                    target_pattern in line
                    and index_pattern.search(line)
                    and status_pattern.search(line)
                ):
                    found = True
                    break
            if not found:
                return False
        return True

    poll_until(check_all_converged, timeout=timeout, desc="Convergence")
    print(f"SUCCESS: All nodes converged at index {index}.")
