#!/usr/bin/env bash
set -euo pipefail

# Qualification harness for the preserved runtime-01 seven-validator chain.
# It never initializes, removes, or rewrites a Gov5 data directory. Control
# files and logs live in the separate runtime-12 qualification directory.

source_runtime="${N42_EXISTING_SOURCE_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-01}"
runtime="${N42_EXISTING_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-12-existing-seven-qualification}"
gov_binary="${N42_EXISTING_GOV_BINARY:-$runtime/artifacts/n42-gov5}"
rust_binary="${N42_EXISTING_RUST_BINARY:-$runtime/artifacts/n42-node}"
chain_dir="$source_runtime/hotstuff_testnet"
genesis_hash="0xdd96ceb7730fb4a01f6c42aa42908f8e3f7fb02c665829ec6bd96493079f3658"
observer_peer="12D3KooWFxgMF1PAgc6pWvxiCRcj8P7iXtvTQy86hcPtdW1UQBLc"
replacement_marker="$runtime/control/rust-replaces-gov6"

gov_addresses=(
  "0x95e727fffab1067124993947ae021de7c33db005"
  "0xb9edc207bde0ab32924ef9e55c6ffc560f07e99c"
  "0x908f62e4546f831c48088c42f3dd29435f2c4eb7"
  "0xa566383640dad8cd5bd18406d8d71cad895910f3"
  "0xa19249f2e094da20b1f1f597263213852c7ffc1e"
  "0x818ff715c22272da213cf8523eba3c4564353cfa"
  "0x95c9d673e9cdc7d66905180670b820638bf24eaf"
)
gov_peers=(
  "16Uiu2HAm3Gt3Mb8e4HX7iD6LJKKygiX9qUFjMF2Pticmva2EpxkX"
  "16Uiu2HAmMi116fAgABFFk1SSqkT9XLpX9QxzhTaefR2M3W632csJ"
  "16Uiu2HAm115nVmE3Qie5WrpK5ZCs9ZgNc3qJFoAfyNbiQeScsusP"
  "16Uiu2HAm2caoentKTqnz86v4GPiUV6ckvJF8ToR8Tv7XAsErA7BT"
  "16Uiu2HAkzgUyejVsVowoCCJcRWYa73tj9B8Ks32k4J3DXweSGpRP"
  "16Uiu2HAmQuea689VwG3rxRS9x7ZkDXfRfGVkhEGirtzKGUL4V2LJ"
  "16Uiu2HAmGHiKh3pqQZ32tb3iM6TMJqqCYXKhH7aXh5aUCYU6d3wc"
)

require_file() {
  test -f "$1" || {
    echo "missing required file: $1" >&2
    exit 1
  }
}

pid_alive() {
  test -f "$1" && kill -0 "$(<"$1")" 2>/dev/null
}

stop_one() {
  local pid_file="${1:?pid file required}"
  if pid_alive "$pid_file"; then
    kill "$(<"$pid_file")"
    for _ in $(seq 1 30); do
      pid_alive "$pid_file" || break
      sleep 1
    done
  fi
  if pid_alive "$pid_file"; then
    kill -KILL "$(<"$pid_file")"
  fi
  rm -f "$pid_file"
}

gov_tcp_port() {
  echo $((34300 + $1))
}

gov_rpc_port() {
  echo $((31500 + $1))
}

