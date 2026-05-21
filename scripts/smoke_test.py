#!/usr/bin/env python3
import json
import os
import random
import subprocess
import sys
import threading
import time
from typing import List, Optional

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
)

# Lact-O-Sensus: Consensus Verification Suite
# Verifies the Raft "Consensus Heart" and AI Egress logic via isolated test cases.

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
                    if "Role Transition: -> Leader" in line:
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
                "intent": {
                    "item_key": "oat_milk",
                    "quantity": "2",
                    "unit": "l",
                    "category": "LiquefiedHydration",
                    "operation": 1,
                },
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


def test_smart_client_success() -> None:
    """Verifies that valid input is successfully committed and converged."""
    leader_id = wait_for_leader()
    print("Stabilizing cluster (2s)...")
    time.sleep(2)
    follower_port = next(
        n["port"] for n in NODES if n["id"] != leader_id
    )

    print("Action: Sending VALID mutation (Water)...")
    output = run_client_command(
        'add "water" 5 l LiquefiedHydration', follower_port
    )

    if "SUCCESS: Committed at version" in output:
        print("SUCCESS: Moral Advocate approved valid mutation.")
        version = extract_version(output)
        if version > 0:
            verify_convergence(version, "Committed")
    else:
        print(f"FAILURE: AI rejected valid mutation:\n{output}")
        raise RuntimeError("Valid mutation was unexpectedly rejected.")


def test_smart_client_veto() -> None:
    """Verifies that invalid input is correctly VETOED and converged."""
    leader_id = wait_for_leader()
    print("Stabilizing cluster (2s)...")
    time.sleep(2)
    follower_port = next(
        n["port"] for n in NODES if n["id"] != leader_id
    )

    print("Action: Sending INVALID mutation (Cigarettes)...")
    output = run_client_command(
        'add "cigarettes" 2 pack NutrientSparseCommodities',
        follower_port,
    )

    if "VETOED:" in output:
        print(
            "SUCCESS: Moral Advocate correctly blocked unethical mutation."
        )
        version = extract_version(output)
        if version > 0:
            verify_convergence(version, "Vetoed")
    else:
        print(
            f"FAILURE: AI failed to veto unethical mutation:\n{output}"
        )
        raise RuntimeError(
            "Unethical mutation was unexpectedly committed."
        )


def test_linearizable_query_rejection() -> None:
    """Verifies that a non-leader node rejects query_state directly."""
    leader_id = wait_for_leader()
    follower_id = next(n["id"] for n in NODES if n["id"] != leader_id)
    follower_port = next(
        n["port"] for n in NODES if n["id"] == follower_id
    )

    print(
        f"Action: Probing FOLLOWER (Node {follower_id}) for query_state..."
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
        f"x-target-node-id: {follower_id}",
        "-d",
        json.dumps({}),
        f"127.0.0.1:{follower_port}",
        "lacto_sensus.v1.IngressService/QueryState",
    ]
    result = subprocess.run(
        cmd, capture_output=True, text=True, check=False
    )

    try:
        response = json.loads(result.stdout)
        status = response.get("status")
        leader_hint = response.get("leaderHint")

        leader_port = next(
            n["port"] for n in NODES if n["id"] == leader_id
        )
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
                raise RuntimeError(
                    "Incorrect leader hint in query rejection."
                )
        else:
            print(
                f"FAILURE: Follower did not reject query_state: {result.stdout}"
            )
            raise RuntimeError(
                "Follower failed to reject linearizable query."
            )
    except json.JSONDecodeError as exc:
        print(f"FAILURE: Malformed JSON response: {result.stdout}")
        raise RuntimeError("Malformed response from follower.") from exc


def test_persistence_restart(cluster: ClusterManager) -> None:
    """Verifies that inventory state survives a total cluster shutdown."""
    leader_id = wait_for_leader()
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)
    print(f"Stabilizing cluster around Leader {leader_id} (2s)...")
    time.sleep(2)

    # 1. Add an item
    print("Action: Adding test item (apple)...")
    output = run_client_command(
        'add "apple" 1 units PrimaryFlora', leader_port
    )
    if "SUCCESS" not in output:
        raise RuntimeError(f"Failed to add item: {output}")

    # Ensure it reaches absolute convergence before shutdown
    version = extract_version(output)
    if version > 0:
        print(
            f"Confirmed commitment at version {version}. Verifying cluster-wide convergence..."
        )
        verify_convergence(version, "Committed")
    else:
        print(f"DEBUG: Client Output: {output}")
        raise RuntimeError(
            "Failed to extract version from client output."
        )

    # 2. Total Cluster Shutdown
    cluster.cleanup()
    print(
        "Action: Cluster is OFFLINE. (Causal history exists only on disk)"
    )
    time.sleep(2)

    # 3. Total Cluster Restart (No Wipe)
    cluster.start_all(start_veto=True, wipe_data=False)
    print("Waiting for cluster recovery...")
    new_leader_id = wait_for_leader()
    new_leader_port = next(
        n["port"] for n in NODES if n["id"] == new_leader_id
    )

    # 4. Verify item existence via the new leader
    print(
        f"Action: Verifying item survival via authoritative Leader {new_leader_id}..."
    )

    output = run_client_command("query apple", new_leader_port)

    if "apple" in output.lower() and "1 units" in output:
        print("SUCCESS: Inventory survived total cluster restart.")
    else:
        print(f"FAILURE: Item not found after restart:\n{output}")
        raise RuntimeError("Inventory data lost after total shutdown.")


