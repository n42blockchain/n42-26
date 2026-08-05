#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT
mkdir -p "$test_root/bin" "$test_root/runtime/evidence" "$test_root/state"

cat >"$test_root/bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail

request=""
url=""
while test "$#" -gt 0; do
  case "$1" in
    --data)
      request="$2"
      shift 2
      ;;
    http://*)
      url="$1"
      shift
      ;;
    *)
      shift
      ;;
  esac
done

port="${url#http://127.0.0.1:}"
port="${port%%/*}"
if [[ "$request" == *'"latest"'* ]]; then
  counter="$MOCK_HEAD_STATE/$port"
  count=0
  test ! -s "$counter" || count="$(<"$counter")"
  count=$((count + 1))
  printf '%s\n' "$count" >"$counter"
  if test "${MOCK_PERSISTENT_LAG:-0}" = 1 && test "$port" = 28501; then
    number=0x64
  elif test "$count" -eq 1 && test "$port" = 28501; then
    number=0x64
  else
    number=0x66
  fi
else
  number=0x64
fi

printf '{"jsonrpc":"2.0","id":1,"result":{"number":"%s","hash":"0xaaa","stateRoot":"0xbbb","receiptsRoot":"0xccc","transactions":[]}}\n' "$number"
MOCK_CURL
chmod +x "$test_root/bin/curl"

transient="$test_root/runtime/evidence/transient.jsonl"
env PATH="$test_root/bin:$PATH" \
  MOCK_HEAD_STATE="$test_root/state" \
  N42_QUAL_RUNTIME="$test_root/runtime" \
  N42_QUAL_PORTS='28501 28502' \
  N42_QUAL_RUST_PORT=28502 \
  N42_QUAL_MAX_LAG=1 \
  N42_QUAL_LAG_CONFIRMATION_ATTEMPTS=3 \
  N42_QUAL_LAG_CONFIRMATION_DELAY_SECONDS=0 \
  "$repo/scripts/gov5-interop-qualification.sh" \
  monitor-heads 0 0 "$transient"
jq -e '.ok==true and .lag==0 and .latestSnapshotAttempts==2 and
  .latestSnapshotConcurrent==true' "$transient" >/dev/null

rm -f "$test_root/state"/*
persistent="$test_root/runtime/evidence/persistent.jsonl"
if env PATH="$test_root/bin:$PATH" \
  MOCK_HEAD_STATE="$test_root/state" MOCK_PERSISTENT_LAG=1 \
  N42_QUAL_RUNTIME="$test_root/runtime" \
  N42_QUAL_PORTS='28501 28502' \
  N42_QUAL_RUST_PORT=28502 \
  N42_QUAL_MAX_LAG=1 \
  N42_QUAL_LAG_CONFIRMATION_ATTEMPTS=3 \
  N42_QUAL_LAG_CONFIRMATION_DELAY_SECONDS=0 \
  "$repo/scripts/gov5-interop-qualification.sh" \
  monitor-heads 0 0 "$persistent"; then
  echo 'persistent lag unexpectedly passed' >&2
  exit 1
fi
jq -e '.ok==false and .error=="execution lag exceeded bound" and .lag==2 and
  .latestSnapshotAttempts==3 and .latestSnapshotConcurrent==true' \
  "$persistent" >/dev/null

jq -nc --arg at "$(date -u +%FT%TZ)" \
  '{at:$at,event:"head_monitor_sampling_regression_test",status:"PASS",
    concurrentLatestSampling:true,transientSamplingSkewRetried:true,
    persistentLagRejected:true}'
