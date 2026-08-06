#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-11-production-qualification}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
genesis_hash="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
genesis_artifact_sha256="561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687"
rust_peer="12D3KooWBMkhLsvbQUWSva1tFiKNmWztd6oqvpaG1DGFqriT9DXi"
gov_peers=(
  "16Uiu2HAm9yzV5dzXsgu65UzkbtTnnDBTM79UZ76sjQ5pGnwqymFw"
  "16Uiu2HAmL4ab3Ad9uv3HSjCmgWCFqVGsSCV3RRSL3oBAPVob6fc6"
  "16Uiu2HAmE7rfc94zw4ihnUXa33nPWtq5neEKqUfFVETdNWxWeBWH"
  "16Uiu2HAmLH7DBmQWGYD4bEeSDMHdWYii5oC22JPCvEJThRiNynq1"
  "16Uiu2HAkveKXRpp42ohX9sJLi1Yi4JbS2em86FZj1WM2FPJCnfDm"
  "16Uiu2HAkw6rzcvsWjpcWpBoWnDuWuSg9NAGXaS7A3VsWU3mWQuEC"
)
gov_addresses=(
  "0xaa5f0ebd2c0b4a7c35aa9e7f0de765f7c0fffa51"
  "0xa5e99142c567fe398b483726927571b1040aadfd"
  "0x9464b8be1aa0e960ad4839298522eae0d5bbe71d"
  "0xa1de4e1c742e47bf805adf07538123b0ddda8dc5"
  "0xb9ef2bad950b795ed889de3aa0208365550cc86a"
  "0x853b2026deebc83fb79ac7d0c48efea595c22578"
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

start_gov_node() {
  node="${1:?gov node number required}"
  if test "$node" -lt 1 || test "$node" -gt 6; then
    echo "gov node number must be in 1..6: $node" >&2
    return 2
  fi
  gov_binary="${N42_GOV_BINARY:-$runtime/geth-live}"
  require_file "$gov_binary"
  require_file "$runtime/artifacts/genesis.json"
  mkdir -p "$runtime/logs" "$runtime/pids"
  index=$((node - 1))
  pid_file="$runtime/pids/gov${node}.pid"
  if pid_alive "$pid_file"; then
    return
  fi
  gov_datadir="$runtime/gov/node${node}"
  # Gov5 5.7.906's built-in `--chain private` genesis is not the interop
  # genesis. Never let an empty or partially copied directory silently create
  # that different chain. Operators must explicitly run `n42 init` with the
  # pinned artifact or copy a previously validated 5.7.905 data directory.
  test "$(shasum -a 256 "$runtime/artifacts/genesis.json" | awk '{print $1}')" = \
    "$genesis_artifact_sha256" || {
    echo "refusing Gov$node start: interop genesis artifact SHA-256 mismatch" >&2
    return 1
  }
  test -s "$gov_datadir/chaindata/mdbx.dat" || {
    echo "refusing Gov$node start: validated initialized chaindata is required" >&2
    return 1
  }
  require_file "$gov_datadir/keystore/bls_${gov_addresses[$index]#0x}.key"
  require_file "$gov_datadir/network-keys"
  require_file "$gov_datadir/network.json"
  require_file "$gov_datadir/epoch_schedule.json"
  args=(
    --chain private
    --profile n42
    --datadir "$gov_datadir"
    --port "$((30301 + index))"
    --http
    --http.port "$((28501 + index))"
    --mine
    --etherbase "${gov_addresses[$index]}"
    --block-interval-ms "${N42_GOV_BLOCK_INTERVAL_MS:-1000}"
    --verbosity "${N42_GOV_VERBOSITY:-3}"
    --p2p.no-discovery
    --p2p.min-sync-peers 0
    --p2p.max-peers 7
    --p2p.genesis-override "$genesis_hash"
    --p2p.peer "/ip4/127.0.0.1/udp/19780/quic-v1/p2p/$rust_peer"
  )
  for peer_index in 0 1 2 3 4 5; do
    if test "$peer_index" -ne "$index"; then
      args+=(
        --p2p.peer
        "/ip4/127.0.0.1/tcp/$((30301 + peer_index))/p2p/${gov_peers[$peer_index]}"
      )
    fi
  done
  if test "${N42_GOV_FOREGROUND:-0}" = "1"; then
    echo "$$" >"$pid_file"
    exec "$gov_binary" "${args[@]}" \
      </dev/null >>"$runtime/logs/gov${node}.log" 2>&1
  fi
  nohup "$gov_binary" "${args[@]}" \
    </dev/null >>"$runtime/logs/gov${node}.log" 2>&1 &
  echo "$!" >"$pid_file"
}

start_gov() {
  gov_count="${N42_GOV_COUNT:-5}"
  if test "$gov_count" -lt 1 || test "$gov_count" -gt 6; then
    echo "N42_GOV_COUNT must be in 1..6: $gov_count" >&2
    return 2
  fi
  for node in $(seq 1 "$gov_count"); do
    start_gov_node "$node"
  done
}

start_rust_validator() {
  name="$1"
  validator_node="$2"
  consensus_port="$3"
  reth_port="$4"
  http_port="$5"
  auth_port="$6"
  starhub_port="$7"
  rust_binary="${N42_NODE_BINARY:-$repo/target/release/n42-node}"
  consensus_config="${N42_CONSENSUS_CONFIG_FILE:-$runtime/artifacts/consensus-peer-bound.json}"
  require_file "$rust_binary"
  require_file "$consensus_config"
  require_file "$runtime/artifacts/bootstrap-bundle.json"
  mkdir -p "$runtime/logs" "$runtime/pids" "$runtime/$name"
  pid_file="$runtime/pids/$name.pid"
  if pid_alive "$pid_file"; then
    return
  fi
  key_dir="${N42_VALIDATOR_KEY_DIR:-}"
  if test -z "$key_dir"; then
    frozen_key_dir="$runtime/artifacts/validator-keys/node${validator_node}"
    if test -d "$frozen_key_dir"; then
      key_dir="$frozen_key_dir"
    else
      key_dir="/Users/jieliu/Documents/n42/live-interop-20260721/runtime-02-generated/node${validator_node}"
    fi
  fi
  validator_key_files=("$key_dir"/keystore/*.key)
  if test "${#validator_key_files[@]}" -ne 1 ||
    ! test -f "${validator_key_files[0]}"; then
    echo "expected exactly one validator key file in $key_dir/keystore" >&2
    return 2
  fi
  require_file "$key_dir/network-keys"
  if test -n "${N42_EXPECTED_VALIDATOR_KEY_SHA256:-}"; then
    test "$(shasum -a 256 "${validator_key_files[0]}" | awk '{print $1}')" = \
      "$N42_EXPECTED_VALIDATOR_KEY_SHA256" || {
      echo "validator key SHA-256 mismatch: ${validator_key_files[0]}" >&2
      return 1
    }
  fi
  if test -n "${N42_EXPECTED_P2P_KEY_SHA256:-}"; then
    test "$(shasum -a 256 "$key_dir/network-keys" | awk '{print $1}')" = \
      "$N42_EXPECTED_P2P_KEY_SHA256" || {
      echo "P2P key SHA-256 mismatch: $key_dir/network-keys" >&2
      return 1
    }
  fi
  trusted_peers=""
  gov_count=6
  if test "$validator_node" -eq 6; then
    gov_count=5
  fi
  for peer_index in $(seq 0 $((gov_count - 1))); do
    peer="/ip4/127.0.0.1/tcp/$((30301 + peer_index))/p2p/${gov_peers[$peer_index]}"
    trusted_peers="${trusted_peers:+$trusted_peers,}$peer"
  done
  if test "$validator_node" -ne 0; then
    trusted_peers="$trusted_peers,/ip4/127.0.0.1/udp/19780/quic-v1/p2p/$rust_peer"
  fi
  rust_env=(
    "N42_CONSENSUS_CONFIG=$consensus_config"
    "N42_VALIDATOR_KEY=@${validator_key_files[0]}"
    "N42_P2P_KEY=@$key_dir/network-keys"
    "N42_DATA_DIR=$runtime/$name/consensus"
    "N42_GOV5_H2_PARTICIPANT=1"
    "N42_GOV5_HEADER_PROFILE=1"
    "N42_INTEROP_GENESIS_HASH=$genesis_hash"
    "N42_GOV5_BOOTSTRAP_BUNDLE=$runtime/artifacts/bootstrap-bundle.json"
    "N42_QMDB_REPLAY_DEPTH=${N42_QMDB_REPLAY_DEPTH:-1048576}"
    "N42_GOV5_CATCHUP_BUFFER_BLOCKS=${N42_GOV5_CATCHUP_BUFFER_BLOCKS:-2048}"
    "N42_CONSENSUS_PORT=$consensus_port"
    "N42_STARHUB_PORT=$starhub_port"
    "N42_NO_AUTO_CONNECT=1"
    "N42_TRUSTED_PEERS=$trusted_peers"
    "N42_ENABLE_MDNS=0"
    "N42_ENABLE_DHT=0"
    "N42_ENABLE_HTTP_RPC=1"
  )
  rust_args=(
    node
    --chain "$runtime/artifacts/genesis.json"
    --datadir "$runtime/$name/reth"
    --disable-discovery
    --port "$reth_port"
    --http
    --http.addr 127.0.0.1
    --http.port "$http_port"
    --authrpc.port "$auth_port"
    --ipcdisable
    --log.file.max-files 0
    --color never
  )
  if test "${N42_RUST_FOREGROUND:-0}" = "1"; then
    echo "$$" >"$pid_file"
    exec env "${rust_env[@]}" "$rust_binary" "${rust_args[@]}" \
      </dev/null >>"$runtime/logs/$name.log" 2>&1
  fi
  nohup env "${rust_env[@]}" "$rust_binary" "${rust_args[@]}" \
    </dev/null >>"$runtime/logs/$name.log" 2>&1 &
  echo "$!" >"$pid_file"
}

start_rust() {
  start_rust_validator rust 0 19780 31303 29545 29551 9443
}

start_rust2() {
  # Validator 6 keeps its exact BLS and secp256k1 PeerId while the client
  # implementation changes. The Gov5 process is stopped cleanly and its
  # database remains untouched for immediate rollback.
  stop_one "$runtime/pids/gov6.pid"
  start_rust_validator rust2 6 30306 31306 29546 29552 9444
}

stop_one() {
  pid_file="$1"
  if pid_alive "$pid_file"; then
    kill "$(<"$pid_file")"
    for _ in $(seq 1 30); do
      if ! pid_alive "$pid_file"; then
        break
      fi
      sleep 1
    done
  fi
  if pid_alive "$pid_file"; then
    kill -KILL "$(<"$pid_file")"
  fi
  rm -f "$pid_file"
}

stop_all() {
  test -d "$runtime/pids" || return
  stop_one "$runtime/pids/rust.pid"
  stop_one "$runtime/pids/rust2.pid"
  for node in 1 2 3 4 5 6; do
    stop_one "$runtime/pids/gov${node}.pid"
  done
}

status() {
  for name in gov1 gov2 gov3 gov4 gov5 gov6 rust rust2; do
    pid_file="$runtime/pids/$name.pid"
    if pid_alive "$pid_file"; then
      echo "$name running pid=$(<"$pid_file")"
    else
      echo "$name stopped"
    fi
  done
  for port in 28501 28502 28503 28504 28505 28506 29545 29546; do
    result="$(curl -fsS --max-time 2 \
      -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
      "http://127.0.0.1:$port" 2>/dev/null || true)"
    if test -n "$result"; then
      echo "rpc:$port $(printf '%s' "$result" | jq -r '.result | [.number,.hash,.stateRoot] | @tsv')"
    fi
  done
}

collect_latest_head_snapshot() {
  local output_dir="${1:?snapshot output directory required}"
  shift
  local snapshot_ports=("$@")
  local snapshot_pids=()
  local snapshot_port pid index number_hex number
  local snapshot_failed_port=0
  local snapshot_min_height=-1
  local snapshot_max_height=0

  # Start every latest-head request before waiting for any response. Sequential
  # reads can straddle a rapid catch-up burst and report an artificial lag even
  # though all clients commit the same canonical blocks within milliseconds.
  for snapshot_port in "${snapshot_ports[@]}"; do
    curl -fsS --max-time 3 \
      -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
      "http://127.0.0.1:$snapshot_port" \
      >"$output_dir/latest-$snapshot_port.json" &
    snapshot_pids+=("$!")
  done

  for index in "${!snapshot_pids[@]}"; do
    pid="${snapshot_pids[$index]}"
    snapshot_port="${snapshot_ports[$index]}"
    if ! wait "$pid"; then
      if test "$snapshot_failed_port" -eq 0; then
        snapshot_failed_port="$snapshot_port"
      fi
    fi
  done

  if test "$snapshot_failed_port" -eq 0; then
    for snapshot_port in "${snapshot_ports[@]}"; do
      if ! number_hex="$(jq -er '.result.number' \
        "$output_dir/latest-$snapshot_port.json")"; then
        snapshot_failed_port="$snapshot_port"
        break
      fi
      number=$((number_hex))
      if test "$snapshot_min_height" -lt 0 || \
        test "$number" -lt "$snapshot_min_height"; then
        snapshot_min_height="$number"
      fi
      if test "$number" -gt "$snapshot_max_height"; then
        snapshot_max_height="$number"
      fi
    done
  fi

  printf '%s\t%s\t%s\n' \
    "$snapshot_min_height" "$snapshot_max_height" "$snapshot_failed_port"
}

monitor_heads() {
  duration_seconds="${1:?duration seconds required}"
  interval_seconds="${2:-10}"
  evidence_file="${3:-$runtime/evidence/head-monitor.jsonl}"
  max_lag="${N42_QUAL_MAX_LAG:-16}"
  lag_confirmation_attempts="${N42_QUAL_LAG_CONFIRMATION_ATTEMPTS:-3}"
  lag_confirmation_delay="${N42_QUAL_LAG_CONFIRMATION_DELAY_SECONDS:-0.2}"
  require_zero_tx="${N42_QUAL_REQUIRE_ZERO_TX:-0}"
  zero_tx_verified_to=-1
  read -r -a ports <<<"${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545 29546}"
  rust_history_port="${N42_QUAL_RUST_PORT:-29546}"
  mkdir -p "$(dirname "$evidence_file")"
  [[ "$lag_confirmation_attempts" =~ ^[1-9][0-9]*$ ]]
  [[ "$lag_confirmation_delay" =~ ^[0-9]+([.][0-9]+)?$ ]]
  first_sample_seconds="$SECONDS"

  while true; do
    sample_dir="$(mktemp -d)"
    sample_failed=0
    failed_port=0
    failed_phase=""
    latest_snapshot_attempt=1
    while true; do
      read -r min_height max_height latest_failed_port < <(
        collect_latest_head_snapshot "$sample_dir" "${ports[@]}"
      )
      if test "$latest_failed_port" -ne 0; then
        sample_failed=1
        failed_port="$latest_failed_port"
        failed_phase="latest"
        break
      fi
      lag=$((max_height - min_height))
      if test "$lag" -le "$max_lag" || \
        test "$latest_snapshot_attempt" -ge "$lag_confirmation_attempts"; then
        break
      fi
      sleep "$lag_confirmation_delay"
      latest_snapshot_attempt=$((latest_snapshot_attempt + 1))
    done

    if test "$sample_failed" -ne 0; then
      jq -nc --arg at "$(date -u +%FT%TZ)" \
        --argjson port "$failed_port" --arg phase "$failed_phase" \
        '{at:$at,ok:false,error:"rpc unavailable",failedPort:$port,
          failedPhase:$phase}' >>"$evidence_file"
      rm -rf "$sample_dir"
      return 1
    fi

    common_hex="$(printf '0x%x' "$min_height")"
    expected=""
    for port in "${ports[@]}"; do
      request="$(jq -nc --arg height "$common_hex" \
        '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$height,false]}')"
      if ! curl -fsS --max-time 3 \
        -H 'content-type: application/json' \
        --data "$request" "http://127.0.0.1:$port" >"$sample_dir/common-$port.json"; then
        sample_failed=1
        failed_port="$port"
        failed_phase="common-height"
        break
      fi
      identity="$(jq -er '.result | [.hash,.stateRoot,.receiptsRoot] | join(":")' \
        "$sample_dir/common-$port.json")"
      if test -z "$expected"; then
        expected="$identity"
      elif test "$identity" != "$expected"; then
        sample_failed=1
        break
      fi
    done

    lag=$((max_height - min_height))
    ok=true
    error=""
    if test "$sample_failed" -ne 0; then
      ok=false
      if test "$failed_port" -ne 0; then
        error="rpc unavailable at common height"
      else
        error="canonical divergence at common height"
      fi
    elif test "$lag" -gt "$max_lag"; then
      ok=false
      error="execution lag exceeded bound"
    fi

    zero_tx_verified_from=-1
    if test "$ok" = true && test "$require_zero_tx" = 1; then
      if test "$zero_tx_verified_to" -lt 0; then
        zero_tx_verified_to="$min_height"
      else
        next_zero_tx_height=$((zero_tx_verified_to + 1))
        if test "$next_zero_tx_height" -le "$min_height"; then
          zero_tx_verified_from="$next_zero_tx_height"
          for height in $(seq "$zero_tx_verified_from" "$min_height"); do
          height_hex="$(printf '0x%x' "$height")"
          request="$(jq -nc --arg height "$height_hex" \
            '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$height,false]}')"
          gov_block="$(curl -fsS --max-time 3 \
            -H 'content-type: application/json' --data "$request" \
            "http://127.0.0.1:28501" 2>/dev/null || true)"
          rust_block="$(curl -fsS --max-time 3 \
            -H 'content-type: application/json' --data "$request" \
            "http://127.0.0.1:$rust_history_port" 2>/dev/null || true)"
          gov_identity="$(printf '%s' "$gov_block" | jq -er \
            '.result | select((.transactions | length) == 0) | [.hash,.stateRoot,.receiptsRoot] | join(":")' \
            2>/dev/null || true)"
          rust_identity="$(printf '%s' "$rust_block" | jq -er \
            '.result | select((.transactions | length) == 0) | [.hash,.stateRoot,.receiptsRoot] | join(":")' \
            2>/dev/null || true)"
          if test -z "$gov_identity" || test "$gov_identity" != "$rust_identity"; then
            ok=false
            error="non-empty transaction block or Gov/Rust historical divergence during zero-tx soak"
            break
          fi
            zero_tx_verified_to="$height"
          done
        fi
      fi
    fi

    jq -nc \
      --arg at "$(date -u +%FT%TZ)" \
      --argjson ok "$ok" \
      --arg error "$error" \
      --argjson common_height "$min_height" \
      --argjson maximum_height "$max_height" \
      --argjson lag "$lag" \
      --argjson latest_snapshot_attempts "$latest_snapshot_attempt" \
      --argjson zero_tx_required "$require_zero_tx" \
      --argjson zero_tx_verified_from "$zero_tx_verified_from" \
      --argjson zero_tx_verified_to "$zero_tx_verified_to" \
      --argjson failed_port "$failed_port" \
      --arg failed_phase "$failed_phase" \
      --arg identity "$expected" \
      '{at:$at,ok:$ok,error:$error,commonHeight:$common_height,
        maximumHeight:$maximum_height,lag:$lag,identity:$identity,
        latestSnapshotAttempts:$latest_snapshot_attempts,
        latestSnapshotConcurrent:true,
        zeroTxRequired:$zero_tx_required,zeroTxVerifiedFrom:$zero_tx_verified_from,
        zeroTxVerifiedTo:$zero_tx_verified_to,
        failedPort:(if $failed_port == 0 then null else $failed_port end),
        failedPhase:(if $failed_phase == "" then null else $failed_phase end)}' \
      >>"$evidence_file"
    rm -rf "$sample_dir"
    if test "$ok" != true; then
      return 1
    fi
    if test $((SECONDS - first_sample_seconds)) -ge "$duration_seconds"; then
      break
    fi
    sleep "$interval_seconds"
  done
}

audit_soak() {
  local evidence_file="${1:?evidence file required}"
  local minimum_elapsed="${2:?minimum first-to-last sample seconds required}"
  local maximum_gap="${3:-120}"
  local maximum_lag="${4:-6}"
  local require_zero_tx="${5:-0}"
  require_file "$evidence_file"
  jq -e -s \
    --argjson minimum_elapsed "$minimum_elapsed" \
    --argjson maximum_gap "$maximum_gap" \
    --argjson maximum_lag "$maximum_lag" \
    --argjson require_zero_tx "$require_zero_tx" '
    . as $samples |
    ($samples | length >= 2) and
    ($samples | all(.[];
      .ok == true and
      (.error // "") == "" and
      (.commonHeight | type) == "number" and
      (.maximumHeight | type) == "number" and
      .maximumHeight >= .commonHeight and
      .lag == (.maximumHeight - .commonHeight) and
      .lag <= $maximum_lag and
      (.identity | test("^0x[0-9a-f]{64}:0x[0-9a-f]{64}:0x[0-9a-f]{64}$")))) and
    ([$samples[].at | fromdateiso8601] as $times |
      ($times[-1] - $times[0]) >= $minimum_elapsed and
      ([range(1; $times | length) as $i |
        ($times[$i] - $times[$i - 1]) > 0 and
        ($times[$i] - $times[$i - 1]) <= $maximum_gap] | all)) and
    ($samples[-1].commonHeight > $samples[0].commonHeight) and
    ($require_zero_tx == 0 or
      (($samples[0].zeroTxRequired == 1) and
       ($samples[0].zeroTxVerifiedFrom == -1) and
       ($samples[0].zeroTxVerifiedTo == $samples[0].commonHeight) and
       ([range(1; $samples | length) as $i |
         $samples[$i].zeroTxRequired == 1 and
         $samples[$i].zeroTxVerifiedTo == $samples[$i].commonHeight and
         (if $samples[$i].commonHeight > $samples[$i - 1].commonHeight then
            $samples[$i].zeroTxVerifiedFrom == ($samples[$i - 1].zeroTxVerifiedTo + 1)
          else
            $samples[$i].zeroTxVerifiedFrom == -1 and
            $samples[$i].zeroTxVerifiedTo == $samples[$i - 1].zeroTxVerifiedTo
          end)] | all)))
  ' "$evidence_file" >/dev/null

  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg evidence "$evidence_file" \
    --arg evidence_sha256 "$(sha256sum "$evidence_file" | awk '{print $1}')" \
    --argjson minimum_elapsed "$minimum_elapsed" \
    --argjson maximum_gap "$maximum_gap" \
    --argjson maximum_lag "$maximum_lag" \
    --argjson require_zero_tx "$require_zero_tx" \
    --slurpfile samples "$evidence_file" '
    [$samples[].at | fromdateiso8601] as $times |
    {
      at:$at,event:"mixed_client_soak_audit",status:"PASS",
      evidence:$evidence,evidenceSha256:$evidence_sha256,
      samples:($samples|length),firstAt:$samples[0].at,lastAt:$samples[-1].at,
      elapsedSeconds:($times[-1]-$times[0]),
      maximumSampleGapSeconds:([range(1;$times|length) as $i |
        $times[$i]-$times[$i-1]]|max),
      startHeight:$samples[0].commonHeight,endHeight:$samples[-1].commonHeight,
      blockGrowth:($samples[-1].commonHeight-$samples[0].commonHeight),
      maximumLag:([$samples[].lag]|max),zeroTransactionRequired:($require_zero_tx==1),
      thresholds:{minimumElapsedSeconds:$minimum_elapsed,
        maximumSampleGapSeconds:$maximum_gap,maximumLag:$maximum_lag}
    }'
}

audit_rust_leaders() {
  local start_height="${1:?first expected Rust-authored height required}"
  local end_height="${2:-}"
  local evidence_file="${3:-}"
  local rust_port="${N42_QUAL_RUST_PORT:-29545}"
  local rust_miner="${N42_QUAL_RUST_MINER:-0x81d4c1f92ddb837cb46f82280d9b491b101fa582}"
  local leader_stride="${N42_QUAL_RUST_LEADER_STRIDE:-6}"
  local -a ports
  read -r -a ports <<<"${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"

  local minimum_head=-1 port head_hex head
  for port in "${ports[@]}"; do
    head_hex="$(rpc_request "http://127.0.0.1:$port" eth_blockNumber '[]' |
      jq -er 'select(.error == null) | .result')"
    head=$((head_hex))
    if test "$minimum_head" -lt 0 || test "$head" -lt "$minimum_head"; then
      minimum_head="$head"
    fi
  done
  if test -z "$end_height"; then
    end_height="$minimum_head"
  fi
  if test "$end_height" -gt "$minimum_head"; then
    echo "Rust leader audit end height $end_height exceeds common head $minimum_head" >&2
    return 1
  fi
  if test "$end_height" -lt "$start_height" || test "$leader_stride" -lt 1; then
    echo "invalid Rust leader audit range or stride" >&2
    return 2
  fi

  local audit_dir
  audit_dir="$(mktemp -d)"
  trap 'rm -rf "$audit_dir"' RETURN

  local range_file="$audit_dir/range.jsonl"
  local chunk_start chunk_end height request response
  chunk_start="$start_height"
  while test "$chunk_start" -le "$end_height"; do
    chunk_end=$((chunk_start + 499))
    if test "$chunk_end" -gt "$end_height"; then
      chunk_end="$end_height"
    fi
    request="$(for height in $(seq "$chunk_start" "$chunk_end"); do
      jq -nc --argjson id "$height" --arg block "$(printf '0x%x' "$height")" \
        '{jsonrpc:"2.0",id:$id,method:"eth_getBlockByNumber",params:[$block,false]}'
    done | jq -s '.')"
    response="$(curl -fsS --max-time 30 -H 'content-type: application/json' \
      --data "$request" "http://127.0.0.1:$rust_port")"
    printf '%s' "$response" | jq -ec \
      'sort_by(.id)[] | select(.error == null and .result != null) |
       {height:.id,number:.result.number,hash:.result.hash,
        parentHash:.result.parentHash,miner:(.result.miner|ascii_downcase),
        difficulty:.result.difficulty}' >>"$range_file"
    chunk_start=$((chunk_end + 1))
  done

  local total_blocks expected_leaders
  total_blocks=$((end_height - start_height + 1))
  expected_leaders=$(((end_height - start_height) / leader_stride + 1))
  jq -e -s \
    --arg miner "$rust_miner" \
    --argjson start "$start_height" \
    --argjson end "$end_height" \
    --argjson stride "$leader_stride" \
    --argjson total "$total_blocks" \
    --argjson leaders "$expected_leaders" '
    length == $total and
    (.[0].height == $start) and (.[-1].height == $end) and
    (all(.[];
      (.hash | test("^0x[0-9a-f]{64}$")) and
      (.parentHash | test("^0x[0-9a-f]{64}$")) and
      (if ((.height - $start) % $stride) == 0 then
         .miner == $miner and .difficulty == "0x0"
       else
         .miner != $miner
       end))) and
    ([.[] | select(.miner == $miner)] | length == $leaders) and
    ([range(1; length) as $i |
      .[$i].height == (.[$i - 1].height + 1) and
      .[$i].parentHash == .[$i - 1].hash] | all)
  ' "$range_file" >/dev/null

  local reference_file="$audit_dir/reference.jsonl"
  jq -c --arg miner "$rust_miner" \
    'select(.miner == $miner) | {height,hash,miner,difficulty}' \
    "$range_file" >"$reference_file"

  local endpoint_file leader_index=0
  for port in "${ports[@]}"; do
    endpoint_file="$audit_dir/endpoint-$port.jsonl"
    chunk_start="$start_height"
    while test "$chunk_start" -le "$end_height"; do
      request="$(for ((height=chunk_start, leader_index=0;
                       height<=end_height && leader_index<500;
                       height+=leader_stride, leader_index++)); do
        jq -nc --argjson id "$height" --arg block "$(printf '0x%x' "$height")" \
          '{jsonrpc:"2.0",id:$id,method:"eth_getBlockByNumber",params:[$block,false]}'
      done | jq -s '.')"
      response="$(curl -fsS --max-time 30 -H 'content-type: application/json' \
        --data "$request" "http://127.0.0.1:$port")"
      printf '%s' "$response" | jq -ec \
        'sort_by(.id)[] | select(.error == null and .result != null) |
         {height:.id,hash:.result.hash,miner:(.result.miner|ascii_downcase),
          difficulty:.result.difficulty}' >>"$endpoint_file"
      chunk_start=$((chunk_start + leader_stride * 500))
    done
    if ! cmp -s "$reference_file" "$endpoint_file"; then
      echo "Rust-authored canonical blocks differ at RPC port $port" >&2
      return 1
    fi
  done

  local leader_log="${N42_QUAL_RUST_LOG:-}"
  local expected_view_stride="${N42_QUAL_RUST_VIEW_STRIDE:-7}"
  local log_summary=null
  if test -n "$leader_log"; then
    require_file "$leader_log"
    local parsed_log="$audit_dir/leader-log.jsonl"
    local matched_log="$audit_dir/matched-leader-log.jsonl"
    rg 'block committed' "$leader_log" | jq -Rc '
      capture("block committed. view=(?<view>[0-9]+) block_hash=(?<hash>0x[0-9a-f]{64}).*proposal=@(?<proposal>[0-9]+)ms R1_collect=(?<r1>[0-9]+)ms R2_collect=(?<r2>[0-9]+)ms total=(?<total>[0-9]+)ms votes=(?<votes>[0-9]+[+][0-9]+)") |
      {view:(.view|tonumber),hash,proposalMs:(.proposal|tonumber),
       r1Ms:(.r1|tonumber),r2Ms:(.r2|tonumber),
       totalMs:(.total|tonumber),votes}' >"$parsed_log"
    jq -c --slurpfile references "$reference_file" '
      . as $entry |
      ([$references[].hash] | index($entry.hash)) as $index |
      select($index != null) | $entry
    ' "$parsed_log" >"$matched_log"
    jq -s 'map(.hash)' "$reference_file" >"$audit_dir/reference-hashes.json"
    jq -s 'map(.hash)' "$matched_log" >"$audit_dir/log-hashes.json"
    if ! cmp -s "$audit_dir/reference-hashes.json" "$audit_dir/log-hashes.json"; then
      echo "Rust leader commit log does not match canonical Rust block order" >&2
      return 1
    fi
    jq -e -s --argjson expected "$expected_leaders" \
      --argjson view_stride "$expected_view_stride" '
      length == $expected and
      all(.[]; .votes == "5+5") and
      ([range(1; length) as $i |
        .[$i].view - .[$i - 1].view == $view_stride] | all)
    ' "$matched_log" >/dev/null
    log_summary="$(jq -nc --slurpfile logs "$matched_log" \
      --argjson view_stride "$expected_view_stride" '
      {matchedCommits:($logs|length),allVotesFivePlusFive:true,
       expectedViewStride:$view_stride,viewStrideExact:true,hashOrderExact:true,
       firstView:$logs[0].view,lastView:$logs[-1].view,
       latencyMs:{proposalMinimum:([$logs[].proposalMs]|min),
         proposalMaximum:([$logs[].proposalMs]|max),
         commitMinimum:([$logs[].totalMs]|min),
         commitMaximum:([$logs[].totalMs]|max),
         commitAverage:(([$logs[].totalMs]|add)/($logs|length))}}')"
  fi

  local first_hash last_hash summary
  first_hash="$(jq -sr '.[0].hash' "$reference_file")"
  last_hash="$(jq -sr '.[-1].hash' "$reference_file")"
  summary="$(jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg miner "$rust_miner" \
    --arg first_hash "$first_hash" \
    --arg last_hash "$last_hash" \
    --argjson start "$start_height" \
    --argjson end "$end_height" \
    --argjson stride "$leader_stride" \
    --argjson blocks "$total_blocks" \
    --argjson leaders "$expected_leaders" \
    --argjson leader_log "$log_summary" \
    --argjson ports "$(printf '%s\n' "${ports[@]}" | jq -R 'tonumber' | jq -s '.')" '
    {at:$at,event:"rust_leader_canonical_audit",status:"PASS",miner:$miner,
     startHeight:$start,endHeight:$end,blocksScanned:$blocks,leaderStride:$stride,
     rustAuthoredBlocks:$leaders,firstRustHash:$first_hash,lastRustHash:$last_hash,
     ports:$ports,parentChainContinuous:true,expectedLeaderSlotsExact:true,
     allConfiguredEndpointsExact:true,leaderCommitLog:$leader_log}')"
  if test -n "$evidence_file"; then
    mkdir -p "$(dirname "$evidence_file")"
    printf '%s\n' "$summary" >>"$evidence_file"
  fi
  printf '%s\n' "$summary"
}

record_rust_resources() {
  local evidence_file="${1:-}"
  local pid_file="${N42_QUAL_RUST_PID_FILE:-$runtime/pids/rust.pid}"
  require_file "$pid_file"
  local rust_pid
  rust_pid="$(<"$pid_file")"
  kill -0 "$rust_pid" 2>/dev/null || {
    echo "Rust process is not alive: $rust_pid" >&2
    return 1
  }

  local rss_kib vsz_kib cpu_percent process_elapsed thread_count fd_count
  local reth_kib consensus_kib log_bytes wal_file wal_bytes head_hex head
  rss_kib="$(ps -p "$rust_pid" -o rss= | tr -d ' ')"
  vsz_kib="$(ps -p "$rust_pid" -o vsz= | tr -d ' ')"
  cpu_percent="$(ps -p "$rust_pid" -o %cpu= | tr -d ' ')"
  process_elapsed="$(ps -p "$rust_pid" -o etime= | tr -d ' ')"
  thread_count="$(ps -M -p "$rust_pid" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')"
  fd_count="$(lsof -p "$rust_pid" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')"
  reth_kib="$(du -sk "$runtime/rust/reth" | awk '{print $1}')"
  consensus_kib="$(du -sk "$runtime/rust/consensus" | awk '{print $1}')"
  log_bytes="$(stat -f '%z' "$runtime/logs/rust.log")"
  wal_file="$(find "$runtime/rust/consensus" -maxdepth 1 -type f -name '*wal*' -print |
    head -n 1)"
  wal_bytes=0
  if test -n "$wal_file"; then
    wal_bytes="$(stat -f '%z' "$wal_file")"
  fi
  head_hex="$(rpc_request "http://127.0.0.1:${N42_QUAL_RUST_PORT:-29545}" \
    eth_blockNumber '[]' | jq -er 'select(.error == null) | .result')"
  head=$((head_hex))

  local snapshot
  snapshot="$(jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --argjson pid "$rust_pid" \
    --arg process_elapsed "$process_elapsed" \
    --argjson head "$head" \
    --argjson rss_kib "$rss_kib" \
    --argjson vsz_kib "$vsz_kib" \
    --argjson cpu_percent "$cpu_percent" \
    --argjson threads "$thread_count" \
    --argjson file_descriptors "$fd_count" \
    --argjson reth_kib "$reth_kib" \
    --argjson consensus_kib "$consensus_kib" \
    --argjson log_bytes "$log_bytes" \
    --arg wal_file "$wal_file" \
    --argjson wal_bytes "$wal_bytes" '
    {at:$at,event:"rust_resource_snapshot",pid:$pid,
     processElapsed:$process_elapsed,head:$head,rssKiB:$rss_kib,vszKiB:$vsz_kib,
     cpuPercent:$cpu_percent,threads:$threads,fileDescriptors:$file_descriptors,
     rethDataKiB:$reth_kib,consensusDataKiB:$consensus_kib,logBytes:$log_bytes,
     qmdbWalFile:$wal_file,qmdbWalBytes:$wal_bytes}')"
  if test -n "$evidence_file"; then
    mkdir -p "$(dirname "$evidence_file")"
    printf '%s\n' "$snapshot" >>"$evidence_file"
  fi
  printf '%s\n' "$snapshot"
}

audit_timeout_recovery() {
  local leader_log="${1:?Rust consensus log required}"
  local evidence_file="${2:-}"
  local rust_port="${N42_QUAL_RUST_PORT:-29545}"
  require_file "$leader_log"

  local audit_dir
  audit_dir="$(mktemp -d)"
  trap 'rm -rf "$audit_dir"' RETURN

  local timed_out="$audit_dir/timed-out.jsonl"
  local pacemaker="$audit_dir/pacemaker.jsonl"
  local rust_commits="$audit_dir/rust-commits.jsonl"
  rg ' WARN view timed out view=[0-9]+' "$leader_log" | jq -Rc '
    capture("^(?<at>[^ ]+).*view timed out view=(?<view>[0-9]+)$") |
    {at,view:(.view|tonumber)}' >"$timed_out"
  rg ' WARN pacemaker timeout, initiating view change view=[0-9]+' \
    "$leader_log" | jq -Rc '
    capture("^(?<at>[^ ]+).*view change view=(?<view>[0-9]+)$") |
    {at,view:(.view|tonumber)}' >"$pacemaker"
  rg ' INFO (.*: )?block committed! view=[0-9]+' "$leader_log" | jq -Rc '
    capture("^(?<at>[^ ]+).*block committed! view=(?<view>[0-9]+) block_hash=(?<hash>0x[0-9a-f]{64}).*votes=(?<votes>[0-9]+[+][0-9]+)$") |
    {at,view:(.view|tonumber),hash,votes}' >"$rust_commits"

  local committed_view
  committed_view="$(rpc_request "http://127.0.0.1:$rust_port" \
    n42_consensusStatus '[]' | jq -er '.result.latestCommittedView')"

  jq -e -n \
    --argjson committed_view "$committed_view" \
    --slurpfile timed_out "$timed_out" \
    --slurpfile pacemaker "$pacemaker" \
    --slurpfile rust_commits "$rust_commits" '
    ($timed_out | sort_by(.view)) as $timeouts |
    ($pacemaker | sort_by(.view)) as $pacemakers |
    ($rust_commits | sort_by(.view)) as $commits |
    ($timeouts | map(select(.view < $committed_view))) as $eligible |
    ($timeouts | map(select(.view >= $committed_view))) as $pending |
    ($timeouts | length >= 1) and
    (($timeouts | map(.view) | unique | length) == ($timeouts | length)) and
    (($pacemakers | map(.view) | unique | length) == ($pacemakers | length)) and
    (($commits | map(.view) | unique | length) == ($commits | length)) and
    (($timeouts | map(.view)) == ($pacemakers | map(.view))) and
    ([range(1; $timeouts | length) as $i |
      $timeouts[$i].view - $timeouts[$i - 1].view == 7] | all) and
    ($eligible | length >= 1) and ($pending | length <= 1) and
    ([$eligible[] as $timeout |
      any($commits[];
        .view == ($timeout.view + 1) and .votes == "5+5" and
        (.hash | test("^0x[0-9a-f]{64}$")))] | all)
  ' >/dev/null

  local summary
  summary="$(jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg log "$leader_log" \
    --arg log_sha256 "$(shasum -a 256 "$leader_log" | awk '{print $1}')" \
    --argjson committed_view "$committed_view" \
    --slurpfile timed_out "$timed_out" \
    --slurpfile rust_commits "$rust_commits" '
    ($timed_out | sort_by(.view)) as $timeouts |
    ($rust_commits | sort_by(.view)) as $commits |
    ($timeouts | map(select(.view < $committed_view))) as $eligible |
    ($timeouts | map(select(.view >= $committed_view))) as $pending |
    {at:$at,event:"timeout_recovery_audit",status:"PASS",
      log:$log,logSha256:$log_sha256,latestCommittedView:$committed_view,
      timeoutEvents:($timeouts|length),completedTimeouts:($eligible|length),
      pendingTimeouts:($pending|length),
      firstTimeoutView:$timeouts[0].view,lastTimeoutView:$timeouts[-1].view,
      timeoutViewStride:7,timeoutAndPacemakerSetsExact:true,
      everyCompletedTimeoutRecoveredAtNextView:true,
      recoveredByRustVotesFivePlusFive:true,
      firstRecoveryView:($eligible[0].view+1),
      lastRecoveryView:($eligible[-1].view+1),
      rustLeaderCommitsObserved:($commits|length)}')"
  if test -n "$evidence_file"; then
    mkdir -p "$(dirname "$evidence_file")"
    printf '%s\n' "$summary" >>"$evidence_file"
  fi
  printf '%s\n' "$summary"
}

audit_runtime_logs() {
  local rust_log="${1:?Rust log required}"
  local evidence_file="${2:-}"
  require_file "$rust_log"

  local audit_dir unknown critical
  audit_dir="$(mktemp -d)"
  trap 'rm -rf "$audit_dir"' RETURN
  unknown="$audit_dir/unknown-warnings.log"
  critical="$audit_dir/critical-signals.log"

  local total timeout pacemaker eviction commits duplicate_vote duplicate_commit
  local payload_retry unsupported_state_sync missing_tx_forward_leader
  total="$(rg -c ' WARN ' "$rust_log" || echo 0)"
  timeout="$(rg -c ' WARN view timed out view=' "$rust_log" || echo 0)"
  pacemaker="$(rg -c ' WARN pacemaker timeout, initiating view change view=' \
    "$rust_log" || echo 0)"
  eviction="$(rg -c ' WARN .*evicted rejected compact execution output hash=' \
    "$rust_log" || echo 0)"
  commits="$(rg -c ' INFO (.*: )?block committed! view=' "$rust_log" || echo 0)"
  duplicate_vote="$(rg -c \
    ' WARN .*suppressed duplicate vote \(already voted in this view\)' \
    "$rust_log" || echo 0)"
  duplicate_commit="$(rg -c \
    ' WARN .*suppressed duplicate commit vote \(already commit-voted in this view\)' \
    "$rust_log" || echo 0)"
  payload_retry="$(rg -c \
    ' WARN fork_choice_updated did not return payload_id, scheduling retry$' \
    "$rust_log" || true)"
  payload_retry="${payload_retry:-0}"
  unsupported_state_sync="$(rg -c \
    ' WARN sync request failed peer=.* error=peer does not advertise N42 state-sync$' \
    "$rust_log" || true)"
  unsupported_state_sync="${unsupported_state_sync:-0}"
  missing_tx_forward_leader="$(rg -c \
    ' WARN leader peer not found for tx forward view=[0-9]+ leader_idx=[0-9]+ buf_len=[0-9]+ peers=[0-9]+$' \
    "$rust_log" || true)"
  missing_tx_forward_leader="${missing_tx_forward_leader:-0}"

  rg ' WARN ' "$rust_log" | rg -v \
    ' WARN (view timed out view=|pacemaker timeout, initiating view change view=|fork_choice_updated did not return payload_id, scheduling retry$|sync request failed peer=.* error=peer does not advertise N42 state-sync$|leader peer not found for tx forward view=[0-9]+ leader_idx=[0-9]+ buf_len=[0-9]+ peers=[0-9]+$)| WARN .*evicted rejected compact execution output hash=| WARN .*suppressed duplicate (commit )?vote \(already (commit-)?voted in this view\)' \
    >"$unknown" || true
  local log
  for log in "$runtime"/logs/gov{1,2,3,4,5}.log "$rust_log"; do
    require_file "$log"
    # Keep the structured ERROR level case-sensitive. With a global -i this
    # also matched harmless INFO fields such as "error=IO error on outbound
    # stream" and falsely rejected an otherwise healthy startup handshake.
    rg ' ERROR ' "$log" >>"$critical" || true
    rg -i '(^|[^a-z])(panic|fatal|equivocat)' "$log" >>"$critical" || true
  done

  test "$total" -gt 0
  test "$timeout" -gt 0
  test "$timeout" -eq "$pacemaker"
  test "$eviction" -eq "$commits"
  # The strict runtime has one persisted-state start before the soak and one
  # controlled restart in the finalizer. Each start may observe one SYNCING
  # FCU and one unsupported state-sync response from each of the five Gov
  # peers before authenticated block catch-up takes over.
  test "$payload_retry" -le 2
  test "$unsupported_state_sync" -le 10
  # A missing validator may be selected while a transaction is buffered. The
  # orchestrator emits at most one warning for that view, never one per flush
  # tick, and the missing-leader views are bounded by pacemaker timeouts.
  test "$missing_tx_forward_leader" -le "$timeout"
  test "$total" -eq \
    $((timeout + pacemaker + eviction + duplicate_vote + duplicate_commit +
      payload_retry + unsupported_state_sync + missing_tx_forward_leader))
  test ! -s "$unknown"
  test ! -s "$critical"

  local summary
  summary="$(jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg log "$rust_log" \
    --arg log_sha256 "$(shasum -a 256 "$rust_log" | awk '{print $1}')" \
    --argjson total "$total" \
    --argjson timeout "$timeout" \
    --argjson pacemaker "$pacemaker" \
    --argjson eviction "$eviction" \
    --argjson commits "$commits" \
    --argjson duplicate_vote "$duplicate_vote" \
    --argjson duplicate_commit "$duplicate_commit" \
    --argjson payload_retry "$payload_retry" \
    --argjson unsupported_state_sync "$unsupported_state_sync" \
    --argjson missing_tx_forward_leader "$missing_tx_forward_leader" '
    {at:$at,event:"mixed_client_runtime_log_audit",status:"PASS",
      rustLog:$log,rustLogSha256:$log_sha256,totalWarnings:$total,
      warningCounts:{viewTimeout:$timeout,pacemakerTimeout:$pacemaker,
        compactExecutionEviction:$eviction,rustLeaderCommit:$commits,
        duplicateVoteSuppression:$duplicate_vote,
        duplicateCommitVoteSuppression:$duplicate_commit,
        payloadBuildRetry:$payload_retry,
        unsupportedStateSyncFallback:$unsupported_state_sync,
        missingTxForwardLeader:$missing_tx_forward_leader},
      missingTxForwardLeaderAtMostOncePerView:true,
      warningPartitionExact:true,timeoutSetsCountExact:true,
      compactEvictionsMatchRustLeaderCommits:true,
      unexpectedWarnings:0,criticalSignals:0,
      govLogsChecked:5,rustLogsChecked:1}')"
  if test -n "$evidence_file"; then
    mkdir -p "$(dirname "$evidence_file")"
    printf '%s\n' "$summary" >>"$evidence_file"
  fi
  printf '%s\n' "$summary"
}

audit_rust_resources() {
  local evidence_file="${1:?resource evidence required}"
  local minimum_elapsed="${2:-86400}"
  local output_file="${3:-}"
  require_file "$evidence_file"

  jq -e -s --argjson minimum_elapsed "$minimum_elapsed" '
    length >= 2 and
    all(.[];
      .event == "rust_resource_snapshot" and
      (.pid|type) == "number" and (.head|type) == "number" and
      (.rssKiB|type) == "number" and .rssKiB > 0 and .rssKiB <= 1048576 and
      (.threads|type) == "number" and .threads > 0 and .threads <= 256 and
      (.fileDescriptors|type) == "number" and
        .fileDescriptors > 0 and .fileDescriptors <= 256 and
      (.rethDataKiB|type) == "number" and .rethDataKiB > 0 and
      (.consensusDataKiB|type) == "number" and .consensusDataKiB > 0 and
      (.logBytes|type) == "number" and
      (.qmdbWalBytes|type) == "number") and
    ([.[].pid] | unique | length) == 1 and
    ([.[].at | fromdateiso8601] as $times |
      ($times[-1] - $times[0]) >= $minimum_elapsed and
      ([range(1; $times|length) as $i |
        ($times[$i]-$times[$i-1]) > 0 and
        ($times[$i]-$times[$i-1]) <= 360] | all)) and
    .[-1].head > .[0].head and
    ([range(1;length) as $i |
      .[$i].head >= .[$i-1].head and
      .[$i].logBytes >= .[$i-1].logBytes and
      .[$i].qmdbWalBytes >= .[$i-1].qmdbWalBytes] | all)
  ' "$evidence_file" >/dev/null

  local summary
  summary="$(jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg evidence "$evidence_file" \
    --arg evidence_sha256 "$(shasum -a 256 "$evidence_file" | awk '{print $1}')" \
    --argjson minimum_elapsed "$minimum_elapsed" \
    --slurpfile samples "$evidence_file" '
    [$samples[].at | fromdateiso8601] as $times |
    {at:$at,event:"rust_resource_audit",status:"PASS",
      evidence:$evidence,evidenceSha256:$evidence_sha256,
      samples:($samples|length),pid:$samples[0].pid,
      firstAt:$samples[0].at,lastAt:$samples[-1].at,
      elapsedSeconds:($times[-1]-$times[0]),
      minimumElapsedSeconds:$minimum_elapsed,
      maximumSampleGapSeconds:([range(1;$times|length) as $i |
        $times[$i]-$times[$i-1]]|max),
      startHead:$samples[0].head,endHead:$samples[-1].head,
      headGrowth:($samples[-1].head-$samples[0].head),
      rssKiB:{minimum:([$samples[].rssKiB]|min),
        maximum:([$samples[].rssKiB]|max),limit:1048576},
      threads:{minimum:([$samples[].threads]|min),
        maximum:([$samples[].threads]|max),limit:256},
      fileDescriptors:{minimum:([$samples[].fileDescriptors]|min),
        maximum:([$samples[].fileDescriptors]|max),limit:256},
      growth:{rethDataKiB:($samples[-1].rethDataKiB-$samples[0].rethDataKiB),
        consensusDataKiB:($samples[-1].consensusDataKiB-$samples[0].consensusDataKiB),
        logBytes:($samples[-1].logBytes-$samples[0].logBytes),
        qmdbWalBytes:($samples[-1].qmdbWalBytes-$samples[0].qmdbWalBytes)},
      allocatedStorageStepDecreaseKiB:{
        maximumObserved:([range(1;$samples|length) as $i |
          ([($samples[$i-1].rethDataKiB-$samples[$i].rethDataKiB),
            ($samples[$i-1].consensusDataKiB-$samples[$i].consensusDataKiB)] | max) |
          select(. > 0)] | max // 0),
        rethMaximum:([range(1;$samples|length) as $i |
          ($samples[$i-1].rethDataKiB-$samples[$i].rethDataKiB) |
          select(. > 0)] | max // 0),
        consensusMaximum:([range(1;$samples|length) as $i |
          ($samples[$i-1].consensusDataKiB-$samples[$i].consensusDataKiB) |
          select(. > 0)] | max // 0)},
      singleProcess:true,logicalCountersMonotonic:true,
      allocatedStorageMeasurementsNonnegative:true,
      allocatedStorageMayDecreaseDuringCompaction:true,
      headLogAndWalCountersMonotonic:true}')"
  if test -n "$output_file"; then
    mkdir -p "$(dirname "$output_file")"
    printf '%s\n' "$summary" >>"$output_file"
  fi
  printf '%s\n' "$summary"
}

monitor_rust_resources() {
  local duration_seconds="${1:?duration seconds required}"
  local interval_seconds="${2:-300}"
  local evidence_file="${3:-$runtime/evidence/rust-resource-snapshots.jsonl}"
  local first_sample_seconds="$SECONDS"
  while true; do
    record_rust_resources "$evidence_file" >/dev/null
    if test $((SECONDS - first_sample_seconds)) -ge "$duration_seconds"; then
      break
    fi
    sleep "$interval_seconds"
  done
}

record_clock_snapshot() {
  label="${1:?snapshot label required}"
  evidence_file="${2:-$runtime/evidence/clock-snapshots.jsonl}"
  local -a ports
  read -r -a ports <<<"${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545 29546}"
  reference_port="${ports[0]}"
  sample_dir="$(mktemp -d)"
  trap 'rm -rf "$sample_dir"' RETURN
  expected=""
  for port in "${ports[@]}"; do
    curl -fsS --max-time 3 \
      -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
      "http://127.0.0.1:$port" >"$sample_dir/$port.json"
    identity="$(jq -er '.result | [.number,.hash,.stateRoot,.receiptsRoot,.timestamp] | join(":")' \
      "$sample_dir/$port.json")"
    if test -z "$expected"; then
      expected="$identity"
    elif test "$identity" != "$expected"; then
      echo "clock snapshot refused: participants disagree" >&2
      return 1
    fi
  done
  mkdir -p "$(dirname "$evidence_file")"
  wall_time="$(date +%s)"
  timestamp_hex="$(jq -er '.result.timestamp' "$sample_dir/$reference_port.json")"
  block_time=$((timestamp_hex))
  jq -c \
    --arg at "$(date -u +%FT%TZ)" \
    --arg label "$label" \
    --argjson wall_time "$wall_time" \
    --argjson block_time "$block_time" \
    --argjson participants "${#ports[@]}" \
    '.result | {
      at:$at,
      label:$label,
      number,
      hash,
      stateRoot,
      receiptsRoot,
      timestamp,
      wallTime:$wall_time,
      futureSeconds:($block_time - $wall_time),
      participants:$participants,
      allEqual:true
    }' "$sample_dir/$reference_port.json" >>"$evidence_file"
}

record_single_head() {
  label="${1:?snapshot label required}"
  port="${2:?RPC port required}"
  evidence_file="${3:-$runtime/evidence/head-snapshots.jsonl}"
  response="$(curl -fsS --max-time 3 \
    -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
    "http://127.0.0.1:$port")"
  wall_time="$(date +%s)"
  timestamp_hex="$(printf '%s' "$response" | jq -er '.result.timestamp')"
  block_time=$((timestamp_hex))
  mkdir -p "$(dirname "$evidence_file")"
  printf '%s' "$response" | jq -c \
    --arg at "$(date -u +%FT%TZ)" \
    --arg label "$label" \
    --argjson port "$port" \
    --argjson wall_time "$wall_time" \
    --argjson block_time "$block_time" \
    '.result | {
      at:$at,
      label:$label,
      number,
      hash,
      stateRoot,
      receiptsRoot,
      timestamp,
      wallTime:$wall_time,
      futureSeconds:($block_time - $wall_time),
      rpcPort:$port
    }' >>"$evidence_file"
}

write_era_checksums() {
  era_dir="${1:?ERA directory required}"
  test -d "$era_dir" || {
    echo "ERA directory does not exist: $era_dir" >&2
    return 1
  }
  checksum_file="$era_dir/checksums.txt"
  temporary_file="$(mktemp "$era_dir/.checksums.XXXXXX")"
  found=0
  while IFS= read -r era_file; do
    sha256sum "$era_file" | awk '{print $1}' >>"$temporary_file"
    found=$((found + 1))
  done < <(find "$era_dir" -maxdepth 1 -type f \
    \( -name '*.era1' -o -name '*.ere' \) -print | LC_ALL=C sort)
  if test "$found" -eq 0; then
    echo "no ERA1/ERE files found in $era_dir" >&2
    rm -f "$temporary_file"
    return 1
  fi
  mv "$temporary_file" "$checksum_file"
}

archive_manifest() {
  local snapshot_dir="${1:?snapshot directory required}"
  test -d "$snapshot_dir/reth" || {
    echo "snapshot is missing reth/: $snapshot_dir" >&2
    return 1
  }
  test -d "$snapshot_dir/consensus" || {
    echo "snapshot is missing consensus/: $snapshot_dir" >&2
    return 1
  }
  if find "$snapshot_dir/reth" "$snapshot_dir/consensus" -type l -print -quit | grep -q .; then
    echo "snapshot refuses symbolic links" >&2
    return 1
  fi
  local temporary_file
  temporary_file="$(mktemp "$snapshot_dir/.manifest.XXXXXX")"
  (
    cd "$snapshot_dir"
    while IFS= read -r file; do
      sha256sum "$file"
    done < <(find reth consensus -type f -print | LC_ALL=C sort)
  ) >"$temporary_file"
  test -s "$temporary_file" || {
    echo "snapshot contains no files" >&2
    rm -f "$temporary_file"
    return 1
  }
  mv "$temporary_file" "$snapshot_dir/manifest.sha256"
}

archive_verify() {
  local snapshot_dir="${1:?snapshot directory required}"
  require_file "$snapshot_dir/manifest.sha256"
  (
    cd "$snapshot_dir"
    sha256sum -c manifest.sha256
  )
}

archive_export() {
  local node_name="${1:?runtime node name required}"
  local output_dir="${2:?snapshot output directory required}"
  local source_dir="$runtime/$node_name"
  test -d "$source_dir/reth" || {
    echo "source is missing reth/: $source_dir" >&2
    return 1
  }
  test -d "$source_dir/consensus" || {
    echo "source is missing consensus/: $source_dir" >&2
    return 1
  }
  test ! -e "$output_dir" || {
    echo "snapshot output already exists: $output_dir" >&2
    return 1
  }
  mkdir -p "$output_dir"
  chmod 700 "$output_dir"
  cp -R -p "$source_dir/reth" "$output_dir/reth"
  cp -R -p "$source_dir/consensus" "$output_dir/consensus"
  archive_manifest "$output_dir"
  archive_verify "$output_dir"
}

archive_import() {
  local snapshot_dir="${1:?snapshot directory required}"
  local destination_dir="${2:?fresh destination directory required}"
  archive_verify "$snapshot_dir"
  test ! -e "$destination_dir" || {
    echo "archive import destination already exists: $destination_dir" >&2
    return 1
  }
  mkdir -p "$destination_dir"
  chmod 700 "$destination_dir"
  cp -R -p "$snapshot_dir/reth" "$destination_dir/reth"
  cp -R -p "$snapshot_dir/consensus" "$destination_dir/consensus"
  cp -p "$snapshot_dir/manifest.sha256" "$destination_dir/manifest.sha256"
  archive_verify "$destination_dir"
}

archive_corruption_drill() {
  local snapshot_dir="${1:?snapshot directory required}"
  local corrupt_dir="${2:?corrupt-copy destination required}"
  local recovered_dir="${3:?recovered destination required}"
  local evidence_file="${4:-$runtime/evidence/archive-corruption-drill.jsonl}"
  archive_import "$snapshot_dir" "$corrupt_dir"
  local corrupt_target="$corrupt_dir/consensus/gov5_qmdb_branches.bin"
  require_file "$corrupt_target"
  printf '\xff' | dd of="$corrupt_target" bs=1 seek=0 count=1 conv=notrunc status=none
  if archive_verify "$corrupt_dir" >/dev/null 2>&1; then
    echo "corruption drill failed: modified archive still verified" >&2
    return 1
  fi
  archive_import "$snapshot_dir" "$recovered_dir"
  mkdir -p "$(dirname "$evidence_file")"
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg snapshot "$snapshot_dir" \
    --arg corrupt_copy "$corrupt_dir" \
    --arg recovered_copy "$recovered_dir" \
    --arg target "consensus/gov5_qmdb_branches.bin" \
    --arg manifest_sha256 "$(sha256sum "$snapshot_dir/manifest.sha256" | awk '{print $1}')" \
    '{
      at:$at,
      event:"archive_corruption_recovery",
      snapshot:$snapshot,
      corruptedCopy:$corrupt_copy,
      corruptedTarget:$target,
      corruptionDetected:true,
      recoveredCopy:$recovered_copy,
      recoveredVerified:true,
      manifestSha256:$manifest_sha256
    }' >>"$evidence_file"
}

rpc_request() {
  local endpoint="${1:?RPC endpoint required}"
  local method="${2:?RPC method required}"
  local params="${3:?RPC params required}"
  local request
  request="$(jq -nc --arg method "$method" --argjson params "$params" \
    '{jsonrpc:"2.0",id:1,method:$method,params:$params}')"
  curl -fsS --max-time 10 \
    -H 'content-type: application/json' \
    --data "$request" "$endpoint"
}

archive_rpc_parity() {
  local gov_endpoint="${1:?Gov RPC endpoint required}"
  local rust_endpoint="${2:?Rust RPC endpoint required}"
  local evidence_file="${3:-$runtime/evidence/p5-archive-rpc-parity.jsonl}"
  local qmdb_proof_verifier="${N42_QUAL_QMDB_PROOF_VERIFY:-$repo/target/release/n42-qmdb-proof-verify}"
  require_file "$qmdb_proof_verifier"
  local heights=(0 29 999 1000 1999 2000 3999 4000 4999 5000 5189)
  local addresses=(
    "0x81d4c1f92ddb837cb46f82280d9b491b101fa582"
    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
  )
  local qmdb_keys=(
    "0x43f460c4d9a58c02e7ff036c37ec1968bf56805c8e549ab945f29059fd596212"
    "0x33977041c4e34e98960946ac3b6a251aba9a3102783167b487b3567f94465a2a"
  )
  local gov_head_hex rust_head_hex gov_head rust_head reference_height_number
  local reference_height reference_block reference_block_hash reference_root
  local -a reference_proofs=()
  local address_index

  gov_head_hex="$(rpc_request "$gov_endpoint" eth_blockNumber '[]' | jq -er '.result')"
  rust_head_hex="$(rpc_request "$rust_endpoint" eth_blockNumber '[]' | jq -er '.result')"
  gov_head=$((gov_head_hex))
  rust_head=$((rust_head_hex))
  if test "$gov_head" -lt "$rust_head"; then
    reference_height_number="$gov_head"
  else
    reference_height_number="$rust_head"
  fi
  reference_height="$(printf '0x%x' "$reference_height_number")"
  reference_block="$(rpc_request "$gov_endpoint" eth_getBlockByNumber \
    "$(jq -nc --arg height "$reference_height" '[$height,false]')")"
  reference_block_hash="$(printf '%s' "$reference_block" | jq -er '.result.hash')"
  reference_root="$(printf '%s' "$reference_block" | jq -er '.result.stateRoot')"
  for address_index in 0 1; do
    local reference_response rust_reference_response rust_reference_root
    local rust_reference_proof
    reference_response="$(rpc_request "$gov_endpoint" eth_getProof \
      "$(jq -nc --arg address "${addresses[$address_index]}" \
        --arg height "$reference_height" '[$address,[],$height]')")"
    reference_proofs+=("$(printf '%s' "$reference_response" |
      jq -er '.result.accountProof[0] | select(length > 66)')")
    rust_reference_response="$(rpc_request "$rust_endpoint" n42_qmdbArchiveProof \
      "$(jq -nc --arg hash "$reference_block_hash" \
        --arg key "${qmdb_keys[$address_index]}" '[$hash,$key]')")"
    rust_reference_root="$(printf '%s' "$rust_reference_response" | jq -er \
      'select(.error == null) | .result.root')"
    rust_reference_proof="$(printf '%s' "$rust_reference_response" | jq -er \
      'select(.error == null) | "0x" + .result.proofHex')"
    if test "$rust_reference_root" != "$reference_root"; then
      echo "QMDB reference proof root mismatch at height $reference_height_number" >&2
      return 1
    fi
    if test "$rust_reference_proof" != "${reference_proofs[$address_index]}"; then
      echo "QMDB reference proof bytes mismatch at height $reference_height_number" >&2
      return 1
    fi
    if ! "$qmdb_proof_verifier" \
      --root "$reference_root" \
      --key "${qmdb_keys[$address_index]}" \
      --proof-hex "$rust_reference_proof" >/dev/null; then
      echo "QMDB reference proof verification failed at height $reference_height_number" >&2
      return 1
    fi
  done

  mkdir -p "$(dirname "$evidence_file")"
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --argjson height "$reference_height_number" \
    --arg block_hash "$reference_block_hash" \
    --arg state_root "$reference_root" \
    --argjson proofs "${#reference_proofs[@]}" \
    '{
      at:$at,
      event:"archive_qmdb_reference_parity",
      height:$height,
      blockHash:$block_hash,
      stateRoot:$state_root,
      proofs:$proofs,
      govRustProofRootsExact:true,
      govRustProofBytesExact:true,
      govRustProofsOfflineVerified:true
    }' >>"$evidence_file"

  local height
  for height in "${heights[@]}"; do
    local height_hex block_response block_hash state_root
    local checks=0 proof_bytes_checks=0
    height_hex="$(printf '0x%x' "$height")"
    block_response="$(rpc_request "$gov_endpoint" eth_getBlockByNumber \
      "$(jq -nc --arg height "$height_hex" '[$height,false]')")"
    block_hash="$(printf '%s' "$block_response" | jq -er '.result.hash')"
    state_root="$(printf '%s' "$block_response" | jq -er '.result.stateRoot')"

    local method params gov_response rust_response gov_result rust_result label
    for label in \
      "eth_getBlockByNumber:false" \
      "eth_getBlockByNumber:true" \
      "eth_getBlockByHash:false" \
      "eth_getBlockByHash:true" \
      "eth_getBlockReceipts" \
      "eth_getBlockTransactionCountByNumber" \
      "eth_getLogs"; do
      method="${label%%:*}"
      case "$label" in
        eth_getBlockByNumber:false)
          params="$(jq -nc --arg height "$height_hex" '[$height,false]')" ;;
        eth_getBlockByNumber:true)
          params="$(jq -nc --arg height "$height_hex" '[$height,true]')" ;;
        eth_getBlockByHash:false)
          params="$(jq -nc --arg hash "$block_hash" '[$hash,false]')" ;;
        eth_getBlockByHash:true)
          params="$(jq -nc --arg hash "$block_hash" '[$hash,true]')" ;;
        eth_getBlockReceipts)
          params="$(jq -nc --arg height "$height_hex" '[$height]')" ;;
        eth_getBlockTransactionCountByNumber)
          params="$(jq -nc --arg height "$height_hex" '[$height]')" ;;
        eth_getLogs)
          params="$(jq -nc --arg height "$height_hex" \
            '[{fromBlock:$height,toBlock:$height}]')" ;;
      esac
      gov_response="$(rpc_request "$gov_endpoint" "$method" "$params")"
      rust_response="$(rpc_request "$rust_endpoint" "$method" "$params")"
      gov_result="$(printf '%s' "$gov_response" | jq -ecS \
        'select(.error == null) | .result')"
      rust_result="$(printf '%s' "$rust_response" | jq -ecS \
        'select(.error == null) | .result')"
      if test "$gov_result" != "$rust_result"; then
        echo "archive RPC mismatch at height $height: $label" >&2
        return 1
      fi
      checks=$((checks + 1))
    done

    for address_index in 0 1; do
      local address state_method
      address="${addresses[$address_index]}"
      for state_method in eth_getBalance eth_getTransactionCount eth_getCode; do
        params="$(jq -nc --arg address "$address" --arg height "$height_hex" \
          '[$address,$height]')"
        gov_response="$(rpc_request "$gov_endpoint" "$state_method" "$params")"
        rust_response="$(rpc_request "$rust_endpoint" "$state_method" "$params")"
        gov_result="$(printf '%s' "$gov_response" | jq -ecS \
          'select(.error == null) | .result')"
        rust_result="$(printf '%s' "$rust_response" | jq -ecS \
          'select(.error == null) | .result')"
        if test "$gov_result" != "$rust_result"; then
          echo "archive state RPC mismatch at height $height: $state_method $address" >&2
          return 1
        fi
        checks=$((checks + 1))
      done

      params="$(jq -nc --arg address "$address" --arg height "$height_hex" \
        '[$address,"0x0",$height]')"
      gov_response="$(rpc_request "$gov_endpoint" eth_getStorageAt "$params")"
      rust_response="$(rpc_request "$rust_endpoint" eth_getStorageAt "$params")"
      gov_result="$(printf '%s' "$gov_response" | jq -er \
        'select(.error == null) | .result')"
      rust_result="$(printf '%s' "$rust_response" | jq -er \
        'select(.error == null) | .result')"
      if test "$gov_result" != "$rust_result"; then
        echo "archive storage RPC mismatch at height $height: $address" >&2
        return 1
      fi
      checks=$((checks + 1))

      rust_response="$(rpc_request "$rust_endpoint" n42_qmdbArchiveProof \
        "$(jq -nc --arg hash "$block_hash" \
          --arg key "${qmdb_keys[$address_index]}" '[$hash,$key]')")"
      local proof_root proof_hex
      proof_root="$(printf '%s' "$rust_response" | jq -er \
        'select(.error == null) | .result.root')"
      proof_hex="$(printf '%s' "$rust_response" | jq -er \
        'select(.error == null) | "0x" + .result.proofHex')"
      if test "$proof_root" != "$state_root"; then
        echo "QMDB archive proof root mismatch at height $height: $address" >&2
        return 1
      fi
      if ! "$qmdb_proof_verifier" \
        --root "$state_root" \
        --key "${qmdb_keys[$address_index]}" \
        --proof-hex "$proof_hex" >/dev/null; then
        echo "QMDB archive proof verification failed at height $height: $address" >&2
        return 1
      fi
      if test "$state_root" = "$reference_root" &&
        test "$proof_hex" != "${reference_proofs[$address_index]}"; then
        echo "QMDB archive proof bytes mismatch at height $height: $address" >&2
        return 1
      fi
      if test "$state_root" = "$reference_root"; then
        proof_bytes_checks=$((proof_bytes_checks + 1))
      fi
      checks=$((checks + 2))
    done

    jq -nc \
      --arg at "$(date -u +%FT%TZ)" \
      --argjson height "$height" \
      --arg block_hash "$block_hash" \
      --arg state_root "$state_root" \
      --arg reference_height "$reference_height" \
      --argjson checks "$checks" \
      --argjson proof_bytes_checks "$proof_bytes_checks" \
      '{
        at:$at,
        event:"archive_rpc_parity",
        height:$height,
        blockHash:$block_hash,
        stateRoot:$state_root,
        checks:$checks,
        govRustRpcExact:true,
        qmdbProofRootExact:true,
        qmdbProofOfflineVerified:true,
        qmdbProofByteComparisonsAgainstGovReference:$proof_bytes_checks,
        govQmdbReferenceProofExact:true,
        govQmdbReferenceHeight:$reference_height
      }' >>"$evidence_file"
  done
}

transaction_burst() {
  local artifact="${1:-$runtime/artifacts/p4-signed-transaction-burst.json}"
  local evidence_file="${2:-$runtime/evidence/p4-transaction-burst.jsonl}"
  local sender contract
  local -a ports
  local gov_ingress_port="${N42_QUAL_GOV_INGRESS_PORT:-28501}"
  local rust_ingress_port="${N42_QUAL_RUST_INGRESS_PORT:-29545}"
  local resume_existing="${N42_QUAL_BURST_RESUME_EXISTING:-0}"
  local state_visibility_attempts="${N42_QUAL_STATE_VISIBILITY_ATTEMPTS:-30}"
  local state_visibility_delay="${N42_QUAL_STATE_VISIBILITY_DELAY_SECONDS:-1}"
  [[ "$resume_existing" =~ ^[01]$ ]]
  [[ "$state_visibility_attempts" =~ ^[1-9][0-9]*$ ]]
  [[ "$state_visibility_delay" =~ ^[0-9]+$ ]]
  read -r -a ports <<<"${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545 29546}"
  case " ${ports[*]} " in
    *" $gov_ingress_port "*) ;;
    *) echo "Gov transaction ingress port is not monitored: $gov_ingress_port" >&2; return 1 ;;
  esac
  case " ${ports[*]} " in
    *" $rust_ingress_port "*) ;;
    *) echo "Rust transaction ingress port is not monitored: $rust_ingress_port" >&2; return 1 ;;
  esac
  require_file "$artifact"
  jq -e '
    (.chainId == 1143) and
    (.expectedContract | test("^0x[0-9a-f]{40}$")) and
    (.transactions | length >= 17) and
    (.transactions[0].nonce | type == "number") and
    ([.transactions[].nonce] ==
      [range(.transactions[0].nonce;
        .transactions[0].nonce + (.transactions | length))]) and
    (all(.transactions[];
      (.raw | startswith("0x")) and
      (.hash | test("^0x[0-9a-f]{64}$")) and
      (.intendedIngress == "gov" or .intendedIngress == "rust")))
  ' "$artifact" >/dev/null
  sender="$(jq -er '.sender' "$artifact")"
  contract="$(jq -er '.expectedContract' "$artifact")"
  mkdir -p "$(dirname "$evidence_file")"

  local port nonce_response first_nonce expected_nonce count
  first_nonce="$(jq -er '.transactions[0].nonce' "$artifact")"
  count="$(jq -er '.transactions | length' "$artifact")"
  if test "$resume_existing" = 1; then
    printf -v expected_nonce '0x%x' "$((first_nonce + count))"
  else
    printf -v expected_nonce '0x%x' "$first_nonce"
  fi
  for port in "${ports[@]}"; do
    nonce_response="$(rpc_request "http://127.0.0.1:$port" \
      eth_getTransactionCount "$(jq -nc --arg sender "$sender" '[$sender,"latest"]')")"
    if test "$(printf '%s' "$nonce_response" | jq -er '.result')" != "$expected_nonce"; then
      echo "transaction burst requires sender nonce $expected_nonce at port $port" >&2
      return 1
    fi
  done

  if test "${N42_QUAL_BURST_PREFLIGHT_ONLY:-0}" = 1; then
    jq -nc \
      --arg at "$(date -u +%FT%TZ)" \
      --arg artifact "$artifact" \
      --arg artifact_sha256 "$(sha256sum "$artifact" | awk '{print $1}')" \
      --arg sender "$sender" \
      --arg expected_nonce "$expected_nonce" \
      --argjson first_nonce "$first_nonce" \
      --argjson ports "$(printf '%s\n' "${ports[@]}" | jq -R 'tonumber' | jq -s '.')" \
      '{
        at:$at,event:"p4_transaction_burst_preflight",artifact:$artifact,
        artifactSha256:$artifact_sha256,sender:$sender,firstNonce:$first_nonce,
        expectedNonce:$expected_nonce,ports:$ports,
        allConfiguredEndpointNoncesExact:true,transactionsSent:0
      }' >>"$evidence_file"
    return 0
  fi

  local index kind nonce ingress raw expected_hash ingress_port
  local response returned_hash receipt block_number first_block="" last_block=""
  if test "$resume_existing" = 1; then
    test -s "$evidence_file"
    jq -e -s --slurpfile artifact "$artifact" '
      (map(select(.event == "p4_transaction_finalized"))) as $finalized |
      (map(select(.event == "p4_transaction_burst_pass")) | length) == 0 and
      ($finalized | length) == ($artifact[0].transactions | length) and
      ([range(0; $finalized | length) as $index |
        ($finalized[$index].nonce == $artifact[0].transactions[$index].nonce and
         $finalized[$index].kind == $artifact[0].transactions[$index].kind and
         $finalized[$index].ingress == $artifact[0].transactions[$index].intendedIngress and
         $finalized[$index].transactionHash == $artifact[0].transactions[$index].hash and
         $finalized[$index].status == "0x1")] | all)
    ' "$evidence_file" >/dev/null
    first_block="$(jq -ers 'map(select(.event == "p4_transaction_finalized"))[0].blockNumber' \
      "$evidence_file")"
    last_block="$(jq -ers 'map(select(.event == "p4_transaction_finalized"))[-1].blockNumber' \
      "$evidence_file")"
  fi
  if test "$resume_existing" = 0; then
    for index in $(seq 0 $((count - 1))); do
    kind="$(jq -er --argjson index "$index" '.transactions[$index].kind' "$artifact")"
    nonce="$(jq -er --argjson index "$index" '.transactions[$index].nonce' "$artifact")"
    ingress="$(jq -er --argjson index "$index" '.transactions[$index].intendedIngress' "$artifact")"
    raw="$(jq -er --argjson index "$index" '.transactions[$index].raw' "$artifact")"
    expected_hash="$(jq -er --argjson index "$index" '.transactions[$index].hash' "$artifact")"
    if test "$ingress" = gov; then
      ingress_port="$gov_ingress_port"
    else
      ingress_port="$rust_ingress_port"
    fi
    response="$(rpc_request "http://127.0.0.1:$ingress_port" \
      eth_sendRawTransaction "$(jq -nc --arg raw "$raw" '[$raw]')")"
    returned_hash="$(printf '%s' "$response" | jq -er 'select(.error == null) | .result')"
    if test "$returned_hash" != "$expected_hash"; then
      echo "transaction hash mismatch at nonce $nonce" >&2
      return 1
    fi

    receipt=""
    for _ in $(seq 1 300); do
      response="$(rpc_request "http://127.0.0.1:$ingress_port" \
        eth_getTransactionReceipt "$(jq -nc --arg hash "$expected_hash" '[$hash]')")"
      if test "$(printf '%s' "$response" | jq -r '.result != null')" = true; then
        receipt="$(printf '%s' "$response" | jq -ec '.result')"
        break
      fi
      sleep 1
    done
    if test -z "$receipt"; then
      echo "transaction was not finalized within 300 seconds: $expected_hash" >&2
      return 1
    fi
    if test "$(printf '%s' "$receipt" | jq -er '.status')" != "0x1"; then
      echo "transaction execution failed: $expected_hash" >&2
      return 1
    fi
    block_number="$(printf '%s' "$receipt" | jq -er '.blockNumber')"
    if test -z "$first_block"; then
      first_block="$block_number"
    fi
    last_block="$block_number"
    jq -nc \
      --arg at "$(date -u +%FT%TZ)" \
      --arg kind "$kind" \
      --argjson nonce "$nonce" \
      --arg ingress "$ingress" \
      --argjson ingress_port "$ingress_port" \
      --arg transaction_hash "$expected_hash" \
      --arg block_number "$block_number" \
      '{
        at:$at,event:"p4_transaction_finalized",kind:$kind,nonce:$nonce,
        ingress:$ingress,ingressPort:$ingress_port,
        transactionHash:$transaction_hash,blockNumber:$block_number,status:"0x1"
      }' >>"$evidence_file"
    done
  fi

  local method params reference candidate
  local exact_checks=0
  for method in \
    eth_getBlockByNumber:false \
    eth_getBlockByNumber:true \
    eth_getBlockReceipts \
    eth_getLogs; do
    case "$method" in
      eth_getBlockByNumber:false)
        params="$(jq -nc --arg block "$last_block" '[$block,false]')" ;;
      eth_getBlockByNumber:true)
        params="$(jq -nc --arg block "$last_block" '[$block,true]')" ;;
      eth_getBlockReceipts)
        params="$(jq -nc --arg block "$last_block" '[$block]')" ;;
      eth_getLogs)
        params="$(jq -nc --arg first "$first_block" --arg last "$last_block" \
          '[{fromBlock:$first,toBlock:$last}]')" ;;
    esac
    reference=""
    for port in "${ports[@]}"; do
      candidate="$(rpc_request "http://127.0.0.1:$port" "${method%%:*}" "$params" |
        jq -ecS 'select(.error == null) | .result')"
      if test -z "$reference"; then
        reference="$candidate"
      elif test "$candidate" != "$reference"; then
        echo "transaction burst RPC mismatch: $method at port $port" >&2
        return 1
      fi
      exact_checks=$((exact_checks + 1))
    done
  done

  local hash
  while IFS= read -r hash; do
    for method in eth_getTransactionByHash eth_getTransactionReceipt; do
      params="$(jq -nc --arg hash "$hash" '[$hash]')"
      reference=""
      for port in "${ports[@]}"; do
        candidate="$(rpc_request "http://127.0.0.1:$port" "$method" "$params" |
          jq -ecS 'select(.error == null) | .result')"
        if test -z "$reference"; then
          reference="$candidate"
        elif test "$candidate" != "$reference"; then
          echo "transaction burst RPC mismatch: $method $hash at port $port" >&2
          return 1
        fi
        exact_checks=$((exact_checks + 1))
      done
    done
  done < <(jq -r '.transactions[].hash' "$artifact")

  local state_method
  for state_method in \
    "eth_getTransactionCount:$sender" \
    "eth_getBalance:$sender" \
    "eth_getBalance:0x000000000000000000000000000000000000dead" \
    "eth_getCode:$contract"; do
    params="$(jq -nc --arg address "${state_method#*:}" --arg block "$last_block" \
      '[$address,$block]')"
    reference=""
    for port in "${ports[@]}"; do
      candidate="$(rpc_request "http://127.0.0.1:$port" "${state_method%%:*}" "$params" |
        jq -ecS 'select(.error == null) | .result')"
      if test -z "$reference"; then
        reference="$candidate"
      elif test "$candidate" != "$reference"; then
        echo "transaction burst state mismatch: $state_method at port $port" >&2
        return 1
      fi
      exact_checks=$((exact_checks + 1))
    done
  done
  params="$(jq -nc --arg contract "$contract" --arg block "$last_block" \
    '[$contract,"0x0",$block]')"
  local visibility_attempt storage_exact expected_storage
  expected_storage="0x0000000000000000000000000000000000000000000000000000000000000001"
  storage_exact=false
  for visibility_attempt in $(seq 1 "$state_visibility_attempts"); do
    reference=""
    storage_exact=true
    for port in "${ports[@]}"; do
      candidate="$(rpc_request "http://127.0.0.1:$port" eth_getStorageAt "$params" |
        jq -er 'select(.error == null) | .result')"
      if test -z "$reference"; then
        reference="$candidate"
      elif test "$candidate" != "$reference"; then
        storage_exact=false
      fi
      exact_checks=$((exact_checks + 1))
    done
    if test "$storage_exact" = true && test "$reference" = "$expected_storage"; then
      break
    fi
    test "$visibility_attempt" -ge "$state_visibility_attempts" ||
      sleep "$state_visibility_delay"
  done
  if test "$storage_exact" != true || test "$reference" != "$expected_storage"; then
    echo "transaction burst contract storage was not consistently visible" >&2
    return 1
  fi

  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg artifact "$artifact" \
    --arg artifact_sha256 "$(sha256sum "$artifact" | awk '{print $1}')" \
    --arg sender "$sender" \
    --arg contract "$contract" \
    --arg first_block "$first_block" \
    --arg last_block "$last_block" \
    --argjson transactions "$count" \
    --argjson exact_checks "$exact_checks" \
    --argjson endpoint_count "${#ports[@]}" \
    --argjson resumed "$resume_existing" \
    --argjson visibility_attempts "$visibility_attempt" \
    '{
      at:$at,event:"p4_transaction_burst_pass",artifact:$artifact,
      artifactSha256:$artifact_sha256,sender:$sender,contract:$contract,
      firstBlock:$first_block,lastBlock:$last_block,transactions:$transactions,
      govIngress:true,rustIngress:true,endpointCount:$endpoint_count,
      allConfiguredEndpointsExact:true,
      allSevenEndpointsExact:($endpoint_count == 7),
      receiptAndLogParity:true,stateAndStorageParity:true,
      exactRpcComparisons:$exact_checks,
      resumedFromFinalizedTransactionsOnly:($resumed == 1),
      stateVisibilityAttempts:$visibility_attempts,
      noTransactionsResentDuringResume:($resumed == 1)
    }' >>"$evidence_file"
}

case "${1:-}" in
  start-gov) start_gov ;;
  start-gov-node) start_gov_node "${2:-}" ;;
  stop-gov-node)
    node="${2:-}"
    test -n "$node" || {
      echo "gov node number required" >&2
      exit 2
    }
    if test "$node" -lt 1 || test "$node" -gt 6; then
      echo "gov node number must be in 1..6: $node" >&2
      exit 2
    fi
    stop_one "$runtime/pids/gov${node}.pid"
    ;;
  start-rust) start_rust ;;
  start-rust2) start_rust2 ;;
  stop-rust) stop_one "$runtime/pids/rust.pid" ;;
  stop-rust2) stop_one "$runtime/pids/rust2.pid" ;;
  start) start_gov; start_rust ;;
  stop) stop_all ;;
  restart-rust) stop_one "$runtime/pids/rust.pid"; start_rust ;;
  restart-rust2) stop_one "$runtime/pids/rust2.pid"; start_rust2 ;;
  restart-gov-node)
    node="${2:-}"
    test -n "$node" || {
      echo "gov node number required" >&2
      exit 2
    }
    stop_one "$runtime/pids/gov${node}.pid"
    start_gov_node "$node"
    ;;
  status) status ;;
  monitor-heads) monitor_heads "${2:-}" "${3:-10}" "${4:-}" ;;
  audit-soak) audit_soak "${2:-}" "${3:-}" "${4:-120}" "${5:-6}" "${6:-0}" ;;
  audit-rust-leaders) audit_rust_leaders "${2:-}" "${3:-}" "${4:-}" ;;
  audit-timeout-recovery) audit_timeout_recovery "${2:-}" "${3:-}" ;;
  audit-runtime-logs) audit_runtime_logs "${2:-}" "${3:-}" ;;
  audit-rust-resources) audit_rust_resources "${2:-}" "${3:-86400}" "${4:-}" ;;
  record-rust-resources) record_rust_resources "${2:-}" ;;
  monitor-rust-resources) monitor_rust_resources "${2:-}" "${3:-300}" "${4:-}" ;;
  record-clock) record_clock_snapshot "${2:-}" "${3:-}" ;;
  record-head) record_single_head "${2:-}" "${3:-}" "${4:-}" ;;
  era-checksums) write_era_checksums "${2:-}" ;;
  archive-export) archive_export "${2:-}" "${3:-}" ;;
  archive-verify) archive_verify "${2:-}" ;;
  archive-import) archive_import "${2:-}" "${3:-}" ;;
  archive-corruption-drill) archive_corruption_drill "${2:-}" "${3:-}" "${4:-}" "${5:-}" ;;
  archive-rpc-parity) archive_rpc_parity "${2:-}" "${3:-}" "${4:-}" ;;
  transaction-burst) transaction_burst "${2:-}" "${3:-}" ;;
  *)
    echo "usage: $0 {start-gov|start-gov-node N|stop-gov-node N|start-rust|start-rust2|stop-rust|stop-rust2|start|stop|restart-gov-node N|restart-rust|restart-rust2|status|monitor-heads <seconds> [interval] [evidence-file]|audit-soak <evidence-file> <minimum-elapsed-seconds> [maximum-gap-seconds] [maximum-lag] [require-zero-tx]|audit-rust-leaders <first-rust-height> [end-height] [evidence-file]|audit-timeout-recovery <rust-log> [evidence-file]|audit-runtime-logs <rust-log> [evidence-file]|audit-rust-resources <evidence-file> [minimum-elapsed-seconds] [output-file]|record-rust-resources [evidence-file]|monitor-rust-resources <seconds> [interval] [evidence-file]|record-clock <label> [evidence-file]|record-head <label> <port> [evidence-file]|era-checksums <directory>|archive-export <node> <snapshot>|archive-verify <snapshot>|archive-import <snapshot> <fresh-destination>|archive-corruption-drill <snapshot> <corrupt-copy> <recovered-copy> [evidence-file]|archive-rpc-parity <gov-endpoint> <rust-endpoint> [evidence-file]|transaction-burst [ARTIFACT] [EVIDENCE]}" >&2
    exit 2
    ;;
esac
