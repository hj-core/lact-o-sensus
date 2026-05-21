import datetime
import json
import os
import re
import shutil
import subprocess
import time
from typing import Dict, List, Optional, TypedDict, IO, Generator

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


def now_ms() -> float:
    """Returns current wall-clock time in milliseconds."""
    return time.time() * 1000


class ClusterManager:
    """Manages the lifecycle of a local 3-node Raft cluster and an AI Veto Node."""

    processes: Dict[int, subprocess.Popen]
    log_files: Dict[int, IO]
    veto_process: Optional[subprocess.Popen]
    veto_log: Optional[IO]

    def __init__(self) -> None:
        self.processes = {}
        self.log_files = {}
        self.veto_process = None
        self.veto_log = None

    def start_node(self, node_id: int, wipe_data: bool = False) -> None:
        """Starts or restarts a specific node."""
        node = next(n for n in NODES if n["id"] == node_id)

        if wipe_data:
            # 1. Wipe diagnostic logs
            if os.path.exists(node["log"]):
                os.remove(node["log"])

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

        cmd = [
            "cargo",
            "run",
            "-p",
            "node-server",
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

    def start_all(
        self, start_veto: bool = False, wipe_data: bool = True
    ) -> None:
        """Starts all nodes defined in NODES and optionally the AI Veto Node."""
        print(
            f"--- Starting cluster (AI Veto: {start_veto}, Wipe: {wipe_data}) ---"
        )

        # 1. Start AI Veto Node if requested
        if start_veto:
            cluster_env = os.environ.copy()
            cluster_env["RUST_LOG"] = "info"
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

        # Give nodes time to initialize and Cargo to finish building if necessary
        time.sleep(5)

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
            raise RuntimeError(
                f"FSM path {fsm_path} not found for wiping."
            )


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
            yield ANSI_ESCAPE.sub("", line)
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
    """Robust leader discovery using most recent election event and term sovereignty."""
    leaders = []  # List of (term, timestamp, node_id)
    for node in NODES:
        for line in get_complete_lines(node["log"], 0):
            if "Transitioning to Leader" in line:
                ts = parse_log_timestamp(line)
                # Try to extract term: "term=4" or "term 4"
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

    # Now verify this leader hasn't demoted in a later log entry
    for line in get_complete_lines(
        next(n["log"] for n in NODES if n["id"] == leader_id), 0
    ):
        if "Role Transition: -> Follower" in line:
            ts = parse_log_timestamp(line)
            # Try to extract term from demotion if available
            term_match = re.search(r"term[= ](\d+)", line)
            demoted_term = int(term_match.group(1)) if term_match else 0

            # If demoted in the same or higher term, or if timestamp is newer
            if ts >= latest_ts and demoted_term >= leader_term:
                return None
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
        if "Role Transition: -> Leader" in line
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


def run_client_command(
    command: str, seed_port: int, timeout: int = 120
) -> str:
    """Helper to run a single command through the client-cli."""
    state_file = ".client_state.json"
    wal_dir = ".client_wal"
    if os.path.exists(state_file):
        os.remove(state_file)
    if os.path.exists(wal_dir):
        shutil.rmtree(wal_dir)

    cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "client-cli",
        "--",
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
    index: int, status_str: str, timeout: float = 5.0
) -> None:
    """Verifies that ALL nodes applied the mutation at the given index."""
    print(
        f"Action: Verifying cluster convergence for index {index} ({status_str})..."
    )
    start = time.time()
    missing = [node["id"] for node in NODES]
    while (time.time() - start) < timeout:
        missing = []
        for node in NODES:
            found = False
            # Robust regex for structured clinical::fsm events.
            # Handles coordinates in either the span context or message body.
            index_pattern = re.compile(rf"index={index}(\s|,|}})")
            status_pattern = re.compile(
                rf"status={status_str}(\s|,|}})"
            )
            target_pattern = "clinical::fsm:"

            for line in get_complete_lines(node["log"], 0):
                if (
                    target_pattern in line
                    and index_pattern.search(line)
                    and status_pattern.search(line)
                ):
                    found = True
                    break
            if not found:
                missing.append(node["id"])

        if not missing:
            print(f"SUCCESS: All nodes converged at index {index}.")
            return
        time.sleep(0.5)

    raise RuntimeError(
        f"Convergence failure. Nodes {missing} did not apply index {index} within {timeout}s."
    )
