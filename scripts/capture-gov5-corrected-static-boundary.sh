#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_STATIC_RUNTIME:?runtime is required}"
prior="${N42_STATIC_PRIOR:?prior static boundary is required}"
burst_correction="${N42_STATIC_BURST_CORRECTION:?burst correction is required}"
controller_recovery="${N42_STATIC_CONTROLLER_RECOVERY:?controller recovery is required}"
output="${N42_STATIC_OUTPUT:?output is required}"
test ! -e "$output"

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

test "$(jq -er '.status' "$prior")" = PASS
test "$(jq -er '.staticGov5Data.filesChecked' "$prior")" = 24
test "$(jq -er '.status' "$burst_correction")" = PASS
test "$(jq -er '.status' "$controller_recovery")" = PASS
rows="$(mktemp)"
trap 'rm -f "$rows"' EXIT
while IFS=$'\t' read -r path expected; do
  actual="$(sha256 "$runtime/$path")"
  test "$actual" = "$expected"
  jq -nc --arg path "$path" --arg expected "$expected" --arg actual "$actual" \
    '{path:$path,expectedSha256:$expected,currentSha256:$actual,exact:true}' >>"$rows"
done < <(jq -r '.staticGov5Data.files[] | [.path,.expectedSha256] | @tsv' "$prior")
test "$(wc -l <"$rows" | tr -d '[:space:]')" = 24

copied_rel="$(jq -er '.copiedData.manifestPath' "$prior")"
copied="$runtime/$copied_rel"
test "$(sha256 "$copied")" = "$(jq -er '.copiedData.evidenceSha256' "$prior")"
jq -e '.status == "PASS" and .files == 141 and (.entries|length) == 141 and
  .allPathsSizesAndHashesExact == true and .sourceManifestSha256 == .targetManifestSha256' \
  "$copied" >/dev/null

for spec in \
  'artifacts/genesis.json artifacts.genesisSha256' \
  'artifacts/consensus-peer-bound.json artifacts.consensusConfigSha256' \
  'artifacts/bootstrap-bundle.json artifacts.bootstrapBundleSha256' \
  'artifacts/validator-keys/node0/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key artifacts.validatorKeySha256' \
  'artifacts/validator-keys/node0/network-keys artifacts.p2pKeySha256' \
  'geth-live binaries.gov5Sha256' \
  'n42-node binaries.rustSha256'; do
  path="${spec%% *}"; field="${spec#* }"
  test "$(sha256 "$runtime/$path")" = "$(jq -er --arg field "$field" 'getpath($field|split("."))' "$prior")"
done

jq -nc --arg at "$(date -u +%FT%TZ)" \
  --arg prior "$(realpath "$prior")" --arg prior_sha "$(sha256 "$prior")" \
  --arg copied_rel "$copied_rel" --arg copied_sha "$(sha256 "$copied")" \
  --arg copied_entries "$(jq -er '.copiedData.entriesSha256' "$prior")" \
  --arg genesis "$(sha256 "$runtime/artifacts/genesis.json")" \
  --arg consensus "$(sha256 "$runtime/artifacts/consensus-peer-bound.json")" \
  --arg bootstrap "$(sha256 "$runtime/artifacts/bootstrap-bundle.json")" \
  --arg validator "$(sha256 "$runtime/artifacts/validator-keys/node0/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key")" \
  --arg p2p "$(sha256 "$runtime/artifacts/validator-keys/node0/network-keys")" \
  --arg harness "$(sha256 "$runtime/artifacts/scripts/gov5-interop-qualification.sh")" \
  --arg finalizer "$(sha256 "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh")" \
  --arg verifier "$(sha256 "$runtime/artifacts/scripts/verify-gov5-906-final-qualification.sh")" \
  --arg waiter "$(sha256 "$runtime/artifacts/scripts/gov5-strict-independent-verifier-waiter.sh")" \
  --arg total "$(sha256 "$runtime/artifacts/scripts/gov5-runtime37-corrected-goal-finalizer.sh")" \
  --arg qmdb "$(sha256 "$runtime/artifacts/binaries/n42-qmdb-proof-verify")" \
  --arg gov "$(sha256 "$runtime/geth-live")" --arg rust "$(sha256 "$runtime/n42-node")" \
  --arg correction "$(realpath "$burst_correction")" --arg correction_sha "$(sha256 "$burst_correction")" \
  --arg controllers "$(realpath "$controller_recovery")" --arg controllers_sha "$(sha256 "$controller_recovery")" \
  --slurpfile files "$rows" '
  {at:$at,event:"runtime37_corrected_static_boundary",status:"PASS",acceptanceRelaxed:false,
   priorBoundary:{path:$prior,sha256:$prior_sha,preserved:true},
   staticGov5Data:{filesChecked:($files|length),allCurrentHashesMatchInitialCopy:true,files:$files},
   copiedData:{manifestPath:$copied_rel,evidenceSha256:$copied_sha,entriesSha256:$copied_entries,
     initialSourceAndTargetExact:true,dataRecopyOrRegenerationRequired:false},
   artifacts:{genesisSha256:$genesis,consensusConfigSha256:$consensus,
     bootstrapBundleSha256:$bootstrap,validatorKeySha256:$validator,p2pKeySha256:$p2p},
   frozenTools:{harnessSha256:$harness,finalizerSha256:$finalizer,
     independentVerifierSha256:$verifier,independentWaiterSha256:$waiter,
     totalGoalVerifierSha256:$total,qmdbProofVerifierSha256:$qmdb},
   binaries:{gov5Sha256:$gov,rustSha256:$rust},
   correction:{burstEvidence:$correction,burstEvidenceSha256:$correction_sha,
     controllerRecovery:$controllers,controllerRecoverySha256:$controllers_sha,
     priorFailurePreserved:true,transactionsResent:0,
     chainDataMutationPerformed:false,nodeOrFormalMonitorMutationPerformed:false},
   genesisHash:"0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec",
   copied905Boundary:{height:92605,hash:"0xb88a3571223cf8cd8291d608572a55f306ea88957cc7ede8ab6b8812ada85a82"},
   securityBoundary:{height:99895,hash:"0x7ccd33002b040389eb0627fca27ef361e330234f85091b016c5e3c4256332407"}}
  ' >"$output.pending"
mv "$output.pending" "$output"
cat "$output"