start_gov_node() {
  local index="${1:?Gov5 node index required}"
  if test "$index" -lt 0 || test "$index" -gt 6; then
    echo "Gov5 node index must be in 0..6: $index" >&2
    return 2
  fi
  if test "$index" -eq 6 && test -f "$replacement_marker"; then
    echo "refusing to start Gov5 node6 while the Rust replacement marker is active" >&2
    return 2
  fi
  require_file "$gov_binary"
  require_file "$chain_dir/node$index/chaindata/mdbx.dat"
  mkdir -p "$runtime/logs" "$runtime/pids"
  local pid_file="$runtime/pids/gov$index.pid"
  if pid_alive "$pid_file"; then
    return
  fi

  local args=(
    --chain private
    --profile n42
    --datadir "$chain_dir/node$index"
    --port "$(gov_tcp_port "$index")"
    --http
    --http.port "$(gov_rpc_port "$index")"
    --mine
    --etherbase "${gov_addresses[$index]}"
    --block-interval-ms "${N42_EXISTING_BLOCK_INTERVAL_MS:-4000}"
    --verbosity "${N42_EXISTING_GOV_VERBOSITY:-3}"
    --p2p.no-discovery
    --p2p.min-sync-peers 0
    --p2p.max-peers 9
    --p2p.genesis-override "$genesis_hash"
    --p2p.peer "/ip4/127.0.0.1/udp/22980/quic-v1/p2p/$observer_peer"
  )
  local peer_index
  for peer_index in 0 1 2 3 4 5 6; do
    if test "$peer_index" -ne "$index"; then
      if test "$peer_index" -eq 6 && test -f "$replacement_marker"; then
        args+=(
          --p2p.peer
          "/ip4/127.0.0.1/udp/34306/quic-v1/p2p/${gov_peers[$peer_index]}"
        )
      else
        args+=(
          --p2p.peer
          "/ip4/127.0.0.1/tcp/$(gov_tcp_port "$peer_index")/p2p/${gov_peers[$peer_index]}"
        )
      fi
    fi
  done
  nohup "$gov_binary" "${args[@]}" >"$runtime/logs/gov$index.log" 2>&1 &
  echo "$!" >"$pid_file"
}

start_gov() {
  local index
  local last_index=6
  if test -f "$replacement_marker"; then
    last_index=5
  fi
  for index in $(seq 0 "$last_index"); do
    start_gov_node "$index"
  done
}

stop_gov() {
  local index
  for index in 0 1 2 3 4 5 6; do
    stop_one "$runtime/pids/gov$index.pid"
  done
}

trusted_gov_peers() {
  local peers=""
  local index
  for index in 0 1 2 3 4 5 6; do
    local peer="/ip4/127.0.0.1/tcp/$(gov_tcp_port "$index")/p2p/${gov_peers[$index]}"
    peers="${peers:+$peers,}$peer"
  done
  printf '%s' "$peers"
}

trusted_replacement_peers() {
  local peers=""
  local index
  for index in 0 1 2 3 4 5; do
    local peer="/ip4/127.0.0.1/tcp/$(gov_tcp_port "$index")/p2p/${gov_peers[$index]}"
    peers="${peers:+$peers,}$peer"
  done
  printf '%s' "$peers"
}

start_observer() {
  require_file "$rust_binary"
  require_file "$chain_dir/genesis.json"
  require_file "$runtime/artifacts/consensus-peer-bound.json"
  require_file "$runtime/artifacts/bootstrap-bundle.json"
  require_file "$runtime/artifacts/observer-p2p.key"
  mkdir -p "$runtime/logs" "$runtime/pids" "$runtime/observer"
  local pid_file="$runtime/pids/observer.pid"
  if pid_alive "$pid_file"; then
    return
  fi
  nohup env \
    N42_CONSENSUS_CONFIG="$runtime/artifacts/consensus-peer-bound.json" \
    N42_DATA_DIR="$runtime/observer/consensus" \
    N42_OBSERVER_MODE=1 \
    N42_GOV5_HEADER_PROFILE=1 \
    N42_INTEROP_GENESIS_HASH="$genesis_hash" \
    N42_GOV5_BOOTSTRAP_BUNDLE="$runtime/artifacts/bootstrap-bundle.json" \
    N42_CONSENSUS_PORT=22980 \
    N42_STARHUB_PORT=9860 \
    N42_NO_AUTO_CONNECT=1 \
    N42_TRUSTED_PEERS="$(trusted_gov_peers)" \
    N42_ENABLE_MDNS=0 \
    N42_ENABLE_DHT=0 \
    N42_ENABLE_HTTP_RPC=1 \
    N42_P2P_KEY="@$runtime/artifacts/observer-p2p.key" \
    "$rust_binary" node \
      --chain "$chain_dir/genesis.json" \
      --datadir "$runtime/observer/reth" \
      --disable-discovery \
      --port 33401 \
      --http \
      --http.addr 127.0.0.1 \
      --http.port 30510 \
      --authrpc.port 30511 \
      --ipcdisable \
      --log.file.max-files 0 \
      --color never \
      -vvv \
      >"$runtime/logs/observer.log" 2>&1 &
  echo "$!" >"$pid_file"
}

