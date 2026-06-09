#!/usr/bin/env python3
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from typing import Dict, Optional, Tuple, Callable, List

from cluster_harness import (
    NodeConfig,
    NODES,
    ClusterManager,
    wait_for_leader,
    count_elections,
    print_cluster_logs,
    check_connectivity,
    run_client_command,
    extract_version,
    verify_convergence,
    get_complete_lines,
    parse_log_timestamp,
    now_ms,
    build_binaries,
    poll_until,
    Registry,
    MutationFlooder,
    VetoMode,
)

# Lact-O-Sensus: Consensus Verification Suite
# Verifies the Raft "Consensus Heart" and AI Egress logic via isolated test cases.

TMP_CONFIG_DIR = ".tmp_smoke_configs"


# --- 1. Clinical Specifications (Test Cases) ---


@dataclass
class TestCase:
    """Structure for a clinical test specification."""

    name: str
    veto_mode: VetoMode
    func: Callable[[ClusterManager], None]
    setup: Optional[Callable[[List[NodeConfig]], None]] = None


def setup_snapshot_threshold(nodes: List[NodeConfig]) -> None:
    """Specialized setup to force frequent log compaction."""
    print("Action: Configuring snapshot_threshold = 20...")
    for node in nodes:
        with open(node["config"], "a", encoding="utf-8") as f:
            f.write("\n[raft]\nsnapshot_threshold = 20\n")


def test_leader_election(cluster: ClusterManager) -> None:
    """Explicitly tests that a leader can be elected."""
    wait_for_leader(cluster)


def test_leadership_stability(cluster: ClusterManager) -> None:
    """Verifies that heartbeats maintain a stable leader without re-elections."""
    wait_for_leader(cluster)
    initial_count = count_elections(cluster)
    print("Verifying stability for 3s...")
    time.sleep(3)  # Mandatory wait to prove no re-elections happen
    if count_elections(cluster) > initial_count:
        raise RuntimeError("Leadership was unstable (unnecessary re-election detected).")
    print("SUCCESS: Leadership stable.")


def test_leader_failover(cluster: ClusterManager) -> None:
    """Verifies that killing the leader triggers a successful re-election."""
    leader_id = wait_for_leader(cluster)
    base_count = count_elections(cluster)

    log_offsets = {
        n["id"]: (os.path.getsize(n["log"]) if os.path.exists(n["log"]) else 0)
        for n in cluster.nodes
    }
    kill_time = cluster.kill_node(leader_id)

    print("Waiting for re-election...")

    def new_leader_elected() -> Optional[Tuple[int, int]]:
        if count_elections(cluster) > base_count:
            for node in cluster.nodes:
                if node["id"] == leader_id:
                    continue
                for line in get_complete_lines(node["log"], log_offsets.get(node["id"], 0)):
                    if "new_role=Leader" in line and "Role Transition" in line:
                        log_ts = parse_log_timestamp(line)
                        return node["id"], int((log_ts if log_ts > 0 else now_ms()) - kill_time)
        return None

    node_id, duration = poll_until(new_leader_elected, timeout=10, desc="Re-election")
    print(f"SUCCESS: New leader (Node {node_id}) elected in {duration}ms.")


def test_identity_guard(cluster: ClusterManager) -> None:
    """Verifies that the Identity Guard (ADR 004) rejects unauthorized cluster IDs."""
    wait_for_leader(cluster)
    # Use the first node in the dynamic cluster for the check
    node = cluster.nodes[0]
    if check_connectivity(node["id"], node["port"], cluster_id="wrong-cluster"):
        print("SUCCESS: Identity Guard correctly rejected unauthorized request.")
    else:
        raise RuntimeError("Identity Guard failed to reject unauthorized request.")


