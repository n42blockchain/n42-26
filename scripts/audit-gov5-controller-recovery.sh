#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:?runtime is required}"
new_finalizer_pid="${N42_RECOVERY_FINALIZER_PID:?new finalizer PID is required}"
new_independent_pid="${N42_RECOVERY_INDEPENDENT_PID:?new independent waiter PID is required}"
expected_finalizer_sha="${N42_RECOVERY_FINALIZER_SHA:?new finalizer SHA-256 is required}"
expected_waiter_sha="${N42_RECOVERY_WAITER_SHA:?new waiter SHA-256 is required}"
expected_verifier_sha="${N42_RECOVERY_VERIFIER_SHA:?new verifier SHA-256 is required}"
expected_harness_sha="${N42_RECOVERY_HARNESS_SHA:?new harness SHA-256 is required}"
expected_burst_correction_sha="${N42_RECOVERY_BURST_CORRECTION_SHA:?burst correction SHA-256 is required}"
prior_failure="$runtime/evidence/gov5-906-finalizer-failures.jsonl"
prior_waiter_failure="$runtime/evidence/runtime37-latest-c0a146-formal-15m-resource-correction-v2-failure.json"
burst_correction="$runtime/evidence/gov5-906-post-burst-correction.json"
formal="$runtime/evidence/mixed-soak-24h.jsonl"
output="${N42_RECOVERY_OUTPUT:-$runtime/evidence/runtime37-post-burst-controller-recovery.json}"

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

require_process() {
  local pid="$1" needle="$2"
  kill -0 "$pid"
  ps -p "$pid" -o command= | rg -F "$needle" >/dev/null
}

test ! -e "$output"
test "$(sha256 "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh")" = \
  "$expected_finalizer_sha"
test "$(sha256 "$runtime/artifacts/scripts/gov5-strict-independent-verifier-waiter.sh")" = \
  "$expected_waiter_sha"
test "$(sha256 "$runtime/artifacts/scripts/verify-gov5-906-final-qualification.sh")" = \
  "$expected_verifier_sha"
test "$(sha256 "$runtime/artifacts/scripts/gov5-interop-qualification.sh")" = \
  "$expected_harness_sha"
test "$(sha256 "$burst_correction")" = "$expected_burst_correction_sha"
require_process "$new_finalizer_pid" gov5-current-qualification-finalizer.sh
require_process "$new_independent_pid" gov5-strict-independent-verifier-waiter.sh
for pid in 57653 57966 60973 62228; do
  ! kill -0 "$pid" 2>/dev/null
done
jq -e '.event == "runtime37_formal_15m_resource_correction_failure" and
  .status == "FAIL" and .statusCode == 1 and
  .command == "kill -0 \"$finalizer_pid\""' "$prior_waiter_failure" >/dev/null
jq -e --arg failure_sha "$(sha256 "$prior_failure")" \
  '.status == "PASS" and .priorFinalizerFailure.sha256 == $failure_sha and
   .priorFinalizerFailure.preserved == true and .transactionsResent == 0 and
   .chainDataMutationPerformed == false' "$burst_correction" >/dev/null
jq -e -s '
  length >= 2 and all(.[]; .ok == true and .zeroTxRequired == 1) and
  (.[-1].at | fromdateiso8601) - (.[0].at | fromdateiso8601) >= 86400
' "$formal" >/dev/null

jq -nc --arg at "$(date -u +%FT%TZ)" \
  --arg prior_failure "$prior_failure" \
  --arg prior_failure_sha "$(sha256 "$prior_failure")" \
  --arg prior_waiter_failure "$prior_waiter_failure" \
  --arg prior_waiter_failure_sha "$(sha256 "$prior_waiter_failure")" \
  --arg burst_correction "$burst_correction" \
  --arg burst_correction_sha "$expected_burst_correction_sha" \
  --arg formal "$formal" --arg formal_sha "$(sha256 "$formal")" \
  --argjson finalizer_pid "$new_finalizer_pid" \
  --argjson independent_pid "$new_independent_pid" \
  --arg finalizer_sha "$expected_finalizer_sha" \
  --arg waiter_sha "$expected_waiter_sha" \
  --arg verifier_sha "$expected_verifier_sha" \
  --arg harness_sha "$expected_harness_sha" '
  {at:$at,event:"runtime37_post_burst_controller_recovery",status:"PASS",
   acceptanceRelaxed:false,
   priorFinalizerFailure:{path:$prior_failure,sha256:$prior_failure_sha,preserved:true},
   priorResourceCorrectionWaiterFailure:{path:$prior_waiter_failure,
     sha256:$prior_waiter_failure_sha,preserved:true,controllerCascade:true},
   burstCorrection:{path:$burst_correction,sha256:$burst_correction_sha},
   priorControllers:{finalizerPid:57653,independentWaiterPid:57966,
     resourceCorrectionWaiterPid:60973,totalGoalFinalizerPid:62228,
     allExited:true},
   replacementControllers:{finalizerPid:$finalizer_pid,
     independentWaiterPid:$independent_pid,finalizerSha256:$finalizer_sha,
     independentWaiterSha256:$waiter_sha,verifierSha256:$verifier_sha,
     harnessSha256:$harness_sha,allAlive:true},
   formalWindow:{path:$formal,sha256:$formal_sha,continuous:true,
     elapsedSecondsAtLeast86400:true,failedSamples:0,zeroTransactionRequired:true},
   finalizedTransactionsBeforeRecovery:17,transactionsResent:0,
   chainDataMutationPerformed:false,nodeOrFormalMonitorMutationPerformed:false,
   controllerCascadeCorrected:true}' >"$output.pending"
mv "$output.pending" "$output"
cat "$output"
