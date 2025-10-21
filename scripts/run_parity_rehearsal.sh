#!/usr/bin/env bash
set -euo pipefail

# Runs a parity rehearsal against a dataset directory by:
# 1. Resetting Elasticsearch indices.
# 2. Capturing artefacts from the Python service.
# 3. Capturing artefacts from the Rust service.
# 4. Diffing the results.
#
# Requirements:
# - Local Elasticsearch running at $ELASTIC_URL (default http://127.0.0.1:9200)
# - Python dependencies installed (pip install . requests)
# - Rust toolchain for building kosh-cli
#
# Usage:
#   scripts/run_parity_rehearsal.sh /path/to/data/root [output-dir]

DATA_ROOT=${1:-}
OUTPUT_DIR=${2:-parity/rehearsal}
if [[ -z "${DATA_ROOT}" ]]; then
  echo "Usage: $0 /path/to/data/root [output-dir]" >&2
  exit 1
fi

ELASTIC_URL=${ELASTIC_URL:-http://127.0.0.1:9200}
PYTHON_PORT=${PYTHON_PORT:-6200}
RUST_PORT=${RUST_PORT:-6201}
INDICES=${INDICES:-"kosh[demo] kosh[demo_alt]"}
PYTHON_OUT="${OUTPUT_DIR}/python"
RUST_OUT="${OUTPUT_DIR}/rust"

echo "[parity] Resetting indices at ${ELASTIC_URL}"
for index in ${INDICES}; do
  encoded=$(python -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],'') )" "${index}")
  curl -fsS -XDELETE "${ELASTIC_URL}/${encoded}" >/dev/null || true
done

echo "[parity] Capturing Python artefacts..."
python scripts/capture_service_artifacts.py \
  --output-dir "${PYTHON_OUT}" \
  --base-url "http://127.0.0.1:${PYTHON_PORT}" \
  --elastic-url "${ELASTIC_URL}" \
  --indices ${INDICES} \
  -- kosh \
    --data-root "${DATA_ROOT}" \
    --data-host "${ELASTIC_URL}" \
    --data-sync 0 \
    --api-port "${PYTHON_PORT}" \
    --log-level INFO

echo "[parity] Capturing Rust artefacts..."
python scripts/capture_service_artifacts.py \
  --output-dir "${RUST_OUT}" \
  --base-url "http://127.0.0.1:${RUST_PORT}" \
  --elastic-url "${ELASTIC_URL}" \
  --indices ${INDICES} \
  -- cargo run --quiet --manifest-path rust/Cargo.toml --package kosh-cli -- \
      --data-root "${DATA_ROOT}" \
      --data-host "${ELASTIC_URL}" \
      --data-sync 0 \
      --api-port "${RUST_PORT}" \
      --log-level INFO

echo "[parity] Diffing results..."
diff -ru "${PYTHON_OUT}" "${RUST_OUT}"
echo "[parity] Rehearsal completed."