def test_ai_veto_egress(cluster: ClusterManager) -> None:
    """Verifies that the Leader can successfully call out to the AI Veto Node."""
    leader_id = wait_for_leader(cluster)
    leader_port = next(n["port"] for n in cluster.nodes if n["id"] == leader_id)
    item = Registry.TEST_ITEMS["MILK"]

    print(f"Action: Sending mutation to Leader (Node {leader_id}) on port {leader_port}...")
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
                "intent": {
                    "item_key": item["key"],
                    "quantity": "2",
                    "unit": item["unit"],
                    "category": item["category"],
                    "operation": 1,
                },
            }
        ),
        f"127.0.0.1:{leader_port}",
        "lacto_sensus.v1.IngressService/ProposeMutation",
    ]
    result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if "MUTATION_STATUS_COMMITTED" in result.stdout:
        print("SUCCESS: Leader successfully called out to AI Veto Node and committed the entry.")
    else:
        raise RuntimeError(f"Leader failed to trigger AI evaluation. Out: {result.stdout}")


def test_smart_client_success(cluster: ClusterManager) -> None:
    """Verifies that valid input is successfully committed and converged."""
    leader_id = wait_for_leader(cluster)
    follower_port = next(n["port"] for n in cluster.nodes if n["id"] != leader_id)
    item = Registry.TEST_ITEMS["WATER"]

    print(f"Action: Sending VALID mutation ({item['key']})...")
    output = run_client_command(
        f'add "{item["key"]}" 5 {item["unit"]} {item["category"]}', follower_port
    )

    if "SUCCESS: Committed at version" in output:
        print("SUCCESS: Moral Advocate approved valid mutation.")
        version = extract_version(output)
        if version > 0:
            verify_convergence(cluster, version, "Committed")
    else:
        raise RuntimeError(f"Valid mutation was unexpectedly rejected.\n{output}")


def test_smart_client_veto(cluster: ClusterManager) -> None:
    """Verifies that invalid input is correctly VETOED and converged."""
    leader_id = wait_for_leader(cluster)
    follower_port = next(n["port"] for n in cluster.nodes if n["id"] != leader_id)
    item = Registry.TEST_ITEMS["CIGARETTES"]

    print(f"Action: Sending INVALID mutation ({item['key']})...")
    output = run_client_command(
        f'add "{item["key"]}" 2 {item["unit"]} {item["category"]}', follower_port
    )

    if "VETOED:" in output:
        print("SUCCESS: Moral Advocate correctly blocked unethical mutation.")
        version = extract_version(output)
        if version > 0:
            verify_convergence(cluster, version, "Vetoed")
    else:
        raise RuntimeError(f"Unethical mutation was unexpectedly committed.\n{output}")


def test_linearizable_query_rejection(cluster: ClusterManager) -> None:
    """Verifies that a non-leader node rejects query_state directly."""
    leader_id = wait_for_leader(cluster)
    follower_id = next(n["id"] for n in cluster.nodes if n["id"] != leader_id)
    follower_port = next(n["port"] for n in cluster.nodes if n["id"] == follower_id)

    print(f"Action: Probing FOLLOWER (Node {follower_id}) for query_state...")
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
        f"x-target-node-id: {follower_id}",
        "-d",
        json.dumps({}),
        f"127.0.0.1:{follower_port}",
        "lacto_sensus.v1.IngressService/QueryState",
    ]
    res = subprocess.run(cmd, capture_output=True, text=True, check=False)
    try:
        response = json.loads(res.stdout)
        leader_hint = response.get("leaderHint")
        leader_port = next(n["port"] for n in cluster.nodes if n["id"] == leader_id)
        if (
            response.get("status") == "QUERY_STATUS_REJECTED"
            and leader_hint == f"http://127.0.0.1:{leader_port}"
        ):
            print(f"SUCCESS: Follower correctly rejected query with hint: {leader_hint}")
        else:
            raise RuntimeError(f"Follower failed to reject or provided wrong hint: {res.stdout}")
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Malformed response: {res.stdout}") from exc


