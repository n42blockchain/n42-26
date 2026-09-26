# System-1 Phase 1 benchmark procedure

The current benchmark schema is eight-class operational fault classification:
`NORMAL`, `NETWORK`, `CONSENSUS`, `EXECUTION`, `STORAGE`, `CONFIGURATION`,
`PERFORMANCE`, `UNKNOWN`, plus `need_escalation` as `YES` or `NO`. This is separate
from the older Jev-first health/domain shadow prototype.

1. Convert finite node, CI, benchmark or subsystem log snapshots to JSONL:

   ```sh
   python3 scripts/collect_decision_events.py node.log \
     --source node --prefix incident-run1-node0 --output events.jsonl
   ```

   For a bounded review queue across several files, use
   `scripts/decision_candidates.py` with `SOURCE:PATH` inputs,
   `--per-bucket`, `--output`, `--manifest` and `--label-template`. It samples
   normal and keyword-abnormal lines separately with a fixed seed; these are
   **candidate** buckets, not truth labels. The template contains null labels
   until a person adjudicates them.

2. Adjudicate **without viewing provider predictions**. For every event ID,
   provide exactly one line in `labels.jsonl`:

   ```json
   {"id":"incident-run1-node0-1","truth":"NETWORK","need_escalation":"YES","incident_id":"incident-run1"}
   ```

   Use `incident_id` to keep repeated lines from the same fault together in
   future train/calibration/test splits. Include normal operation and genuine
   unknowns. Do not invent labels for an unlabeled log dump.

3. Join the exact event and label sets, then run each provider against the
   same frozen corpus:

   ```sh
   python3 scripts/decision_corpus.py events.jsonl labels.jsonl --output corpus.jsonl
   python3 scripts/run_decision_provider.py rules corpus.jsonl \
     --output rules.jsonl --metadata rules-meta.json
   python3 scripts/decision_benchmark.py score corpus.jsonl \
     --predictions rules.jsonl --metadata rules-meta.json --output rules-score.json
   ```

   The local GLiClass run uses a **pre-downloaded local model directory**:

   ```sh
   python3 scripts/run_decision_provider.py gliclass corpus.jsonl \
     --model-dir /path/to/pinned/gliclass-model --warmup 10 \
     --output gliclass.jsonl --metadata gliclass-meta.json
   ```

   Only an explicitly requested Jev benchmark uses `TYPESAFE_API_KEY`:

   ```sh
   TYPESAFE_API_KEY=... python3 scripts/run_decision_provider.py jev corpus.jsonl \
     --output jev.jsonl --metadata jev-meta.json
   ```

Every provider emits exactly one prediction per event. Failures are explicit
`UNKNOWN`/`YES` predictions and are counted in metadata; do not treat a run
with provider failures as a clean accuracy or QPS result. The runner measures
process wall time, CPU time and peak process RSS. Cost remains `null` until
metered provider charges or input tokens are supplied. Run CPU comparisons
under the shared quiet-hardware claim and preserve model artifact hashes,
thread count and host details alongside results. A raw GLiClass score is not a
calibrated probability of correctness. The default is **benchmark only**;
there is no production local-first routing decision until a labelled holdout
and calibration gate pass.

`scripts/decision_gateway.py` is a read-only composition prototype. It tries
the local provider first and asks Jev only for explicit `UNKNOWN` or local
failure. It records unresolved failures for escalation, and a critical rule
cannot be changed into `NORMAL` by either model. It has no chain, wallet or
node-control methods. Low raw model scores alone do not trigger fallback until
the Phase 1 corpus supports a calibrated threshold.

The [official GLiClass README](https://github.com/Knowledgator/GLiClass)
shows its local pipeline and CPU-compatible serving options. The adapter loads
only a local directory and never downloads a model at runtime. No GLiClass
package or model is bundled with this repository. Under the shared quiet-hardware
claim, `scripts/install-local-decision-models.sh` creates an isolated Python
environment in `.artifacts/decision-system1-models/.venv`, installs a CPU-only
PyTorch and GLiClass, downloads pinned GLiClass small v1.0 and ModernBERT-base
revisions, hashes files and runs local loading smoke tests. The script refuses
to run without both Codex claim files. Use the model path in its manifest for
`--model-dir`. ModernBERT-base is a masked-language-model checkpoint, not an
N42 fault classifier until it receives an independently trained head.