start_participant() {
  if ! test -f "$replacement_marker"; then
    echo "refusing to start the participant without an active replacement marker" >&2
    return 2
  fi
  require_file "$rust_binary"
  require_file "$chain_dir/genesis.json"
  require_file "$runtime/artifacts/consensus-peer-bound.json"
  require_file "$runtime/artifacts/bootstrap-bundle.json"
  require_file "$chain_dir/node6/network-keys"
  require_file "$runtime/participant/reth/db/mdbx.dat"
  require_file "$runtime/participant/consensus/gov5-bootstrap-receipt.json"
  local bls_files=("$chain_dir/node6/keystore/"*.key)
  if test "${#bls_files[@]}" -ne 1 || ! test -f "${bls_files[0]}"; then
    echo "expected exactly one Gov5 node6 BLS key file" >&2
    return 2
  fi
  mkdir -p "$runtime/logs" "$runtime/pids"
  local pid_file="$runtime/pids/participant.pid"
  if pid_alive "$pid_file"; then
    return
  fi
  nohup env \
    N42_CONSENSUS_CONFIG="$runtime/artifacts/consensus-peer-bound.json" \
    N42_VALIDATOR_KEY="@${bls_files[0]}" \
    N42_P2P_KEY="@$chain_dir/node6/network-keys" \
    N42_DATA_DIR="$runtime/participant/consensus" \
    N42_GOV5_H2_PARTICIPANT=1 \
    N42_GOV5_HEADER_PROFILE=1 \
    N42_INTEROP_GENESIS_HASH="$genesis_hash" \
    N42_GOV5_BOOTSTRAP_BUNDLE="$runtime/artifacts/bootstrap-bundle.json" \
    N42_CONSENSUS_PORT=34306 \
    N42_STARHUB_PORT=9861 \
    N42_NO_AUTO_CONNECT=1 \
    N42_TRUSTED_PEERS="$(trusted_replacement_peers)" \
    N42_ENABLE_MDNS=0 \
    N42_ENABLE_DHT=0 \
    N42_ENABLE_HTTP_RPC=1 \
    "$rust_binary" node \
      --chain "$chain_dir/genesis.json" \
      --datadir "$runtime/participant/reth" \
      --disable-discovery \
      --port 33406 \
      --http \
      --http.addr 127.0.0.1 \
      --http.port 30516 \
      --authrpc.port 30517 \
      --ipcdisable \
      --log.file.max-files 0 \
      --color never \
      -vvv \
      >"$runtime/logs/participant.log" 2>&1 &
  echo "$!" >"$pid_file"
}

prepare_participant() {
  if pid_alive "$runtime/pids/observer.pid"; then
    echo "observer must be stopped before preparing the participant copy" >&2
    return 2
  fi
  if lsof +D "$runtime/observer" 2>/dev/null | tail -n +2 | rg -q .; then
    echo "observer data directory is still open by a process" >&2
    return 2
  fi
  local snapshot="$runtime/snapshots/observer-after-24h"
  if test -e "$runtime/participant"; then
    echo "refusing to overwrite existing participant directory" >&2
    return 2
  fi
  if test ! -d "$snapshot/observer"; then
    if test -e "$snapshot"; then
      echo "incomplete observer snapshot already exists: $snapshot" >&2
      return 2
    fi
    mkdir -p "$snapshot"
    cp -R -p "$runtime/observer" "$snapshot/observer"
    (
      cd "$snapshot"
      find observer -type f -print0 | sort -z | xargs -0 sha256sum
    ) >"$snapshot/manifest.sha256"
  fi
  (
    cd "$snapshot"
    sha256sum -c manifest.sha256 >/dev/null
  )
  mkdir -p "$runtime/participant"
  cp -R -p "$snapshot/observer/reth" "$runtime/participant/reth"
  cp -R -p "$snapshot/observer/consensus" "$runtime/participant/consensus"
  require_file "$runtime/participant/reth/db/mdbx.dat"
  require_file "$runtime/participant/consensus/gov5-bootstrap-receipt.json"
}

