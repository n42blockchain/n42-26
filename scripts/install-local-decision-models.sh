#!/usr/bin/env bash
# Install pinned CPU GLiClass and ModernBERT artifacts under the shared claim.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Requires the shared Codex box claim on both directories" >&2
    exit 2
fi

export N42_MODEL_ROOT="${N42_MODEL_ROOT:-$PROJECT_DIR/.artifacts/decision-system1-models}"
export PYTHONPATH="$SCRIPT_DIR${PYTHONPATH:+:$PYTHONPATH}"
export N42_GL_REV=21edefaf7951f68c68c505f9139ba536d3b448f7
export N42_MB_REV=8949b909ec900327062f0ebf497f51aef5e6f0c8
VENV="$N42_MODEL_ROOT/.venv"
mkdir -p "$N42_MODEL_ROOT"
python3 -m venv "$VENV"
"$VENV/bin/python" -m pip install --index-url https://download.pytorch.org/whl/cpu 'torch==2.13.0+cpu'
"$VENV/bin/python" -m pip install 'gliclass==0.1.20' huggingface_hub

"$VENV/bin/python" - <<'PY'
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path

from huggingface_hub import snapshot_download

root = Path(os.environ['N42_MODEL_ROOT'])
sources = (
    ('knowledgator/gliclass-small-v1.0', os.environ['N42_GL_REV'], 'gliclass-small-v1.0'),
    ('answerdotai/ModernBERT-base', os.environ['N42_MB_REV'], 'modernbert-base'),
)
models = []
for repo_id, revision, name in sources:
    destination = root / f'{name}-{revision[:12]}'
    snapshot_download(
        repo_id=repo_id,
        revision=revision,
        local_dir=destination,
        allow_patterns=['config.json', 'model.safetensors', 'tokenizer.json',
                        'tokenizer_config.json', 'special_tokens_map.json',
                        'added_tokens.json', 'spm.model', 'vocab.txt'],
    )
    files = {}
    for path in sorted(destination.iterdir()):
        if path.is_file():
            digest = hashlib.sha256()
            with path.open('rb') as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b''):
                    digest.update(chunk)
            files[path.name] = {'bytes': path.stat().st_size, 'sha256': digest.hexdigest()}
    models.append({'repo_id': repo_id, 'revision': revision, 'path': str(destination), 'files': files})

from decision_providers import GliclassProvider
import torch
from transformers import AutoModelForMaskedLM, AutoTokenizer

torch.set_num_threads(4)
gliclass = GliclassProvider.from_local(models[0]['path'])
gliclass_result = gliclass.predict({'source': 'node', 'text': 'peer timeout while finality stalled'})
tokenizer = AutoTokenizer.from_pretrained(models[1]['path'], local_files_only=True)
modernbert = AutoModelForMaskedLM.from_pretrained(models[1]['path'], local_files_only=True)
modernbert.eval()
with torch.no_grad():
    logits = modernbert(**tokenizer('N42 peer timeout', return_tensors='pt')).logits
assert logits.shape[0] == 1 and logits.shape[1] > 0

manifest = {
    'python': os.sys.version.split()[0],
    'packages': {name: importlib.metadata.version(name) for name in ('torch', 'gliclass', 'transformers', 'huggingface_hub')},
    'models': models,
    'smoke': {'gliclass': gliclass_result, 'modernbert_logits_shape': list(logits.shape)},
}
with (root / 'manifest.json').open('x', encoding='utf-8') as output:
    json.dump(manifest, output, indent=2)
    output.write('\n')
print(json.dumps({'model_root': str(root), 'packages': manifest['packages'], 'smoke': manifest['smoke']}))
PY
