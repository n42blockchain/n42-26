#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-11-production-qualification}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
genesis_hash="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
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
  mkdir -p "$runtime/logs" "$runtime/pids"
  index=$((node - 1))
  pid_file="$runtime/pids/gov${node}.pid"
  if pid_alive "$pid_file"; then
    return
  fi
  args=(
    --chain private
    --profile n42
    --datadir "$runtime/gov/node${node}"
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
  nohup "$gov_binary" "${args[@]}" \
    >"$runtime/logs/gov${node}.log" 2>&1 &
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
  key_dir="/Users/jieliu/Documents/n42/live-interop-20260721/runtime-02-generated/node${validator_node}"
  validator_key_files=("$key_dir"/keystore/*.key)
  if test "${#validator_key_files[@]}" -ne 1 ||
    ! test -f "${validator_key_files[0]}"; then
    echo "expected exactly one validator key file in $key_dir/keystore" >&2
    return 2
  fi
  require_file "$key_dir/network-keys"
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
  nohup env \
    N42_CONSENSUS_CONFIG="$consensus_config" \
    N42_VALIDATOR_KEY="@${validator_key_files[0]}" \
    N42_P2P_KEY="@$key_dir/network-keys" \
    N42_DATA_DIR="$runtime/$name/consensus" \
    N42_GOV5_H2_PARTICIPANT=1 \
    N42_GOV5_HEADER_PROFILE=1 \
    N42_INTEROP_GENESIS_HASH="$genesis_hash" \
    N42_GOV5_BOOTSTRAP_BUNDLE="$runtime/artifacts/bootstrap-bundle.json" \
    N42_CONSENSUS_PORT="$consensus_port" \
    N42_STARHUB_PORT="$starhub_port" \
    N42_NO_AUTO_CONNECT=1 \
    N42_TRUSTED_PEERS="$trusted_peers" \
    N42_ENABLE_MDNS=0 \
    N42_ENABLE_DHT=0 \
    N42_ENABLE_HTTP_RPC=1 \
    "$rust_binary" node \
      --chain "$runtime/artifacts/genesis.json" \
      --datadir "$runtime/$name/reth" \
      --disable-discovery \
      --port "$reth_port" \
      --http \
      --http.addr 127.0.0.1 \
      --http.port "$http_port" \
      --authrpc.port "$auth_port" \
      --ipcdisable \
      --log.file.max-files 0 \
      --color never \
      >"$runtime/logs/$name.log" 2>&1 &
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

monitor_heads() {
  duration_seconds="${1:?duration seconds required}"
  interval_seconds="${2:-10}"
  evidence_file="${3:-$runtime/evidence/head-monitor.jsonl}"
  max_lag="${N42_QUAL_MAX_LAG:-16}"
  require_zero_tx="${N42_QUAL_REQUIRE_ZERO_TX:-0}"
  zero_tx_verified_to=-1
  ports=(28501 28502 28503 28504 28505 29545 29546)
  mkdir -p "$(dirname "$evidence_file")"
  deadline=$((SECONDS + duration_seconds))

  while test "$SECONDS" -lt "$deadline"; do
    sample_dir="$(mktemp -d)"
    sample_failed=0
    min_height=-1
    max_height=0
    for port in "${ports[@]}"; do
      if ! curl -fsS --max-time 3 \
        -H 'content-type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' \
        "http://127.0.0.1:$port" >"$sample_dir/latest-$port.json"; then
        sample_failed=1
        break
      fi
      number_hex="$(jq -er '.result.number' "$sample_dir/latest-$port.json")"
      number=$((number_hex))
      if test "$min_height" -lt 0 || test "$number" -lt "$min_height"; then
        min_height="$number"
      fi
      if test "$number" -gt "$max_height"; then
        max_height="$number"
      fi
    done

    if test "$sample_failed" -ne 0; then
      jq -nc --arg at "$(date -u +%FT%TZ)" \
        '{at:$at,ok:false,error:"rpc unavailable"}' >>"$evidence_file"
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
      error="canonical divergence at common height"
    elif test "$lag" -gt "$max_lag"; then
      ok=false
      error="execution lag exceeded bound"
    fi

    zero_tx_verified_from=-1
    if test "$ok" = true && test "$require_zero_tx" = 1; then
      if test "$zero_tx_verified_to" -lt 0; then
        zero_tx_verified_to="$min_height"
      else
        zero_tx_verified_from=$((zero_tx_verified_to + 1))
        if test "$zero_tx_verified_from" -le "$min_height"; then
          for height in $(seq "$zero_tx_verified_from" "$min_height"); do
          height_hex="$(printf '0x%x' "$height")"
          request="$(jq -nc --arg height "$height_hex" \
            '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$height,false]}')"
          gov_block="$(curl -fsS --max-time 3 \
            -H 'content-type: application/json' --data "$request" \
            "http://127.0.0.1:28501" 2>/dev/null || true)"
          rust_block="$(curl -fsS --max-time 3 \
            -H 'content-type: application/json' --data "$request" \
            "http://127.0.0.1:29546" 2>/dev/null || true)"
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
      --argjson zero_tx_required "$require_zero_tx" \
      --argjson zero_tx_verified_from "$zero_tx_verified_from" \
      --argjson zero_tx_verified_to "$zero_tx_verified_to" \
      --arg identity "$expected" \
      '{at:$at,ok:$ok,error:$error,commonHeight:$common_height,maximumHeight:$maximum_height,lag:$lag,identity:$identity,zeroTxRequired:$zero_tx_required,zeroTxVerifiedFrom:$zero_tx_verified_from,zeroTxVerifiedTo:$zero_tx_verified_to}' \
      >>"$evidence_file"
    rm -rf "$sample_dir"
    if test "$ok" != true; then
      return 1
    fi
    sleep "$interval_seconds"
  done
}

record_clock_snapshot() {
  label="${1:?snapshot label required}"
  evidence_file="${2:-$runtime/evidence/clock-snapshots.jsonl}"
  ports=(28501 28502 28503 28504 28505 29545 29546)
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
  timestamp_hex="$(jq -er '.result.timestamp' "$sample_dir/28501.json")"
  block_time=$((timestamp_hex))
  jq -c \
    --arg at "$(date -u +%FT%TZ)" \
    --arg label "$label" \
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
      participants:7,
      allEqual:true
    }' "$sample_dir/28501.json" >>"$evidence_file"
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
  local heights=(0 29 999 1000 1999 2000 3999 4000 4999 5000 5189)
  local addresses=(
    "0x81d4c1f92ddb837cb46f82280d9b491b101fa582"
    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
  )
  local qmdb_keys=(
    "0x43f460c4d9a58c02e7ff036c37ec1968bf56805c8e549ab945f29059fd596212"
    "0x33977041c4e34e98960946ac3b6a251aba9a3102783167b487b3567f94465a2a"
  )
  local reference_height="0x1388"
  local reference_block reference_root
  local -a reference_proofs=()
  local address_index

  reference_block="$(rpc_request "$gov_endpoint" eth_getBlockByNumber \
    "$(jq -nc --arg height "$reference_height" '[$height,false]')")"
  reference_root="$(printf '%s' "$reference_block" | jq -er '.result.stateRoot')"
  for address_index in 0 1; do
    local reference_response
    reference_response="$(rpc_request "$gov_endpoint" eth_getProof \
      "$(jq -nc --arg address "${addresses[$address_index]}" \
        --arg height "$reference_height" '[$address,[],$height]')")"
    reference_proofs+=("$(printf '%s' "$reference_response" |
      jq -er '.result.accountProof[0] | select(length > 66)')")
  done

  mkdir -p "$(dirname "$evidence_file")"
  local height
  for height in "${heights[@]}"; do
    local height_hex block_response block_hash state_root
    local checks=0
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
      if test "$state_root" = "$reference_root" &&
        test "$proof_hex" != "${reference_proofs[$address_index]}"; then
        echo "QMDB archive proof bytes mismatch at height $height: $address" >&2
        return 1
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
      '{
        at:$at,
        event:"archive_rpc_parity",
        height:$height,
        blockHash:$block_hash,
        stateRoot:$state_root,
        checks:$checks,
        govRustRpcExact:true,
        qmdbProofRootExact:true,
        qmdbProofBytesExactAgainstGovReference:true,
        govQmdbReferenceHeight:$reference_height
      }' >>"$evidence_file"
  done
}

case "${1:-}" in
  start-gov) start_gov ;;
  start-gov-node) start_gov_node "${2:-}" ;;
  start-rust) start_rust ;;
  start-rust2) start_rust2 ;;
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
  record-clock) record_clock_snapshot "${2:-}" "${3:-}" ;;
  record-head) record_single_head "${2:-}" "${3:-}" "${4:-}" ;;
  era-checksums) write_era_checksums "${2:-}" ;;
  archive-export) archive_export "${2:-}" "${3:-}" ;;
  archive-verify) archive_verify "${2:-}" ;;
  archive-import) archive_import "${2:-}" "${3:-}" ;;
  archive-corruption-drill) archive_corruption_drill "${2:-}" "${3:-}" "${4:-}" "${5:-}" ;;
  archive-rpc-parity) archive_rpc_parity "${2:-}" "${3:-}" "${4:-}" ;;
  *)
    echo "usage: $0 {start-gov|start-gov-node N|start-rust|start-rust2|start|stop|restart-gov-node N|restart-rust|restart-rust2|status|monitor-heads <seconds> [interval] [evidence-file]|record-clock <label> [evidence-file]|record-head <label> <port> [evidence-file]|era-checksums <directory>|archive-export <node> <snapshot>|archive-verify <snapshot>|archive-import <snapshot> <fresh-destination>|archive-corruption-drill <snapshot> <corrupt-copy> <recovered-copy> [evidence-file]|archive-rpc-parity <gov-endpoint> <rust-endpoint> [evidence-file]}" >&2
    exit 2
    ;;
esac
