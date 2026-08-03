#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME BASELINE OUTPUT}"
baseline="${2:?usage: $0 RUNTIME BASELINE OUTPUT}"
output="${3:?usage: $0 RUNTIME BASELINE OUTPUT}"

test -d "$runtime"
test -s "$baseline"
test ! -e "$output"

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

assert_hash() {
  local path="$1" expected="$2" actual
  test -f "$runtime/$path"
  actual="$(sha256 "$runtime/$path")"
  test "$actual" = "$expected"
  jq -nc --arg path "$path" --arg expected "$expected" --arg actual "$actual" \
    '{path:$path,expectedSha256:$expected,currentSha256:$actual,exact:true}'
}

test "$(jq -er '.status' "$baseline")" = PASS
test "$(jq -er '.staticGov5Data.filesChecked' "$baseline")" = 24

static_rows="$(mktemp)"
temporary=""
trap 'rm -f "$static_rows" "$temporary"' EXIT
while IFS=$'\t' read -r path expected; do
  assert_hash "$path" "$expected" >>"$static_rows"
done < <(jq -r '.staticGov5Data.files[] | [.path,.expectedSha256] | @tsv' "$baseline")
test "$(wc -l <"$static_rows" | tr -d '[:space:]')" = 24

genesis="$(jq -er '.artifacts.genesisSha256' "$baseline")"
consensus="$(jq -er '.artifacts.consensusConfigSha256' "$baseline")"
bootstrap="$(jq -er '.artifacts.bootstrapBundleSha256' "$baseline")"
validator="$(jq -er '.artifacts.validatorKeySha256' "$baseline")"
p2p="$(jq -er '.artifacts.p2pKeySha256' "$baseline")"
harness="$(jq -er '.frozenTools.harnessSha256' "$baseline")"
finalizer="$(jq -er '.frozenTools.finalizerSha256' "$baseline")"
independent="$(jq -er '.frozenTools.independentVerifierSha256' "$baseline")"
qmdb="$(jq -er '.frozenTools.qmdbProofVerifierSha256' "$baseline")"
total="$(jq -er '.frozenTools.totalGoalVerifierSha256' "$baseline")"
gov_binary="$(jq -er '.binaries.gov5Sha256' "$baseline")"
rust_binary="$(jq -er '.binaries.rustSha256' "$baseline")"

assert_hash artifacts/genesis.json "$genesis" >/dev/null
assert_hash artifacts/consensus-peer-bound.json "$consensus" >/dev/null
assert_hash artifacts/bootstrap-bundle.json "$bootstrap" >/dev/null
assert_hash artifacts/validator-keys/node0/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key "$validator" >/dev/null
assert_hash artifacts/validator-keys/node0/network-keys "$p2p" >/dev/null
assert_hash artifacts/scripts/gov5-interop-qualification.sh "$harness" >/dev/null
assert_hash artifacts/scripts/gov5-current-qualification-finalizer.sh "$finalizer" >/dev/null
assert_hash artifacts/scripts/verify-gov5-906-final-qualification.sh "$independent" >/dev/null
assert_hash artifacts/binaries/n42-qmdb-proof-verify "$qmdb" >/dev/null
assert_hash artifacts/scripts/gov5-current-total-goal-verifier.sh "$total" >/dev/null
assert_hash geth-live "$gov_binary" >/dev/null
assert_hash n42-node "$rust_binary" >/dev/null

copied_manifest="$runtime/evidence/runtime28-copied-chain-data-manifest.json"
copied_manifest_sha="$(jq -er '.copiedData.evidenceSha256' "$baseline")"
copied_entries_sha="$(jq -er '.copiedData.entriesSha256' "$baseline")"
test "$(sha256 "$copied_manifest")" = "$copied_manifest_sha"
test "$(jq -er '.status' "$copied_manifest")" = PASS
test "$(jq -er '.allPathsSizesAndHashesExact' "$copied_manifest")" = true
test "$(jq -er '.recomputedEntriesSha256' "$copied_manifest")" = "$copied_entries_sha"

temporary="$(mktemp "$runtime/evidence/.static-boundary.XXXXXX")"
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg baseline "$(realpath "$baseline")" \
  --arg baseline_sha "$(sha256 "$baseline")" \
  --arg copied_manifest_sha "$copied_manifest_sha" \
  --arg copied_entries_sha "$copied_entries_sha" \
  --arg genesis "$genesis" --arg consensus "$consensus" \
  --arg bootstrap "$bootstrap" --arg validator "$validator" --arg p2p "$p2p" \
  --arg harness "$harness" --arg finalizer "$finalizer" \
  --arg independent "$independent" --arg qmdb "$qmdb" --arg total "$total" \
  --arg gov_binary "$gov_binary" --arg rust_binary "$rust_binary" \
  --slurpfile files "$static_rows" '
  {at:$at,event:"gov5_runtime_static_boundary_recheck",status:"PASS",
   mutationPerformed:false,baselineEvidence:$baseline,baselineEvidenceSha256:$baseline_sha,
   copiedData:{manifestSha256:$copied_manifest_sha,entriesSha256:$copied_entries_sha,
     originalCopyExact:true,runningDataRehashExcludedBecauseExpectedToAdvance:true},
   staticGov5Data:{filesChecked:($files|length),allCurrentHashesMatchInitialCopy:true,
     files:$files},
   artifacts:{genesisSha256:$genesis,consensusConfigSha256:$consensus,
     bootstrapBundleSha256:$bootstrap,validatorKeySha256:$validator,p2pKeySha256:$p2p},
   frozenTools:{harnessSha256:$harness,finalizerSha256:$finalizer,
     independentVerifierSha256:$independent,qmdbProofVerifierSha256:$qmdb,
     totalGoalVerifierSha256:$total},
   binaries:{gov5Sha256:$gov_binary,rustSha256:$rust_binary}}
  ' >"$temporary"
mv "$temporary" "$output"
trap - EXIT
rm -f "$static_rows"
shasum -a 256 "$output"
