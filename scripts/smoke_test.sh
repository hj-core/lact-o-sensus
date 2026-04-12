#!/bin/bash
set -e

# Lact-O-Sensus: 3-Node Cluster Smoke Test
# Verifies that nodes can start and respond to gRPC identity checks.

echo "--- Starting 3-Node Local Cluster ---"

# Start nodes in the background
RUST_LOG=info cargo run -p raft-node -- --config crates/raft-node/configs/node_1.toml > node_1.log 2>&1 &
PID1=$!
RUST_LOG=info cargo run -p raft-node -- --config crates/raft-node/configs/node_2.toml > node_2.log 2>&1 &
PID2=$!
RUST_LOG=info cargo run -p raft-node -- --config crates/raft-node/configs/node_3.toml > node_3.log 2>&1 &
PID3=$!

# Function to clean up on exit
cleanup() {
    echo "--- Shutting down cluster ---"
    kill $PID1 $PID2 $PID3 2>/dev/null || true
    wait $PID1 $PID2 $PID3 2>/dev/null || true
    echo "Done."
}
trap cleanup EXIT

echo "Waiting for nodes to initialize..."
sleep 3

echo "--- Verifying Connectivity ---"

check_node() {
    local port=$1
    local name=$2
    echo -n "Checking $name on port $port... "
    if grpcurl -plaintext -import-path crates/common/proto -proto lacto_sensus.proto \
        -d '{"cluster_id": "lacto-dev-01", "term": 1}' \
        127.0.0.1:$port lacto_sensus.v1.ConsensusService/RequestVote > /dev/null 2>&1; then
        echo "OK"
    else
        echo "FAILED"
        exit 1
    fi
}

check_node 50051 "Node 1"
check_node 50052 "Node 2"
check_node 50053 "Node 3"

echo "--- Identity Guard Test ---"
echo -n "Verifying Node 1 rejects wrong cluster_id... "
if grpcurl -plaintext -import-path crates/common/proto -proto lacto_sensus.proto \
    -d '{"cluster_id": "wrong-cluster"}' \
    127.0.0.1:50051 lacto_sensus.v1.ConsensusService/RequestVote 2>&1 | grep -q "Cluster ID mismatch"; then
    echo "OK (Rejected correctly)"
else
    echo "FAILED (Security boundary breached)"
    exit 1
fi

echo ""
echo "SUCCESS: 3-Node Skeleton verified."
