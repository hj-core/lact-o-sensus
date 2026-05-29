import datetime
import json
import os
import random
import re
import shutil
import socket
import subprocess
import threading
import time
from enum import Enum, auto
from typing import (
    Dict,
    List,
    Optional,
    TypedDict,
    IO,
    Generator,
    Callable,
    Any,
)

# Lact-O-Sensus: Cluster Test Harness
# Provides the core infrastructure for managing local Raft clusters, AI Veto Nodes, and Clients.


class VetoMode(Enum):
    """Modes for the Clinical Oracle (AI Veto) service."""

    DISABLED = auto()
    REAL = auto()  # Real Ollama/LLM
    MOCK = auto()  # Instant-Approve Mock


# --- 1. Global Configuration & Registries ---


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
        "MILK": {"key": "milk", "category": "LiquefiedHydration", "unit": "l"},
        "BANANA": {"key": "banana", "category": "PrimaryFlora", "unit": "units"},
        "CIGARETTES": {
            "key": "cigarettes",
            "category": "NutrientSparseCommodities",
            "unit": "pack",
        },
    }


# --- 2. Low-Level Utilities ---


def now_ms() -> float:
    """Returns current wall-clock time in milliseconds."""
    return time.time() * 1000


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


def parse_log_timestamp(line: str) -> float:
    """Extracts milliseconds timestamp from a standard tracing line."""
    try:
        ts_str = line.split(" ")[0]
        ts = datetime.datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
        return ts.timestamp() * 1000
    except (ValueError, IndexError):
        return 0.0


def get_complete_lines(log_path: str, offset: int = 0) -> Generator[str, None, int]:
    """Yields complete lines from a file and returns the new offset."""
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


# --- 3. Cluster Infrastructure ---


class MutationFlooder(threading.Thread):
    """
    Background stress-tester that continuously proposes mutations (ADR 007).
    Refactored for readability and isolation (Commit 6).
    """

    def __init__(self, cluster: "ClusterManager") -> None:
        super().__init__(daemon=True)
        self.cluster = cluster
        self.stop_event = threading.Event()
        self.successful_items: List[str] = []
        self.counter = 0
        self.exception: Optional[Exception] = None

    def run(self) -> None:
        """Main flood loop with error isolation and dynamic seed selection."""
        try:
            while not self.stop_event.is_set():
                self._flood_iteration()
                time.sleep(0.1)
        except Exception as e:  # pylint: disable=broad-exception-caught
            self.exception = e

    def stop(self) -> None:
        """Gracefully halts the flooder and waits for the thread to exit."""
        self.stop_event.set()
        self.join()

    # --- Internal Flood Logic ---

    def _flood_iteration(self):
        """Attempts a single mutation proposal across the cluster."""
        self.counter += 1
        item_name = self._generate_item_name()

        # Select a random living node as the seed to avoid stale connections
        living_ports = [
            n["port"] for n in self.cluster.nodes if self.cluster.check_node_alive(n["id"])
        ]
        if not living_ports:
            time.sleep(0.5)
            return

        self._propose_mutation(item_name, random.choice(living_ports))

    def _generate_item_name(self) -> str:
        lexicon = [item["key"] for item in Registry.TEST_ITEMS.values()]
        base = lexicon[self.counter % len(lexicon)]
        return f"{base}_{self.counter}"

    def _propose_mutation(self, name: str, port: int):
        """Dispatches a mutation intent to the target port."""
        try:
            # All flood items use a standard Count unit and the first valid category
            cmd = f'add "{name}" 1 units {Registry.CATEGORIES[0]}'
            output = run_client_command(cmd, port, timeout=15)

            if "SUCCESS: Committed" in output:
                self.successful_items.append(name)
            else:
                print(f"DEBUG: Flooder failed to commit '{name}': {output.strip()}")
        except Exception as e:  # pylint: disable=broad-exception-caught
            print(f"DEBUG: Flooder RPC exception: {e}")


class LogRegistry:
    """Manages in-memory log lines and disk offsets for the cluster."""

    def __init__(self, nodes: List[NodeConfig]):
        self.nodes = nodes
        self.node_logs: Dict[int, List[str]] = {n["id"]: [] for n in nodes}
        self.node_offsets: Dict[int, int] = {n["id"]: 0 for n in nodes}

    def reset_node(self, node_id: int):
        self.node_logs[node_id] = []
        self.node_offsets[node_id] = 0

    def refresh(self):
        """Surgically reads new lines from all node logs."""
        for node in self.nodes:
            self._refresh_node(node["id"], node["log"])

    def _refresh_node(self, nid: int, path: str):
        """Appends new complete lines from disk to the in-memory buffer."""
        if not os.path.exists(path):
            return

        with open(path, "r", encoding="utf-8") as f:
            f.seek(self.node_offsets[nid])
            while (line := f.readline()).endswith("\n"):
                self.node_logs[nid].append(ANSI_ESCAPE.sub("", line))
                self.node_offsets[nid] = f.tell()