def test_persistence_restart(cluster: ClusterManager) -> None:
    """Verifies that inventory state survives a total cluster shutdown."""
    leader_id = wait_for_leader(cluster)
    leader_port = next(n["port"] for n in cluster.nodes if n["id"] == leader_id)
    item = Registry.TEST_ITEMS["APPLE"]

    print(f"Action: Adding test item ({item['key']})...")
    output = run_client_command(
        f'add "{item["key"]}" 1 {item["unit"]} {item["category"]}', leader_port
    )
    version = extract_version(output)
    if version > 0:
        verify_convergence(cluster, version, "Committed")

    cluster.cleanup()
    print("Action: Cluster is OFFLINE.")
    # Standard boot for persistence recovery
    cluster.start_all(veto_mode=VetoMode.REAL, wipe_data=False)

    new_leader_id = wait_for_leader(cluster)
    new_leader_port = next(n["port"] for n in cluster.nodes if n["id"] == new_leader_id)
    output = run_client_command(f"query {item['key']}", new_leader_port)

    if item["key"] in output.lower() and f"1 {item['unit']}" in output:
        print("SUCCESS: Inventory survived total cluster restart.")
    else:
        raise RuntimeError(f"Inventory data lost after restart.\n{output}")


def test_cold_boot_recovery(cluster: ClusterManager) -> None:
    """Verifies that a node can recover FSM state from log when FSM data is lost."""
    leader_id = wait_for_leader(cluster)
    follower_id = next(n["id"] for n in cluster.nodes if n["id"] != leader_id)
    leader_port = next(n["port"] for n in cluster.nodes if n["id"] == leader_id)
    item = Registry.TEST_ITEMS["MILK"]

    run_client_command(f'add "{item["key"]}" 1 {item["unit"]} {item["category"]}', leader_port)
    output = run_client_command(
        f'add "{Registry.TEST_ITEMS["APPLE"]["key"]}" 1 units PrimaryFlora', leader_port
    )
    version = extract_version(output)
    verify_convergence(cluster, version, "Committed")

    cluster.kill_node(follower_id)
    log_path = next(n["log"] for n in cluster.nodes if n["id"] == follower_id)
    log_offset = os.path.getsize(log_path) if os.path.exists(log_path) else 0

    cluster.wipe_node_fsm(follower_id)
    print(f"Action: Restarting Node {follower_id} from existing log...")
    cluster.start_node(follower_id, wipe_data=False)

    def recovered() -> bool:
        rec, applied = False, False
        for line in get_complete_lines(log_path, log_offset):
            if "Recovery: REPLAY COMPLETE" in line and str(version) in line:
                rec = True
            if "Mutation applied to state machine" in line and f"index={version}" in line:
                applied = True
        return rec and applied

    poll_until(recovered, timeout=15.0, desc="Cold-boot recovery")
    print(f"SUCCESS: Node {follower_id} replayed {version} entries and restored FSM state.")


def test_read_your_writes_consistency(cluster: ClusterManager) -> None:
    """Verify that queries block until the requested state version is reached."""
    leader_id = wait_for_leader(cluster)
    leader_port = next(n["port"] for n in cluster.nodes if n["id"] == leader_id)
    item = Registry.TEST_ITEMS["BANANA"]

    output = run_client_command(
        f'add "{item["key"]}" 3 {item["unit"]} {item["category"]}', leader_port
    )
    version = extract_version(output)
    if version == 0:
        raise RuntimeError("Failed to commit mutation for RYW test.")

    print(f"Action: Querying with min_state_version={version}...")
    output = run_client_command(f'query "{item["key"]}" {version}', leader_port)
    if f"Inventory (version: {version}):" not in output:
        raise RuntimeError(f"RYW query failed or wrong version.\n{output}")

    print(f"Action: Querying with future min_state_version={version + 1000} (should fail fast)...")
    output = run_client_command(f'query "{item["key"]}" {version + 1000}', leader_port)
    if "exceeds consistent horizon" not in output:
        raise RuntimeError(f"Expected horizon rejection, but got: {output}")
    print("SUCCESS: Read-Your-Writes consistency verified.")


