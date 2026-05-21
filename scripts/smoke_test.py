#!/usr/bin/env python3
import json
import os
import random
import subprocess
import sys
import threading
import time
from typing import List, Optional, Tuple

from cluster_harness import (
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
)

# Lact-O-Sensus: Consensus Verification Suite
# Verifies the Raft "Consensus Heart" and AI Egress logic via isolated test cases.

# --- Test Cases ---


def test_leader_election(cluster: ClusterManager) -> None:
    """Explicitly tests that a leader can be elected."""
    wait_for_leader(cluster)


def test_leadership_stability(cluster: ClusterManager) -> None:
    """Verifies that heartbeats maintain a stable leader without re-elections."""
    wait_for_leader(cluster)
    initial_count = count_elections(cluster)
    print("Verifying stability for 3s...")

    # We wait a fixed duration to prove no re-elections happen during normal operation.
    time.sleep(3)

    if count_elections(cluster) > initial_count:
        raise RuntimeError(
            "Leadership was unstable (unnecessary re-election detected)."
        )
    print("SUCCESS: Leadership stable.")


def test_leader_failover(cluster: ClusterManager) -> None:
    """Verifies that killing the leader triggers a successful re-election."""
    leader_id = wait_for_leader(cluster)
    base_count = count_elections(cluster)

    log_offsets = {
        n["id"]: (os.path.getsize(n["log"]) if os.path.exists(n["log"]) else 0)
        for n in NODES
    }
    kill_time = cluster.kill_node(leader_id)

    print("Waiting for re-election...")

    def new_leader_elected() -> Optional[Tuple[int, int]]:
        if count_elections(cluster) > base_count:
            for node in NODES:
                if node["id"] == leader_id:
                    continue
                for line in get_complete_lines(
                    node["log"], log_offsets.get(node["id"], 0)
                ):
                    if "Role Transition: -> Leader" in line:
                        log_ts = parse_log_timestamp(line)
                        duration = int((log_ts if log_ts > 0 else now_ms()) - kill_time)
                        return node["id"], duration
        return None

    node_id, duration = poll_until(new_leader_elected, timeout=10, desc="Re-election")
    print(f"SUCCESS: New leader (Node {node_id}) elected in {duration}ms.")


def test_identity_guard(cluster: ClusterManager) -> None:
    """Verifies that the Identity Guard (ADR 004) rejects unauthorized cluster IDs."""
    wait_for_leader(cluster)
    if check_connectivity(1, 50051, cluster_id="wrong-cluster"):
        print("SUCCESS: Identity Guard correctly rejected unauthorized request.")
    else:
        raise RuntimeError("Identity Guard failed to reject unauthorized request.")


def test_ai_veto_egress(cluster: ClusterManager) -> None:
    """Verifies that the Leader can successfully call out to the AI Veto Node."""
    leader_id = wait_for_leader(cluster)
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

    item = Registry.TEST_ITEMS["MILK"]
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
        print(
            "SUCCESS: Leader successfully called out to AI Veto Node and committed the entry."
        )
    else:
        print(f"FAILURE: Unexpected response from leader: {result.stdout} {result.stderr}")
        raise RuntimeError("Leader failed to trigger AI evaluation or received error.")


def test_smart_client_success(cluster: ClusterManager) -> None:
    """Verifies that valid input is successfully committed and converged."""
    # wait_for_leader now ensures stability, replacing the old time.sleep(2)
    leader_id = wait_for_leader(cluster)

    follower_port = next(n["port"] for n in NODES if n["id"] != leader_id)

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
        print(f"FAILURE: AI rejected valid mutation:\n{output}")
        raise RuntimeError("Valid mutation was unexpectedly rejected.")


def test_smart_client_veto(cluster: ClusterManager) -> None:
    """Verifies that invalid input is correctly VETOED and converged."""
    # wait_for_leader now ensures stability, replacing the old time.sleep(2)
    leader_id = wait_for_leader(cluster)

    follower_port = next(n["port"] for n in NODES if n["id"] != leader_id)

    item = Registry.TEST_ITEMS["CIGARETTES"]
    print(f"Action: Sending INVALID mutation ({item['key']})...")
    output = run_client_command(
        f'add "{item["key"]}" 2 {item["unit"]} {item["category"]}',
        follower_port,
    )

    if "VETOED:" in output:
        print("SUCCESS: Moral Advocate correctly blocked unethical mutation.")
        version = extract_version(output)
        if version > 0:
            verify_convergence(cluster, version, "Vetoed")
    else:
        print(f"FAILURE: AI failed to veto unethical mutation:\n{output}")
        raise RuntimeError("Unethical mutation was unexpectedly committed.")