class ClusterManager:
    """Orchestrator for local cluster lifecycle and diagnostic operations."""

    def __init__(self, nodes: List[NodeConfig]) -> None:
        self.nodes = nodes
        self.processes: Dict[int, subprocess.Popen] = {}
        self.log_files: Dict[int, IO] = {}
        self.veto_process: Optional[subprocess.Popen] = None
        self.veto_log: Optional[IO] = None
        self.logs = LogRegistry(nodes)

    # --- Lifecycle API ---

    def start_all(self, veto_mode: VetoMode = VetoMode.DISABLED, wipe_data: bool = True) -> None:
        """High-level orchestrator for full cluster boot."""
        print(f"--- Starting cluster (Veto Mode: {veto_mode.name}, Wipe: {wipe_data}) ---")
        if veto_mode != VetoMode.DISABLED:
            self._start_veto_node(veto_mode)
        for node in self.nodes:
            self.start_node(node["id"], wipe_data=wipe_data)
        self._wait_for_initialization(veto_mode)

    def start_node(self, node_id: int, wipe_data: bool = False) -> None:
        """Starts or restarts a specific Raft node."""
        node = next(n for n in self.nodes if n["id"] == node_id)
        if wipe_data:
            self._wipe_node_data(node_id)

        cluster_env = os.environ.copy()
        cluster_env["RUST_LOG"] = "info"
        mode = "w" if wipe_data else "a"
        log_file: IO = open(node["log"], mode, encoding="utf-8")
        self.log_files[node_id] = log_file

        self.processes[node_id] = subprocess.Popen(
            [NODE_SERVER_BIN, "--config", node["config"]],
            stdout=log_file,
            stderr=subprocess.STDOUT,
            env=cluster_env,
        )

    def kill_node(self, node_id: int) -> float:
        """Kills a node and returns the timestamp."""
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
        """Deterministic resource reclamation."""
        print("--- Cleaning up cluster ---")
        self._stop_veto()
        self._stop_raft_nodes()
        self.processes.clear()
        self.log_files.clear()

    # --- Diagnostic & Verification API ---

    def refresh_logs(self):
        self.logs.refresh()

    def check_node_alive(self, node_id: int) -> bool:
        return node_id in self.processes and self.processes[node_id].poll() is None

    def wipe_node_fsm(self, node_id: int) -> None:
        """Surgically deletes only the FSM database for a specific node."""
        fsm_path = f"data/node_{node_id}/fsm"
        print(f"Action: Surgical Wipe of FSM at {fsm_path}")
        if os.path.exists(fsm_path):
            shutil.rmtree(fsm_path)
        else:
            raise RuntimeError(f"FSM path {fsm_path} not found for wiping.")

    # --- Internal Helpers ---

    def _start_veto_node(self, veto_mode: VetoMode):
        if veto_mode == VetoMode.DISABLED:
            return

        if os.path.exists(VETO_LOG):
            os.remove(VETO_LOG)
        self.veto_log = open(VETO_LOG, "w", encoding="utf-8")

        use_mock = veto_mode == VetoMode.MOCK
        bin_path = os.path.join(TARGET_DIR, "mock-veto") if use_mock else AI_VETO_BIN
        args = [bin_path, "--port", str(VETO_PORT)]
        if veto_mode == VetoMode.REAL:
            args.extend(["--model", "qwen3.5:4b"])

        self.veto_process = subprocess.Popen(
            args,
            stdout=self.veto_log,
            stderr=subprocess.STDOUT,
            env={"RUST_LOG": "info", **os.environ},
        )

    def _wait_for_initialization(self, veto_mode: VetoMode):
        print("Action: Waiting for nodes to initialize...")
        for node in self.nodes:
            poll_until(
                lambda n=node: self.check_node_alive(n["id"]),
                timeout=5,
                desc=f"Node {node['id']} startup",
            )

        if veto_mode != VetoMode.DISABLED:
            timeout = 60 if veto_mode == VetoMode.REAL else 30
            print(
                f"Action: Waiting for AI Veto Node ({veto_mode.name})"
                f"to listen on port {VETO_PORT}..."
            )
            poll_until(
                lambda: self._is_port_listening(VETO_PORT),
                timeout=timeout,
                desc=f"AI Veto ({veto_mode.name}) readiness",
            )

    def _is_port_listening(self, port: int) -> bool:
        """Checks if a local TCP port is open."""
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.5)
            return s.connect_ex(("127.0.0.1", port)) == 0

    def _wipe_node_data(self, node_id: int):
        node = next(n for n in self.nodes if n["id"] == node_id)
        if os.path.exists(node["log"]):
            os.remove(node["log"])
        self.logs.reset_node(node_id)
        data_dir = f"data/node_{node_id}"
        if os.path.exists(data_dir):
            shutil.rmtree(data_dir)

    def _stop_veto(self):
        if self.veto_process:
            self.veto_process.terminate()
            try:
                self.veto_process.wait(timeout=2)
            except subprocess.SubprocessError:
                self.veto_process.kill()
        if self.veto_log:
            self.veto_log.close()
        self.veto_process = None
        self.veto_log = None

    def _stop_raft_nodes(self):
        for p in self.processes.values():
            p.terminate()
        for p in self.processes.values():
            try:
                p.wait(timeout=2)
            except subprocess.SubprocessError:
                p.kill()
        for f in self.log_files.values():
            f.close()


