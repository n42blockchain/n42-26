"""Refresh the standalone harness from current production store code and selected tests."""
import argparse
import shutil
from pathlib import Path

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--repo", type=Path, required=True)
args = p.parse_args()
out = Path(__file__).resolve().parent
source_path = args.repo / "crates/n42-node/src/qmdb_state_root.rs"
source = source_path.read_text()
start = source.index("/// Maximum ancestry replay")
end = source.index("/// Reth 2.4.1 Engine Tree strategy")
imports = (out / "store.rs").read_text().split("/// Maximum ancestry replay")[0]

def function(name):
    pos = source.index("    fn " + name + "(")
    begin = source.index("{", pos)
    depth, end = 1, begin + 1
    # These selected functions have balanced braces in their format strings.
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[pos:end] + "\n"

helpers = ["operation", "store", "expected_root", "persistent_store"]
tests = [
    "prepared_candidate_adopts_only_the_complete_delta_and_checks_header_root",
    "prepared_sibling_parent_and_changed_operations_never_reuse_a_root",
    "prepared_candidate_is_invisible_to_historical_reads_and_capacity_fallback",
    "prepared_candidate_is_never_written_into_a_durable_base",
    "prepared_admission_wal_faults_rollback_and_retry_without_publication",
    "prepared_child_is_discarded_if_pending_parent_wal_fails",
]
body = imports + source[start:end] + "\n#[cfg(test)]\nmod tests {\nuse super::*;\n"
body += "".join(function(name) for name in helpers)
body += "".join("#[test]\n" + function(name) for name in tests)
body += '#[test]\n#[ignore="measurement"]\n' + function("bench_persistent_commit_path") + "}\n"
(out / "store.rs").write_text(body)
shutil.copy2(source_path, out / "optimized-full.rs")
shutil.copy2(args.repo / "crates/n42-node/src/exec_cache.rs", out / "exec_cache.rs")
shutil.copy2(args.repo / "crates/n42-node/src/qmdb_read_view.rs", out / "qmdb-read-view.rs")