def test_snapshot_installation(cluster: ClusterManager) -> None:
    """Verifies that a lagging follower is caught up via InstallSnapshot."""
    # 1. Cluster is already started by main loop with the correct threshold
    leader_id = wait_for_leader(cluster)
    follower_id = next(n["id"] for n in cluster.nodes if n["id"] != leader_id)

    print(f"Action: Partitioning Node {follower_id}...")
    cluster.kill_node(follower_id)

    print("Action: Flooding mutations to trigger compaction (>20 entries)...")
    flooder = MutationFlooder(cluster)
    flooder.start()
    poll_until(
        lambda: len(flooder.successful_items) >= 25,
        timeout=15.0,
        desc="Mutation flooding (snapshot trigger)",
    )
    flooder.stop()

    if flooder.exception:
        raise flooder.exception

    print(f"Action: Reconnecting Node {follower_id}...")
    cluster.start_node(follower_id, wipe_data=False)

    # 3. Verification: Semantic Query
    follower_port = next(n["port"] for n in cluster.nodes if n["id"] == follower_id)

    def check_catchup() -> bool:
        try:
            output = run_client_command("query", follower_port)
            ver = extract_version(output)
            print(f"DEBUG: Follower(Node {follower_id}) Version: {ver}")
            return ver >= 20
        except Exception as e:  # pylint: disable=broad-exception-caught
            # ADR 010: Log transient failures during polling to aid forensics
            print(f"DEBUG: Catch-up probe transient failure: {e}")
            return False

    print("Action: Waiting for Node convergence via Snapshot...")
    poll_until(check_catchup, timeout=30.0, desc="Snapshot Catch-up")
    print("SUCCESS: Follower caught up successfully via snapshot.")


def test_replication_chaos(cluster: ClusterManager) -> None:
    """Verify data integrity after multiple SIGKILLs during active replication."""
    wait_for_leader(cluster)
    flooder = MutationFlooder(cluster)
    flooder.start()

    try:
        for i in range(1, 4):
            time.sleep(3)  # Let mutations flow
            victim_id = random.choice([n["id"] for n in cluster.nodes])
            print(f"\n--- Chaos Round {i}: Targeting Node {victim_id} ---")
            cluster.kill_node(victim_id)
            cluster.start_node(victim_id, wipe_data=False)
            wait_for_leader(cluster, timeout=20)

        flooder.stop()
        if flooder.exception:
            raise flooder.exception
        print(f"Action: Chaos stopped. {len(flooder.successful_items)} items committed.")

        time.sleep(1)  # Final settlement
        leader_id = wait_for_leader(cluster)
        leader_port = next(n["port"] for n in cluster.nodes if n["id"] == leader_id)
        output = run_client_command("query", leader_port)

        missing = [
            it
            for it in flooder.successful_items
            if not any(
                it.lower().replace("_", "") in line.lower().replace("_", "")
                for line in output.split("\n")
                if line.strip().startswith("-")
            )
        ]
        if missing:
            raise RuntimeError(f"Data Integrity Violation! Missing: {missing[0]}")

        final_ver = extract_version(output)
        if final_ver > 0:
            cluster.refresh_logs()
            status = "Committed"
            for line in cluster.logs.node_logs.get(leader_id, []):
                if f"apply{{index={final_ver}" in line and "Mutation applied" in line:
                    m = re.search(r"status=(\w+)", line)
                    if m:
                        status = m.group(1)
                    break
            verify_convergence(cluster, final_ver, status)
        print("SUCCESS: 100% Data Integrity achieved after Chaos.")
    finally:
        flooder.stop()


# --- 2. Orchestration Helpers ---