def test_cold_boot_recovery(cluster: ClusterManager) -> None:
    """Verifies that a node can recover FSM state from log when FSM data is lost."""
    leader_id = wait_for_leader()
    # 1. Identify a follower dynamically
    follower_id = next(n["id"] for n in NODES if n["id"] != leader_id)
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

    # 2. Add test items to ensure non-zero log index
    print("Action: Adding test items (milk, apple)...")
    run_client_command('add "milk" 1 l LiquefiedHydration', leader_port)
    output = run_client_command(
        'add "apple" 1 units PrimaryFlora', leader_port
    )
    version = extract_version(output)
    if version == 0:
        raise RuntimeError("Failed to commit items for recovery test.")

    verify_convergence(version, "Committed")

    # 3. Kill the node FIRST, then record log offset
    cluster.kill_node(follower_id)
    log_path = next(n["log"] for n in NODES if n["id"] == follower_id)
    log_offset = (
        os.path.getsize(log_path) if os.path.exists(log_path) else 0
    )

    # 4. Surgical Wipe: Delete ONLY the FSM database of the follower
    cluster.wipe_node_fsm(follower_id)

    # 5. Restart the follower (Wipe=False, so log survives)
    print(f"Action: Restarting Node {follower_id} from existing log...")
    cluster.start_node(follower_id, wipe_data=False)

    # 6. Verify recovery in the follower's logs (looking only at new lines)
    print(f"Action: Verifying recovery logs for Node {follower_id}...")
    recovered = False
    fsm_applied = False
    start_time = time.time()
    while (time.time() - start_time) < 15.0:
        for line in get_complete_lines(log_path, log_offset):
            if (
                "Recovery: REPLAY COMPLETE" in line
                and str(version) in line
            ):
                recovered = True
            # FSM marker proving the data was actually applied to the DB
            if (
                "Mutation applied to state machine" in line
                and f"index={version}" in line
            ):
                fsm_applied = True
        if recovered and fsm_applied:
            break
        time.sleep(0.5)

    if not recovered:
        print_cluster_logs(20)
        raise RuntimeError(
            f"Node {follower_id} failed to perform recovery replay log."
        )
    if not fsm_applied:
        raise RuntimeError(
            f"Node {follower_id} logged recovery completion but FSM markers are missing."
        )

    print(
        f"SUCCESS: Node {follower_id} replayed {version} entries and restored FSM state."
    )


def test_read_your_writes_consistency() -> None:
    """Verify that queries block until the requested state version is reached."""
    leader_id = wait_for_leader()
    leader_port = next(n["port"] for n in NODES if n["id"] == leader_id)

    # 1. Read-Your-Writes Success Path
    print("Action: Proposing mutation to get a valid state version...")
    output = run_client_command(
        'add "banana" 3 units PrimaryFlora', leader_port
    )
    version = extract_version(output)
    if version == 0:
        raise RuntimeError(
            f"Failed to commit mutation for RYW test: {output}"
        )

    print(
        f"Action: Querying with min_state_version={version} (should succeed)..."
    )
    output = run_client_command(
        f'query "banana" {version}', leader_port
    )
    if f"Inventory (version: {version}):" not in output:
        raise RuntimeError(
            f"Query with min_version {version} failed or returned wrong version: {output}"
        )
    # Use flexible check for normalized item keys (e.g. 'banana_units')
    if "banana" not in output.lower() or "3 units" not in output:
        raise RuntimeError(
            f"Expected item 'banana' missing from RYW query: {output}"
        )

    # 2. Strict Horizon Rejection Path
    future_version = version + 1000
    print(
        f"Action: Querying with future min_state_version={future_version} "
        "(should fail immediately)..."
    )
    # This should fail fast because it exceeds the horizon.
    output = run_client_command(
        f'query "banana" {future_version}', leader_port
    )
    if "exceeds consistent horizon" not in output:
        raise RuntimeError(
            f"Expected horizon rejection for version {future_version}, but got: {output}"
        )

    print(
        "SUCCESS: Read-Your-Writes consistency and Strict Horizon verified."
    )


