# Phase 1: read-only node/CI shadow gateway

This describes the earlier Jev-first prototype. The local-first, eight-class
Rules / GLiClass / Jev benchmark is specified in `local-system1-roadmap.md` and
implemented separately by `scripts/decision_benchmark.py` and
`scripts/run_decision_provider.py`. Use the new eight-class corpus for the
current Go/No-Go decision.

`scripts/decision_shadow.py` accepts one sanitized event per JSONL line and
records the deterministic rule result beside a shadow Jev classification. It
cannot write node configuration, call an N42 RPC method, sign, submit, restart
or ban anything. It does not alter an existing alert. Critical deterministic
alerts cannot be downgraded by model output. API errors and malformed answers
route to deep analysis in the recorded shadow result.

Input example:

```json
{"id":"ci-102","source":"ci","text":"execution test failed","observed_at_ms":1790445000000,"truth":"DEGRADED","truth_domain":"EXECUTION"}
```

Allowed sources: `node`, `ci`, `rpc`, `network`, `benchmark`, `configuration`,
`peerdas`, `block_stm`, `qmdb`.
`truth` and `truth_domain` are optional adjudicated labels used only for
measurement. IDs and event text have size limits; obvious key/token patterns
are removed before model calls, but operators must sanitize logs before
creating the input file. The output contains no event text.

```sh
python3 scripts/collect_decision_events.py node.log \
  --source node --prefix node0-window1 --output events.jsonl
python3 scripts/decision_shadow.py events.jsonl \
  --output shadow.jsonl --metrics metrics.json
TYPESAFE_API_KEY=... python3 scripts/decision_shadow.py events.jsonl \
  --output shadow-jev.jsonl --metrics metrics-jev.json --jev
python3 -m unittest discover -s scripts -p test_decision_shadow.py -v
```

Outputs must be new paths so an experiment cannot silently overwrite prior
evidence. The collector is an offline snapshot converter, not a daemon: feed
CI logs, benchmark summaries or node logs with the matching `--source` and
unique `--prefix`. It redacts common key/token patterns and truncates long
lines, but operators must inspect and sanitize the source for other secrets
before sending events to a model. The Jev call uses the pinned `jev-1.13.0` template; an alternate
endpoint is accepted only via HTTPS or loopback HTTP for tests. The script
does not infer token use from text. Cost per 10,000 events remains `null`
until actual metered token counts and an explicit current price are supplied
to the scoring function. No production routing or subsequent roadmap phase
is authorized by a passing synthetic test.

For the Phase 1 gate, label a real representative corpus and report severe
recall, false-positive rate, domain accuracy, latency, escalation rate and
measured spend for rules-only and shadow runs. Require no added severe misses
and a material reduction in human triage or System-2 calls at acceptable cost.
