# Kosh Rust Service Quick Reference

This repository keeps the authoritative code; long-form documentation lives in the
`kosh_docs` repository. The notes below capture the essentials so you can work in this tree
without hunting across repos.

## Bring-Up & Artefacts

- Build/bake everything locally:
  ```bash
  docker compose build kosh-rust
  ```
- Run Elasticsearch + the Rust API (hot reload):
  ```bash
  docker compose up kosh-rust
  ```
- Export OpenAPI / GraphQL bundles without starting the server:
  ```bash
  docker compose run --rm kosh-rust --dump-artifacts /tmp/out
  ```

## Configuration & Flags

- Config precedence mirrors Python: defaults → optional INI (`--config`) → inline `--config-text`
  fragments → env vars (`KOSH_*`) → CLI flags.
- Useful toggles:
  - `--data-bulk-batch-size <n>` / `KOSH_DATA_BULK_BATCH_SIZE=<n>` (defaults to `5000`)
  - Streaming parser is enabled by default; set `KOSH_DATA_STREAMING_PARSER=false` (or adjust the INI) to force the legacy DOM path.
  - `--data-parse-concurrency <n>` / `KOSH_DATA_PARSE_CONCURRENCY=<n>` to pin parser workers; omit
    to let kosh auto-detect (`cpu_limit - 1`, capped at 12).
  - `--dump-artifacts <dir>` to write OpenAPI + GraphQL bundles and exit.

## Streaming Parser

- The streaming parser keeps memory usage flat for simple XPath roots (`/items/item`, `//item`, etc.).
  When the XPath is complex it automatically falls back to the DOM/XPath path, so behaviour stays
  identical.
- Validate DOM vs streaming parity with the included example:
  ```bash
  cargo run --example streaming_parity -- /path/to/kosh_data
  ```

## Observability

The `/metrics` endpoint exposes Prometheus counters and histograms:

| Metric | Purpose |
| --- | --- |
| `kosh_http_requests_total` | HTTP response census |
| `kosh_search_failures_total` | Elasticsearch read failures |
| `kosh_rebuild_outcomes_total` / `_retries_total` | Rebuild success/failure counts |
| `kosh_rebuild_latency_seconds` | Rebuild duration histogram |
| `kosh_parser_duration_seconds` | Time spent parsing XML per rebuild |
| `kosh_bulk_latency_seconds` | Elasticsearch bulk ingestion latency |

The bundled Grafana provisioning (see `monitoring/grafana/`) wires these metrics and dashboards
when you run `docker compose up grafana`.

## Examples & Utilities

- Parity harness for DOM vs streaming: `cargo run --example streaming_parity`.
- Watcher/rescan logic is driven via `docker compose up kosh-rust`; the watcher health checks
  `/metrics` so instrumentation must come up for the container to stay healthy.

For fuller operational runbooks (deployments, alerting playbooks, release procedures), refer to
`kosh_docs/kosh-rust-ops-runbook.md` and the adjacent documents.