snapshot_replaced_validator_current() {
  if pid_alive "$runtime/pids/gov6.pid"; then
    echo "Gov5 node6 must be stopped before taking the maintenance snapshot" >&2
    return 2
  fi
  if lsof +D "$chain_dir/node6" 2>/dev/null | tail -n +2 | rg -q .; then
    echo "Gov5 node6 data directory is still open by a process" >&2
    return 2
  fi
  local snapshot="$runtime/snapshots/gov-node6-maintenance-window"
  if test ! -d "$snapshot/node6"; then
    if test -e "$snapshot"; then
      echo "incomplete Gov5 node6 maintenance snapshot already exists: $snapshot" >&2
      return 2
    fi
    mkdir -p "$snapshot"
    cp -R -p "$chain_dir/node6" "$snapshot/node6"
    (
      cd "$snapshot"
      find node6 -type f -print0 | sort -z | xargs -0 sha256sum
    ) >"$snapshot/manifest.sha256"
  fi
  (
    cd "$snapshot"
    sha256sum -c manifest.sha256 >/dev/null
  )
}

activate_replacement() {
  if test -f "$replacement_marker"; then
    echo "replacement is already active" >&2
    return 2
  fi
  require_file "$runtime/snapshots/gov-node6-pre-replacement/manifest.sha256"
  (
    cd "$runtime/snapshots/gov-node6-pre-replacement"
    sha256sum -c manifest.sha256 >/dev/null
  )
  require_file "$runtime/participant/reth/db/mdbx.dat"
  require_file "$runtime/participant/consensus/gov5-bootstrap-receipt.json"

  stop_one "$runtime/pids/observer.pid"
  stop_one "$runtime/pids/gov6.pid"
  snapshot_replaced_validator_current
  mkdir -p "$(dirname "$replacement_marker")"
  touch "$replacement_marker"
  local index
  for index in 0 1 2 3 4 5; do
    stop_one "$runtime/pids/gov$index.pid"
    start_gov_node "$index"
  done
  start_participant
}

rollback_replacement() {
  stop_one "$runtime/pids/participant.pid"
  rm -f "$replacement_marker"
  local index
  for index in 0 1 2 3 4 5; do
    stop_one "$runtime/pids/gov$index.pid"
    start_gov_node "$index"
  done
  start_gov_node 6
}

status() {
  local index
  for index in 0 1 2 3 4 5 6; do
    local pid_file="$runtime/pids/gov$index.pid"
    if pid_alive "$pid_file"; then
      echo "gov$index running pid=$(<"$pid_file")"
    else
      echo "gov$index stopped"
    fi
  done
  if pid_alive "$runtime/pids/observer.pid"; then
    echo "observer running pid=$(<"$runtime/pids/observer.pid")"
  else
    echo "observer stopped"
  fi
  if pid_alive "$runtime/pids/participant.pid"; then
    echo "participant running pid=$(<"$runtime/pids/participant.pid")"
  else
    echo "participant stopped"
  fi
  local port
  for port in 31500 31501 31502 31503 31504 31505 31506 30510 30516; do
    local response
    response="$(curl -fsS --max-time 2 \
      -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
      "http://127.0.0.1:$port" 2>/dev/null || true)"
    if test -n "$response"; then
      echo "rpc:$port $(printf '%s' "$response" |
        jq -r '.result | [.number,.hash,.stateRoot] | @tsv')"
    fi
  done
}