def prepare_test_configs(templates: Dict[int, str]) -> List[NodeConfig]:
    """Creates fresh configuration files from templates in the shared temp directory."""
    os.makedirs(TMP_CONFIG_DIR, exist_ok=True)

    dynamic_nodes = []
    for n in NODES:
        conf_path = os.path.join(TMP_CONFIG_DIR, f"node_{n['id']}.toml")
        with open(conf_path, "w", encoding="utf-8") as f:
            f.write(templates[n["id"]])

        dn = n.copy()
        dn["config"] = conf_path
        dynamic_nodes.append(dn)
    return dynamic_nodes


def run_single_test(test: TestCase, dynamic_nodes: List[NodeConfig]) -> bool:
    """Orchestrates the lifecycle of a single clinical test case."""
    if test.setup:
        test.setup(dynamic_nodes)

    cluster = ClusterManager(dynamic_nodes)
    try:
        cluster.start_all(veto_mode=test.veto_mode, wipe_data=True)
        test.func(cluster)
        return True
    except Exception as e:  # pylint: disable=broad-exception-caught
        print(f"RESULT: FAILED -> {e}")
        print_cluster_logs(dynamic_nodes)
        return False
    finally:
        cluster.cleanup()


# --- 3. Main Runner ---


def main() -> None:
    print("=== Lact-O-Sensus Consensus & Integration Suite ===")
    filter_arg = sys.argv[1].lower() if len(sys.argv) > 1 else None
    build_binaries()

    tests = [
        TestCase("Leader Election", VetoMode.DISABLED, test_leader_election),
        TestCase("Leadership Stability", VetoMode.DISABLED, test_leadership_stability),
        TestCase("Chaos Failover", VetoMode.DISABLED, test_leader_failover),
        TestCase("Identity Guard (ADR 004)", VetoMode.DISABLED, test_identity_guard),
        TestCase(
            "Linearizable Query Rejection", VetoMode.DISABLED, test_linearizable_query_rejection
        ),
        TestCase("AI Veto Egress", VetoMode.REAL, test_ai_veto_egress),
        TestCase("Smart Client (Success Path)", VetoMode.REAL, test_smart_client_success),
        TestCase("Smart Client (Veto Path)", VetoMode.REAL, test_smart_client_veto),
        TestCase(
            "Inventory Durability (Restart Recovery)", VetoMode.REAL, test_persistence_restart
        ),
        TestCase("Cold-Boot Recovery (Log Replay)", VetoMode.REAL, test_cold_boot_recovery),
        TestCase(
            "Snapshot Installation",
            VetoMode.MOCK,
            test_snapshot_installation,
            setup=setup_snapshot_threshold,
        ),
        TestCase("Read-Your-Writes Consistency", VetoMode.REAL, test_read_your_writes_consistency),
        TestCase("Replication Chaos Audit", VetoMode.REAL, test_replication_chaos),
    ]

    # Pre-load config templates to ensure immutability
    templates = {n["id"]: open(n["config"], "r", encoding="utf-8").read() for n in NODES}

    if os.path.exists(TMP_CONFIG_DIR):
        shutil.rmtree(TMP_CONFIG_DIR)

    passed, total_run = 0, 0
    try:
        for test in tests:
            if filter_arg and filter_arg not in test.name.lower():
                continue

            total_run += 1
            print(f"\n[TEST] {test.name}")

            dynamic_nodes = prepare_test_configs(templates)
            if run_single_test(test, dynamic_nodes):
                passed += 1
            else:
                break
            time.sleep(0.5)
    except KeyboardInterrupt:
        print("\nRESULT: ABORTED by user.")
    finally:
        # Note: Forensics are preserved in TMP_CONFIG_DIR (Rule 15).
        pass

    print(f"\n=== Final Result: {passed}/{total_run} Tests Passed ===")
    if not filter_arg and passed < total_run:
        sys.exit(1)


if __name__ == "__main__":
    main()