def test_linearizable_query_rejection(cluster: ClusterManager) -> None:
    """Verifies that a non-leader node rejects query_state directly."""
    leader_id = wait_for_leader(cluster)
    follower_id = next(n["id"] for n in NODES if n["id"] != leader_id)
    follower_port = next(n["port"] for n in NODES if n["id"] == follower_id)

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
    result = subprocess.run(cmd, capture_output=True, text=True, check=False)

    try:
        response = json.loads(result.stdout)
        status = response.get("status")
        leader_hint = response.get("leaderHint")

        leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)
        expected_hint = f"http://127.0.0.1:{leader_port}"

        if status == "QUERY_STATUS_REJECTED":
            if leader_hint == expected_hint:
                print(
                    "SUCCESS: Follower correctly rejected query_state "
                    f"with accurate hint: {leader_hint}"
                )
            else:
                print(
                    "FAILURE: Follower provided incorrect leader hint. "
                    f"Expected {expected_hint}, got {leader_hint}"
                )
                raise RuntimeError("Incorrect leader hint in query rejection.")
        else:
            print(f"FAILURE: Follower did not reject query_state: {result.stdout}")
            raise RuntimeError("Follower failed to reject linearizable query.")
    except json.JSONDecodeError as exc:
        print(f"FAILURE: Malformed JSON response: {result.stdout}")
        raise RuntimeError("Malformed response from follower.") from exc


def test_persistence_restart(cluster: ClusterManager) -> None:
    """Verifies that inventory state survives a total cluster shutdown."""
    # wait_for_leader now ensures stability, replacing the old time.sleep(2)
    leader_id = wait_for_leader(cluster)
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

    # 1. Add an item
    item = Registry.TEST_ITEMS["APPLE"]
    print(f"Action: Adding test item ({item['key']})...")
    output = run_client_command(
        f'add "{item["key"]}" 1 {item["unit"]} {item["category"]}', leader_port
    )
    if "SUCCESS" not in output:
        raise RuntimeError(f"Failed to add item: {output}")

    # Ensure it reaches absolute convergence before shutdown
    version = extract_version(output)
    if version > 0:
        print(
            f"Confirmed commitment at version {version}. Verifying cluster-wide convergence..."
        )
        verify_convergence(cluster, version, "Committed")
    else:
        print(f"DEBUG: Client Output: {output}")
        raise RuntimeError("Failed to extract version from client output.")

    # 2. Total Cluster Shutdown
    cluster.cleanup()
    print("Action: Cluster is OFFLINE. (Causal history exists only on disk)")

    # 3. Total Cluster Restart (No Wipe)
    cluster.start_all(start_veto=True, wipe_data=False)
    print("Waiting for cluster recovery...")
    new_leader_id = wait_for_leader(cluster)
    new_leader_port = next(n["port"] for n in NODES if n["id"] == new_leader_id)

    # 4. Verify item existence via the new leader
    print(
        f"Action: Verifying item survival via authoritative Leader {new_leader_id}..."
    )

    output = run_client_command(f"query {item['key']}", new_leader_port)

    if item["key"] in output.lower() and f"1 {item['unit']}" in output:
        print("SUCCESS: Inventory survived total cluster restart.")
    else:
        print(f"FAILURE: Item not found after restart:\n{output}")
        raise RuntimeError("Inventory data lost after total shutdown.")