monitor_observer() {
  local duration_seconds="${1:?duration seconds required}"
  local interval_seconds="${2:-60}"
  local evidence_file="${3:-$runtime/evidence/p6-observer-24h-soak.jsonl}"
  local deadline=$((SECONDS + duration_seconds))
  mkdir -p "$(dirname "$evidence_file")"

  while test "$SECONDS" -lt "$deadline"; do
    local sample_dir
    sample_dir="$(mktemp -d)"
    local ok=true
    local min_height=-1
    local max_height=0
    local port
    for port in 31500 31501 31502 31503 31504 31505 31506 30510; do
      if ! curl -fsS --max-time 5 \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
        "http://127.0.0.1:$port" >"$sample_dir/$port.json"; then
        ok=false
        break
      fi
      local height_hex height
      height_hex="$(jq -er '.result.number' "$sample_dir/$port.json")"
      height=$((height_hex))
      if test "$min_height" -lt 0 || test "$height" -lt "$min_height"; then
        min_height="$height"
      fi
      if test "$height" -gt "$max_height"; then
        max_height="$height"
      fi
    done

    if test "$ok" = true; then
      local common_hex expected_hash expected_root expected_receipts_root
      common_hex="$(printf '0x%x' "$min_height")"
      expected_hash=""
      expected_root=""
      expected_receipts_root=""
      for port in 31500 31501 31502 31503 31504 31505 31506 30510; do
        local response hash root receipts_root
        response="$(curl -fsS --max-time 5 \
          -H 'content-type: application/json' \
          --data "$(jq -nc --arg height "$common_hex" \
            '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$height,false]}')" \
          "http://127.0.0.1:$port")"
        hash="$(printf '%s' "$response" | jq -er '.result.hash')"
        root="$(printf '%s' "$response" | jq -er '.result.stateRoot')"
        receipts_root="$(printf '%s' "$response" | jq -er '.result.receiptsRoot')"
        if test -z "$expected_hash"; then
          expected_hash="$hash"
          expected_root="$root"
          expected_receipts_root="$receipts_root"
        elif test "$hash" != "$expected_hash" ||
          test "$root" != "$expected_root" ||
          test "$receipts_root" != "$expected_receipts_root"; then
          ok=false
          break
        fi
      done
      if test $((max_height - min_height)) -gt 4; then
        ok=false
      fi
      local observer_status observer_archive
      observer_status="$(curl -fsS --max-time 5 \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"n42_consensusStatus","params":[]}' \
        http://127.0.0.1:30510)"
      observer_archive="$(curl -fsS --max-time 5 \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"n42_qmdbArchiveInfo","params":[]}' \
        http://127.0.0.1:30510)"
      if test "$(printf '%s' "$observer_status" | jq -r '.result.hasCommittedQc')" != false; then
        ok=false
      fi
      jq -nc \
        --arg at "$(date -u +%FT%TZ)" \
        --argjson ok "$ok" \
        --argjson minHeight "$min_height" \
        --argjson maxHeight "$max_height" \
        --arg blockHash "$expected_hash" \
        --arg stateRoot "$expected_root" \
        --arg receiptsRoot "$expected_receipts_root" \
        --argjson observerStatus "$(printf '%s' "$observer_status" | jq '.result')" \
        --argjson observerArchive "$(printf '%s' "$observer_archive" | jq '.result')" \
        '{
          at:$at,event:"p6_observer_soak_sample",ok:$ok,
          minHeight:$minHeight,maxHeight:$maxHeight,lag:($maxHeight-$minHeight),
          commonBlockHash:$blockHash,stateRoot:$stateRoot,receiptsRoot:$receiptsRoot,
          observerStatus:$observerStatus,
          observerReadOnly:($observerStatus.hasCommittedQc == false),
          observerArchive:$observerArchive
        }' >>"$evidence_file"
    else
      jq -nc --arg at "$(date -u +%FT%TZ)" \
        '{at:$at,event:"p6_observer_soak_sample",ok:false,error:"RPC unavailable"}' \
        >>"$evidence_file"
    fi
    rm -rf "$sample_dir"
    if test "$ok" != true; then
      return 1
    fi
    sleep "$interval_seconds"
  done
}

