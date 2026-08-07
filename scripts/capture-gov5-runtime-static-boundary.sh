#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME COPIED_DATA_MANIFEST OUTPUT}"
copied_manifest="${2:?usage: $0 RUNTIME COPIED_DATA_MANIFEST OUTPUT}"
output="${3:?usage: $0 RUNTIME COPIED_DATA_MANIFEST OUTPUT}"

runtime="$(cd "$runtime" && pwd -P)"
copied_manifest="$(realpath "$copied_manifest")"
test -s "$copied_manifest"
test ! -e "$output"
[[ "$copied_manifest" == "$runtime/"* ]]
copied_manifest_relative="${copied_manifest#"$runtime/"}"

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

test "$(jq -er '.status' "$copied_manifest")" = PASS
test "$(jq -er '.allPathsSizesAndHashesExact' "$copied_manifest")" = true
copied_entries_sha="$(jq -er \
  '.recomputedEntriesSha256 //
   (select(.sourceManifestSha256 == .targetManifestSha256) |
    .targetManifestSha256)' "$copied_manifest")"

static_rows="$(mktemp)"
temporary=""
trap 'rm -f "$static_rows" "$temporary"' EXIT
while IFS=$'\t' read -r path expected; do
  assert_hash "$path" "$expected" >>"$static_rows"
done < <(jq -r '
  .entries[] |
  select(.path | test("^gov/node[1-6]/(epoch_schedule\\.json|network-keys|network\\.json|keystore/bls_[^/]+\\.key)$")) |
  [.path,.sha256] | @tsv' "$copied_manifest")
test "$(wc -l <"$static_rows" | tr -d '[:space:]')" = 24

genesis="$(sha256 "$runtime/artifacts/genesis.json")"
consensus="$(sha256 "$runtime/artifacts/consensus-peer-bound.json")"
bootstrap="$(sha256 "$runtime/artifacts/bootstrap-bundle.json")"
validator="$(sha256 "$runtime/artifacts/validator-keys/node0/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key")"
p2p="$(sha256 "$runtime/artifacts/validator-keys/node0/network-keys")"
harness="$(sha256 "$runtime/artifacts/scripts/gov5-interop-qualification.sh")"
finalizer="$(sha256 "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh")"
independent="$(sha256 "$runtime/artifacts/scripts/verify-gov5-906-final-qualification.sh")"
qmdb="$(sha256 "$runtime/artifacts/binaries/n42-qmdb-proof-verify")"
total="$(sha256 "$runtime/artifacts/scripts/gov5-current-total-goal-verifier.sh")"
gov_binary="$(sha256 "$runtime/geth-live")"
rust_binary="$(sha256 "$runtime/n42-node")"

temporary="$(mktemp "$runtime/evidence/.static-boundary-baseline.XXXXXX")"
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg copied_manifest_path "$copied_manifest_relative" \
  --arg copied_manifest_sha "$(sha256 "$copied_manifest")" \
  --arg copied_entries_sha "$copied_entries_sha" \
  --arg genesis "$genesis" --arg consensus "$consensus" \
  --arg bootstrap "$bootstrap" --arg validator "$validator" --arg p2p "$p2p" \
  --arg harness "$harness" --arg finalizer "$finalizer" \
  --arg independent "$independent" --arg qmdb "$qmdb" --arg total "$total" \
  --arg gov_binary "$gov_binary" --arg rust_binary "$rust_binary" \
  --slurpfile files "$static_rows" '
  {at:$at,event:"gov5_runtime_static_boundary_baseline",status:"PASS",
   mutationPerformed:false,
   copiedData:{manifestPath:$copied_manifest_path,
     evidenceSha256:$copied_manifest_sha,entriesSha256:$copied_entries_sha,
     initialSourceAndTargetExact:true},
   staticGov5Data:{filesChecked:($files|length),allCurrentHashesMatchInitialCopy:true,
     files:$files},
   artifacts:{genesisSha256:$genesis,consensusConfigSha256:$consensus,
     bootstrapBundleSha256:$bootstrap,validatorKeySha256:$validator,p2pKeySha256:$p2p},
   frozenTools:{harnessSha256:$harness,finalizerSha256:$finalizer,
     independentVerifierSha256:$independent,qmdbProofVerifierSha256:$qmdb,
     totalGoalVerifierSha256:$total},
   binaries:{gov5Sha256:$gov_binary,rustSha256:$rust_binary},
   runningChaindataExcludedBecauseExpectedToAdvance:true}' >"$temporary"
mv "$temporary" "$output"
temporary=""
trap - EXIT
rm -f "$static_rows"
shasum -a 256 "$output"