def test_cold_boot_recovery(cluster: ClusterManager) -> None:
    """Verifies that a node can recover FSM state from log when FSM data is lost."""
    leader_id = wait_for_leader(cluster)
    # 1. Identify a follower dynamically
    follower_id = next(n["id"] for n in NODES if n["id"] != leader_id)
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

    # 2. Add test items to ensure non-zero log index
    item1 = Registry.TEST_ITEMS["MILK"]
    item2 = Registry.TEST_ITEMS["APPLE"]
    print(f"Action: Adding test items ({item1['key']}, {item2['key']})...")
    run_client_command(
        f'add "{item1["key"]}" 1 {item1["unit"]} {item1["category"]}', leader_port
    )
    output = run_client_command(
        f'add "{item2["key"]}" 1 {item2["unit"]} {item2["category"]}', leader_port
    )
    version = extract_version(output)
    if version == 0:
        raise RuntimeError("Failed to commit items for recovery test.")

    verify_convergence(cluster, version, "Committed")

    # 3. Kill the node FIRST, then record log offset
    cluster.kill_node(follower_id)
    log_path = next(n["log"] for n in NODES if n["id"] == follower_id)
    log_offset = (os.path.getsize(log_path) if os.path.exists(log_path) else 0)

    # 4. Surgical Wipe: Delete ONLY the FSM database of the follower
    cluster.wipe_node_fsm(follower_id)

    # 5. Restart the follower (Wipe=False, so log survives)
    print(f"Action: Restarting Node {follower_id} from existing log...")
    cluster.start_node(follower_id, wipe_data=False)

    # 6. Verify recovery in the follower's logs (looking only at new lines)
    print(f"Action: Verifying recovery logs for Node {follower_id}...")

    def recovered_and_applied() -> bool:
        recovered = False
        fsm_applied = False
        for line in get_complete_lines(log_path, log_offset):
            if "Recovery: REPLAY COMPLETE" in line and str(version) in line:
                recovered = True
            if "Mutation applied to state machine" in line and f"index={version}" in line:
                fsm_applied = True
        return recovered and fsm_applied

    poll_until(recovered_and_applied, timeout=15.0, desc="Cold-boot recovery")
    print(
        f"SUCCESS: Node {follower_id} replayed {version} entries and restored FSM state."
    )


def test_read_your_writes_consistency(cluster: ClusterManager) -> None:
    """Verify that queries block until the requested state version is reached."""
    leader_id = wait_for_leader(cluster)
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

    # 1. Read-Your-Writes Success Path
    item = Registry.TEST_ITEMS["BANANA"]
    print("Action: Proposing mutation to get a valid state version...")
    output = run_client_command(
        f'add "{item["key"]}" 3 {item["unit"]} {item["category"]}', leader_port
    )
    version = extract_version(output)
    if version == 0:
        raise RuntimeError(f"Failed to commit mutation for RYW test: {output}")

    print(f"Action: Querying with min_state_version={version} (should succeed)...")
    output = run_client_command(f'query "{item["key"]}" {version}', leader_port)
    if f"Inventory (version: {version}):" not in output:
        raise RuntimeError(
            f"Query with min_version {version} failed or returned wrong version: {output}"
        )
    # Use flexible check for normalized item keys (e.g. 'banana_units')
    if item["key"] in output.lower() and f"3 {item['unit']}" in output:
        pass
    else:
        raise RuntimeError(f"Expected item '{item['key']}' missing from RYW query: {output}")

    # 2. Strict Horizon Rejection Path
    future_version = version + 1000
    print(
        f"Action: Querying with future min_state_version={future_version} "
        "(should fail immediately)..."
    )
    # This should fail fast because it exceeds the horizon.
    output = run_client_command(f'query "{item["key"]}" {future_version}', leader_port)
    if "exceeds consistent horizon" not in output:
        raise RuntimeError(
            f"Expected horizon rejection for version {future_version}, but got: {output}"
        )

    print("SUCCESS: Read-Your-Writes consistency and Strict Horizon verified.")


class MutationFlooder(threading.Thread):
    """Continuously spams mutations to the cluster in a background thread."""

    def __init__(self, cluster: ClusterManager) -> None:
        super().__init__(daemon=True)
        self.cluster = cluster
        self.stop_event = threading.Event()
        self.successful_items: List[str] = []
        self.counter = 0
        self.exception: Optional[Exception] = None

    def run(self) -> None:
        try:
            while not self.stop_event.is_set():
                self.counter += 1
                lexicon = [item["key"] for item in Registry.TEST_ITEMS.values()]
                base_name = lexicon[self.counter % len(lexicon)]
                item_name = f"{base_name}_{self.counter}"

                # Dynamic Seed Selection: Pick a living node to avoid stale seeds
                living_ports = [
                    n["port"] for n in NODES if n["id"] in self.cluster.processes
                ]
                if not living_ports:
                    time.sleep(0.5)
                    continue

                seed_port = random.choice(living_ports)

                # Use a valid category for all flood items
                category = Registry.CATEGORIES[0]

                try:
                    output = run_client_command(
                        f'add "{item_name}" 1 units {category}',
                        seed_port,
                        timeout=15,
                    )
                    if "SUCCESS: Committed" in output:
                        print(f"DEBUG: Flooder committed '{item_name}'")
                        self.successful_items.append(item_name)
                    else:
                        # Log failure for visibility
                        first_line = output.split("\n", maxsplit=1)[0]
                        print(f"DEBUG: Flooder failed '{item_name}': {first_line}")
                except (subprocess.TimeoutExpired, RuntimeError) as e:
                    print(f"DEBUG: Flooder error '{item_name}': {e}")
                time.sleep(0.1)
        except Exception as e:  # pylint: disable=broad-except
            self.exception = e

    def stop(self) -> None:
        self.stop_event.set()
        self.join()