monitor_participant() {
  local duration_seconds="${1:?duration seconds required}"
  local interval_seconds="${2:-60}"
  local evidence_file="${3:-$runtime/evidence/p6-participant-24h-soak.jsonl}"
  local leader_evidence="${4:-$runtime/evidence/p6-participant-leaders.jsonl}"
  local deadline=$((SECONDS + duration_seconds))
  local last_scanned=-1
  local leader_counts=(0 0 0 0 0 0 0)
  mkdir -p "$(dirname "$evidence_file")" "$(dirname "$leader_evidence")"

  while test "$SECONDS" -lt "$deadline"; do
    local sample_dir
    sample_dir="$(mktemp -d)"
    local ok=true
    local min_height=-1
    local max_height=0
    local port
    for port in 31500 31501 31502 31503 31504 31505 30516; do
      if ! curl -fsS --max-time 5 \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
        "http://127.0.0.1:$port" >"$sample_dir/$port.json"; then
        ok=false
        break
      fi
      local height_hex height
      height_hex="$(jq -er '.result.number' "$sample_dir/$port.json")"
      height=$((height_hex))
      if test "$min_height" -lt 0 || test "$height" -lt "$min_height"; then
        min_height="$height"
      fi
      if test "$height" -gt "$max_height"; then
        max_height="$height"
      fi
    done

    local expected_hash=""
    local expected_root=""
    local expected_receipts_root=""
    if test "$ok" = true; then
      local common_hex
      common_hex="$(printf '0x%x' "$min_height")"
      for port in 31500 31501 31502 31503 31504 31505 30516; do
        local response hash root receipts_root
        response="$(curl -fsS --max-time 5 \
          -H 'content-type: application/json' \
          --data "$(jq -nc --arg height "$common_hex" \
            '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$height,false]}')" \
          "http://127.0.0.1:$port")"
        hash="$(printf '%s' "$response" | jq -er '.result.hash')"
        root="$(printf '%s' "$response" | jq -er '.result.stateRoot')"
        receipts_root="$(printf '%s' "$response" | jq -er '.result.receiptsRoot')"
        if test -z "$expected_hash"; then
          expected_hash="$hash"
          expected_root="$root"
          expected_receipts_root="$receipts_root"
        elif test "$hash" != "$expected_hash" ||
          test "$root" != "$expected_root" ||
          test "$receipts_root" != "$expected_receipts_root"; then
          ok=false
          break
        fi
      done
      if test $((max_height - min_height)) -gt 4; then
        ok=false
      fi
    fi

    local participant_status='null'
    local participant_equivocations='null'
    if test "$ok" = true; then
      participant_status="$(curl -fsS --max-time 5 \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"n42_consensusStatus","params":[]}' \
        http://127.0.0.1:30516 | jq '.result')"
      participant_equivocations="$(curl -fsS --max-time 5 \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"n42_equivocations","params":[]}' \
        http://127.0.0.1:30516 | jq '.result')"
      if test "$(printf '%s' "$participant_status" | jq -r '.hasCommittedQc')" != true ||
        test "$(printf '%s' "$participant_status" | jq -r '.validatorCount')" -ne 7 ||
        test "$(printf '%s' "$participant_equivocations" | jq -r '.total')" -ne 0 ||
        test "$(printf '%s' "$participant_equivocations" | jq -r '.evidence | length')" -ne 0; then
        ok=false
      fi
    fi

    if test "$ok" = true; then
      if test "$last_scanned" -lt 0; then
        last_scanned="$min_height"
      fi
      local block_number
      if test "$last_scanned" -lt "$min_height"; then
        for block_number in $(seq $((last_scanned + 1)) "$min_height"); do
          local response miner block_hash
          response="$(curl -fsS --max-time 5 \
            -H 'content-type: application/json' \
            --data "$(jq -nc --arg height "$(printf '0x%x' "$block_number")" \
              '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$height,false]}')" \
            http://127.0.0.1:30516)"
          miner="$(printf '%s' "$response" | jq -er '.result.miner | ascii_downcase')"
          block_hash="$(printf '%s' "$response" | jq -er '.result.hash')"
          local validator_index=-1
          local index
          for index in 0 1 2 3 4 5 6; do
            if test "$miner" = "${gov_addresses[$index]}"; then
              validator_index="$index"
              leader_counts[$index]=$((leader_counts[$index] + 1))
              break
            fi
          done
          if test "$validator_index" -lt 0; then
            ok=false
            break
          fi
          if test "$validator_index" -eq 6; then
            jq -nc \
              --arg at "$(date -u +%FT%TZ)" \
              --argjson height "$block_number" \
              --arg hash "$block_hash" \
              --arg miner "$miner" \
              '{at:$at,event:"p6_rust_leader_block",height:$height,hash:$hash,miner:$miner}' \
              >>"$leader_evidence"
          fi
        done
      fi
      last_scanned="$min_height"
    fi

    local counts_json
    counts_json="$(jq -nc \
      --argjson validator0 "${leader_counts[0]}" \
      --argjson validator1 "${leader_counts[1]}" \
      --argjson validator2 "${leader_counts[2]}" \
      --argjson validator3 "${leader_counts[3]}" \
      --argjson validator4 "${leader_counts[4]}" \
      --argjson validator5 "${leader_counts[5]}" \
      --argjson rustValidator6 "${leader_counts[6]}" \
      '{
        validator0:$validator0,validator1:$validator1,validator2:$validator2,
        validator3:$validator3,validator4:$validator4,validator5:$validator5,
        rustValidator6:$rustValidator6
      }')"
    jq -nc \
      --arg at "$(date -u +%FT%TZ)" \
      --argjson ok "$ok" \
      --argjson minHeight "$min_height" \
      --argjson maxHeight "$max_height" \
      --arg blockHash "$expected_hash" \
      --arg stateRoot "$expected_root" \
      --arg receiptsRoot "$expected_receipts_root" \
      --argjson participantStatus "$participant_status" \
      --argjson participantEquivocations "$participant_equivocations" \
      --argjson leaderCounts "$counts_json" \
      '{
        at:$at,event:"p6_participant_soak_sample",ok:$ok,
        minHeight:$minHeight,maxHeight:$maxHeight,lag:($maxHeight-$minHeight),
        commonBlockHash:$blockHash,stateRoot:$stateRoot,receiptsRoot:$receiptsRoot,
        participantStatus:$participantStatus,
        participantEquivocations:$participantEquivocations,
        leaderCounts:$leaderCounts
      }' >>"$evidence_file"
    rm -rf "$sample_dir"
    if test "$ok" != true; then
      return 1
    fi
    sleep "$interval_seconds"
  done

  local index
  for index in 0 1 2 3 4 5 6; do
    if test "${leader_counts[$index]}" -lt 2; then
      echo "validator $index did not lead twice during participant qualification" >&2
      return 1
    fi
  done
}

