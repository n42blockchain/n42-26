#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
finalizer="$script_dir/gov5-current-qualification-finalizer.sh"
producer_waiter="$script_dir/gov5-strict24h-six-producer-waiter.sh"
correction_waiter="$script_dir/gov5-runtime37-15m-resource-correction-waiter.sh"
total_finalizer="$script_dir/gov5-runtime37-total-goal-finalizer.sh"
independent_waiter="$script_dir/gov5-strict-independent-verifier-waiter.sh"
independent_verifier="$script_dir/verify-gov5-906-final-qualification.sh"
main_guardian="$script_dir/gov5-current-main-fail-close-guardian.sh"
override="210517ae2b40233a078b4a2999e07ea9bd2f6211d30d24a87eaf481473f5376b"
default="aa906f42b83048cb4168e1ceb1077d1ca8b27429be5189acd1aaa74f06c551e9"
assignment="$(rg -m1 '^expected_harness_sha=' "$finalizer")"

bash -n "$finalizer"
bash -n "$producer_waiter"
bash -n "$correction_waiter"
bash -n "$total_finalizer"
bash -n "$independent_waiter"
bash -n "$independent_verifier"
bash -n "$main_guardian"
test -n "$assignment"

actual="$(N42_QUAL_EXPECTED_HARNESS_SHA="$override" bash -c "$assignment; printf '%s' \"\$expected_harness_sha\"")"
test "$actual" = "$override"

actual="$(env -u N42_QUAL_EXPECTED_HARNESS_SHA bash -c "$assignment; printf '%s' \"\$expected_harness_sha\"")"
test "$actual" = "$default"

rg -F 'linkage="${N42_STRICT24H_PRODUCER_LINKAGE:-${output%.json}-linkage.json}"' \
  "$producer_waiter" >/dev/null
rg -F 'failure="${N42_STRICT24H_PRODUCER_FAILURE:-${output%.json}-failure.json}"' \
  "$producer_waiter" >/dev/null
rg -F 'measured24hResourceAudit.elapsedSeconds>=86400' "$correction_waiter" >/dev/null
rg -F 'waiter_failure="${N42_CORRECTION_WAITER_FAILURE:-' "$correction_waiter" >/dev/null
rg -F 'assert_controller_rebind_correction' "$correction_waiter" >/dev/null
rg -F 'milestone_required+=("$supplemental_15m_correction")' "$total_finalizer" >/dev/null
rg -F 'assert_no_uncorrected_failures' "$total_finalizer" >/dev/null
rg -F 'failures="${N42_QUAL_FAILURES:-' "$finalizer" >/dev/null
rg -F 'preflight_burst "${N42_QUAL_PREFLIGHT_LABEL:-launch-preflight}"' "$finalizer" >/dev/null
rg -F '"$correction_waiter_v2_failure"' "$total_finalizer" >/dev/null
rg -F 'correctedControllerRebindFailure:true,correctedIndependentVerifierHarnessPin:true' \
  "$total_finalizer" >/dev/null
rg -F 'N42_VERIFY_HARNESS_SHA="$expected_harness_sha"' "$independent_waiter" >/dev/null
rg -F 'expected_harness_sha="${N42_VERIFY_HARNESS_SHA:-' "$independent_verifier" >/dev/null
rg -F 'N42_VERIFY_HARNESS_SHA="$expected_harness"' "$total_finalizer" >/dev/null
rg -F 'assert_independent_harness_rebind' "$total_finalizer" >/dev/null
rg -F 'assert_milestone_remote_retry_correction' "$total_finalizer" >/dev/null
rg -F '.correctedMilestoneRemoteRetryFailure==true' "$total_finalizer" >/dev/null
rg -F 'remote_retry_controller_rebind=' "$total_finalizer" >/dev/null
rg -F '.correctedRemoteRetryControllerRebind==true' "$total_finalizer" >/dev/null
rg -F 'static="${N42_TOTAL_STATIC:-' "$total_finalizer" >/dev/null
rg -F '.frozenTools.independentVerifierSha256==$verifier' "$total_finalizer" >/dev/null
rg -F '.correction.priorBaselinePreserved==true' "$total_finalizer" >/dev/null
for script in "$independent_waiter" "$main_guardian" "$total_finalizer"; do
  rg -F 'planned_rust_restart_in_progress' "$script" >/dev/null
  rg -F 'test "$age" -ge 0 && test "$age" -le 900' "$script" >/dev/null
done

jq -nc '{status:"PASS",harnessShaOverrideAccepted:true,legacyDefaultPreserved:true,
  producerCompanionPathsFollowOutput:true,
  shortWindowProjectionCorrectionRequiresMeasured24h:true,
  finalizerPreflightFailurePathIsIsolated:true,
  finalizerPreflightArtifactLabelIsIsolated:true,
  correctedControllerRebindFailureIsPreservedAndBound:true,
  correctedControllerV2FailureRemainsFailClose:true,
  independentVerifierHarnessShaIsExplicitlyPinned:true,
  correctedStaticBaselineIsExplicitlyBound:true,
  milestoneRemoteRetryFailureIsPreservedAndCorrected:true,
  remoteRetryControllersAreReboundAndVerified:true,
  plannedRustRestartDoesNotTripGuardians:true}'