def test_replication_chaos(cluster: ClusterManager) -> None:
    """Verify data integrity after multiple SIGKILLs during active replication."""
    wait_for_leader(cluster)

    flooder = MutationFlooder(cluster)
    print("Action: Starting background mutation flood...")
    flooder.start()

    try:
        # Perform 3 rounds of chaos
        for i in range(1, 4):
            # Wait for some mutations to commit
            time.sleep(3)
            victim_id = random.choice([n["id"] for n in NODES])
            print(f"\n--- Chaos Round {i}: Targeting Node {victim_id} ---")

            cluster.kill_node(victim_id)

            print(f"Action: Restarting Node {victim_id}...")
            cluster.start_node(victim_id, wipe_data=False)

            # Wait for cluster to re-stabilize and elect a leader
            wait_for_leader(cluster, timeout=20)

        print("\nAction: Chaos phase complete. Stopping flood...")
        flooder.stop()
        if flooder.exception:
            raise flooder.exception

        print(
            f"Action: Flood stopped. {len(flooder.successful_items)} items successfully committed."
        )

        # 1. Final Convergence Check
        # Wait for a bit to let any pending replications settle (using minimal delay)
        time.sleep(1)
        leader_id = wait_for_leader(cluster)
        leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

        # 2. Verify Data Parity
        print("Action: Verifying final inventory parity on Leader...")
        inventory_output = run_client_command("query", leader_port)

        missing_items = []
        # Normalize output for easier matching
        normalized_output = inventory_output.lower().replace("_", "")
        for item in flooder.successful_items:
            search_key = item.lower().replace("_", "")
            if f"- {search_key} (" not in normalized_output:
                missing_items.append(item)

        if missing_items:
            raise RuntimeError(
                f"Data Integrity Violation! {len(missing_items)} successful items "
                f"missing from Leader inventory. Example: {missing_items[0]}"
            )

        # 3. Verify Cluster-Wide Convergence via Logs
        final_version = extract_version(inventory_output)
        if final_version > 0:
            verify_convergence(cluster, final_version, "Committed")

        print("SUCCESS: 100% Data Integrity and Parity achieved after Chaos.")

    finally:
        flooder.stop()


# --- Runner Logic ---


def main() -> None:
    print("=== Lact-O-Sensus Consensus & Integration Suite ===")

    # Allow filtering tests by name via command line
    filter_arg = sys.argv[1] if len(sys.argv) > 1 else None

    # Step 0: Build binaries once per suite run
    build_binaries()

    tests = [
        (
            "Leader Election",
            False,
            test_leader_election,
        ),
        (
            "Leadership Stability",
            False,
            test_leadership_stability,
        ),
        (
            "Chaos Failover",
            False,
            test_leader_failover,
        ),
        (
            "Identity Guard (ADR 004)",
            False,
            test_identity_guard,
        ),
        (
            "Linearizable Query Rejection",
            False,
            test_linearizable_query_rejection,
        ),
        ("AI Veto Egress", True, test_ai_veto_egress),
        (
            "Smart Client (Success Path)",
            True,
            test_smart_client_success,
        ),
        (
            "Smart Client (Veto Path)",
            True,
            test_smart_client_veto,
        ),
        (
            "Inventory Durability (Restart Recovery)",
            True,
            test_persistence_restart,
        ),
        (
            "Cold-Boot Recovery (Log Replay)",
            True,
            test_cold_boot_recovery,
        ),
        (
            "Read-Your-Writes Consistency",
            True,
            test_read_your_writes_consistency,
        ),
        (
            "Replication Chaos Audit",
            True,
            test_replication_chaos,
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
            # Fail-Fast: stop immediately to preserve logs/state of the failure
            break
        finally:
            cluster.cleanup()
            time.sleep(0.5)

    print(f"\n=== Final Result: {passed}/{total_run} Tests Passed ===")
    if total_run > 0 and passed < total_run:
        sys.exit(1)


if __name__ == "__main__":
    main()