case "${1:-}" in
  start-gov) start_gov ;;
  start-gov-node) start_gov_node "${2:-}" ;;
  stop-gov-node) stop_one "$runtime/pids/gov${2:?Gov5 node index required}.pid" ;;
  restart-gov-node)
    stop_one "$runtime/pids/gov${2:?Gov5 node index required}.pid"
    start_gov_node "$2"
    ;;
  stop-gov) stop_gov ;;
  start-observer) start_observer ;;
  stop-observer) stop_one "$runtime/pids/observer.pid" ;;
  restart-observer)
    stop_one "$runtime/pids/observer.pid"
    start_observer
    ;;
  start-participant) start_participant ;;
  prepare-participant) prepare_participant ;;
  snapshot-replaced-validator) snapshot_replaced_validator_current ;;
  restart-participant)
    stop_one "$runtime/pids/participant.pid"
    start_participant
    ;;
  activate-replacement) activate_replacement ;;
  rollback-replacement) rollback_replacement ;;
  stop)
    stop_one "$runtime/pids/observer.pid"
    stop_one "$runtime/pids/participant.pid"
    stop_gov
    ;;
  status) status ;;
  monitor-observer) monitor_observer "${2:-}" "${3:-60}" "${4:-}" ;;
  monitor-participant) monitor_participant "${2:-}" "${3:-60}" "${4:-}" "${5:-}" ;;
  *)
    echo "usage: $0 {start-gov|start-gov-node N|stop-gov-node N|restart-gov-node N|stop-gov|start-observer|stop-observer|restart-observer|prepare-participant|snapshot-replaced-validator|start-participant|restart-participant|activate-replacement|rollback-replacement|stop|status|monitor-observer SECONDS [INTERVAL] [EVIDENCE]|monitor-participant SECONDS [INTERVAL] [EVIDENCE] [LEADER-EVIDENCE]}" >&2
    exit 2
    ;;
esac
