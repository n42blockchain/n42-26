#!/usr/bin/env bash
set -euo pipefail

HIVE_TESTS_DIR="${HIVE_TESTS_DIR:?HIVE_TESTS_DIR must point to an ethereum/hive checkout}"
OUT_DIR="${OUT_DIR:-$PWD/hive_assets}"
FIXTURES_URL="${FIXTURES_URL:-https://github.com/ethereum/execution-spec-tests/releases/download/v5.3.0/fixtures_develop.tar.gz}"

mkdir -p "$OUT_DIR"

cd "$HIVE_TESTS_DIR"
go build .

# Build only the simulator images needed by the execution-spec shard lane.
./hive \
    --sim "ethereum/eels/consume-engine" \
    --sim.buildarg "fixtures=${FIXTURES_URL}" \
    -sim.timelimit 1s || true

./hive \
    --sim "ethereum/eels/consume-rlp" \
    --sim.buildarg "fixtures=${FIXTURES_URL}" \
    -sim.timelimit 1s || true

docker save hive/hiveproxy:latest -o "$OUT_DIR/hiveproxy.tar"
docker save hive/simulators/ethereum/eels/consume-engine:latest -o "$OUT_DIR/eels_engine.tar"
docker save hive/simulators/ethereum/eels/consume-rlp:latest -o "$OUT_DIR/eels_rlp.tar"

mv ./hive "$OUT_DIR/hive"