# --- 4. High-Level Orchestration Helpers ---


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
        "mock-veto",
        "-p",
        "client-cli",
    ]
    if subprocess.run(cmd, check=False).returncode != 0:
        raise RuntimeError("Cargo build failed.")
    print("SUCCESS: Binaries compiled.")


def find_current_leader(cluster: ClusterManager) -> Optional[int]:
    """Robust leader discovery using stateful log registry."""
    cluster.refresh_logs()
    leaders = []
    for nid, lines in cluster.logs.node_logs.items():
        for line in lines:
            if "Transitioning to Leader" in line:
                term_match = re.search(r"term[= ](\d+)", line)
                leaders.append(
                    (int(term_match.group(1)) if term_match else 0, parse_log_timestamp(line), nid)
                )

    if not leaders:
        return None

    # Sovereign leader has the highest term, then latest timestamp
    leaders.sort(key=lambda x: (x[0], x[1]), reverse=True)
    best_term, best_ts, best_lid = leaders[0]

    # Verify no subsequent demotion
    for line in cluster.logs.node_logs[best_lid]:
        if "Role Transition: -> Follower" in line:
            term_match = re.search(r"term[= ](\d+)", line)
            if (
                parse_log_timestamp(line) >= best_ts
                and (int(term_match.group(1)) if term_match else 0) >= best_term
            ):
                return None
    return best_lid


def wait_for_leader(cluster: ClusterManager, timeout: float = 15.0) -> int:
    """Waits for a leader to emerge and stabilize."""
    print(f"Waiting for leader to emerge (max {timeout}s)...", end="", flush=True)

    def check_stability() -> Optional[int]:
        lid = find_current_leader(cluster)
        if lid:
            time.sleep(0.1)  # Stability buffer
            if find_current_leader(cluster) == lid:
                return lid
        return None

    leader_id = poll_until(check_stability, timeout=timeout, desc="Stable leader")
    print(f" OK (Node {leader_id})")
    return leader_id


def verify_convergence(
    cluster: ClusterManager, index: int, status_str: str, timeout: float = 5.0
) -> None:
    """Verifies cluster-wide convergence for a specific mutation index."""
    print(f"Action: Verifying cluster convergence for index {index} ({status_str})...")
    idx_pat = re.compile(rf"index={index}(\s|,|}})")
    sts_pat = re.compile(rf"status={status_str}(\s|,|}})")

    def all_nodes_applied() -> bool:
        cluster.refresh_logs()
        for _, lines in cluster.logs.node_logs.items():
            if not any(
                "clinical::fsm:" in line and idx_pat.search(line) and sts_pat.search(line)
                for line in lines
            ):
                return False
        return True

    poll_until(all_nodes_applied, timeout=timeout, desc="Convergence")
    print(f"SUCCESS: All nodes converged at index {index}.")


def check_connectivity(target_node_id: int, port: int, cluster_id: str = "probe") -> bool:
    """Evaluation of gRPC status for protocol-level checks."""
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
        json.dumps({"term": 1, "candidate_id": str(peer_id)}),
        f"127.0.0.1:{port}",
        "raft.v1.ConsensusService/RequestVote",
    ]
    res = subprocess.run(cmd, capture_output=True, text=True, check=False)
    return (
        res.returncode == 0
        if cluster_id == "lacto-dev-01"
        else "Code: Unauthenticated" in res.stderr
    )


def run_client_command(command: str, seed_port: int, timeout: int = 120) -> str:
    """Executes a command via the pre-compiled client-cli binary."""
    state_file, wal_dir = ".client_state.json", ".client_wal"
    for p in [state_file, wal_dir]:
        if os.path.exists(p):
            if os.path.isfile(p):
                os.remove(p)
            else:
                shutil.rmtree(p)

    p = subprocess.Popen(
        [CLIENT_CLI_BIN, "--cluster-id", "lacto-dev-01", "--seed", f"127.0.0.1:{seed_port}"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        stdout, _ = p.communicate(f"{command}\nexit\n", timeout=timeout)
        return stdout
    except subprocess.TimeoutExpired:
        p.kill()
        raise
    finally:
        if os.path.exists(state_file):
            os.remove(state_file)


def count_elections(cluster: ClusterManager) -> int:
    cluster.refresh_logs()
    return sum(
        1
        for lines in cluster.logs.node_logs.values()
        for l in lines
        if "Role Transition: -> Leader" in l
    )


def extract_version(output: str) -> int:
    match = re.search(r"version:?\s*(\d+)", output)
    return int(match.group(1)) if match else 0


def print_cluster_logs(nodes: List[NodeConfig], lines: int = 5) -> None:
    print(f"\n--- Diagnostic Tail (Last {lines} lines) ---")
    for n in nodes:
        if os.path.exists(n["log"]):
            print(f"\n--- Node {n['id']} ---")
            subprocess.run(["tail", "-n", str(lines), n["log"]], check=False)
