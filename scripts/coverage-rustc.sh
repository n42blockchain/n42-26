#!/usr/bin/env bash
# Cargo invokes this wrapper only for workspace members, leaving dependencies alone.
set -euo pipefail
exec "$@" -C instrument-coverage -C codegen-units=1