class MutationFlooder(threading.Thread):
    """Continuously spams mutations to the cluster in a background thread."""

    GROCERY_LEXICON = [
        "milk",
        "bread",
        "apple",
        "cheese",
        "water",
        "carrot",
    ]

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
                base_name = self.GROCERY_LEXICON[
                    self.counter % len(self.GROCERY_LEXICON)
                ]
                item_name = f"{base_name}_{self.counter}"

                # Dynamic Seed Selection: Pick a living node to avoid stale seeds
                living_ports = [
                    n["port"]
                    for n in NODES
                    if n["id"] in self.cluster.processes
                ]
                if not living_ports:
                    time.sleep(0.5)
                    continue

                seed_port = random.choice(living_ports)

                try:
                    output = run_client_command(
                        f'add "{item_name}" 1 units PrimaryFlora',
                        seed_port,
                        timeout=15,
                    )
                    if "SUCCESS: Committed" in output:
                        print(f"DEBUG: Flooder committed '{item_name}'")
                        self.successful_items.append(item_name)
                    else:
                        # Log failure for visibility
                        first_line = output.split("\n", maxsplit=1)[0]
                        print(
                            f"DEBUG: Flooder failed '{item_name}': {first_line}"
                        )
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
    wait_for_leader()

    flooder = MutationFlooder(cluster)
    print("Action: Starting background mutation flood...")
    flooder.start()

    try:
        # Perform 3 rounds of chaos
        for i in range(1, 4):
            time.sleep(3)  # Let some mutations fly
            victim_id = random.choice([n["id"] for n in NODES])
            print(
                f"\n--- Chaos Round {i}: Targeting Node {victim_id} ---"
            )

            cluster.kill_node(victim_id)
            time.sleep(2)  # Wait for cluster to react

            print(f"Action: Restarting Node {victim_id}...")
            cluster.start_node(victim_id, wipe_data=False)

            # Wait for cluster to re-stabilize and elect a leader
            wait_for_leader(timeout=20)

        print("\nAction: Chaos phase complete. Stopping flood...")
        flooder.stop()
        if flooder.exception:
            raise flooder.exception

        print(
            f"Action: Flood stopped. {len(flooder.successful_items)} items successfully committed."
        )
        if flooder.successful_items:
            print(
                f"DEBUG: Expected items: {', '.join(flooder.successful_items)}"
            )

        # 1. Final Convergence Check
        # Get the highest version among successful mutations
        # Actually, we can just wait for a bit and then check the leader.
        time.sleep(3)
        leader_id = wait_for_leader()
        leader_port = next(
            n["port"] for n in NODES if n["id"] == leader_id
        )

        # 2. Verify Data Parity
        print("Action: Verifying final inventory parity on Leader...")
        inventory_output = run_client_command("query", leader_port)
        print(f"DEBUG: Final Inventory Output:\n{inventory_output}")

        missing_items = []
        # Normalize output for easier matching
        normalized_output = inventory_output.lower().replace("_", "")
        for item in flooder.successful_items:
            # Check for the item key as a distinct entry in the inventory
            # We look for the pattern " - {item_name} "
            search_key = item.lower().replace("_", "")
            if f"- {search_key} (" not in normalized_output:
                missing_items.append(item)

        if missing_items:
            raise RuntimeError(
                f"Data Integrity Violation! {len(missing_items)} successful items "
                "missing from Leader inventory: {missing_items[:5]}..."
            )

        # 3. Verify Cluster-Wide Convergence via Logs
        # We check that every node reached at least the same FSM index.
        # We'll use a conservative version from the leader's output.
        final_version = extract_version(inventory_output)
        if final_version > 0:
            verify_convergence(final_version, "Committed")

        print(
            "SUCCESS: 100% Data Integrity and Parity achieved after Chaos."
        )

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
        (
            "Linearizable Query Rejection",
            False,
            lambda c: test_linearizable_query_rejection(),
        ),
        ("AI Veto Egress", True, lambda c: test_ai_veto_egress()),
        (
            "Smart Client (Success Path)",
            True,
            lambda c: test_smart_client_success(),
        ),
        (
            "Smart Client (Veto Path)",
            True,
            lambda c: test_smart_client_veto(),
        ),
        (
            "Inventory Durability (Restart Recovery)",
            True,
            # pylint: disable=W0108
            lambda c: test_persistence_restart(c),
        ),
        (
            "Cold-Boot Recovery (Log Replay)",
            True,
            # pylint: disable=W0108
            lambda c: test_cold_boot_recovery(c),
        ),
        (
            "Read-Your-Writes Consistency",
            True,
            lambda c: test_read_your_writes_consistency(),
        ),
        (
            "Replication Chaos Audit",
            True,
            # pylint: disable=W0108
            lambda c: test_replication_chaos(c),
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
            time.sleep(1)

    print(f"\n=== Final Result: {passed}/{total_run} Tests Passed ===")
    if total_run > 0 and passed < total_run:
        sys.exit(1)


if __name__ == "__main__":
    main()
