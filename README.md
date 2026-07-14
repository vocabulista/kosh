# Kosh – APIs for Lexical Data

Kosh delivers versioned REST and GraphQL endpoints for historical dictionaries and other
lexicographic datasets curated by the University of Cologne. Explore the public service at
<https://kosh.uni-koeln.de> or browse the deployed collections via <https://dicts.uni-koeln.de>.

## What Is Kosh?

At its core, Kosh ingests TEI/XML corpora, indexes them in Elasticsearch, and exposes consistent
query surfaces for downstream applications (dictionary portals, editorial tooling, data exports).
This Rust workspace provides the production-grade implementation used in Cologne’s reference
deployment; Python clients and other integrations consume the same API contract.

## Rust Service Overview

The workspace is split into three crates so platform teams can swap pieces without breaking the API:

- `kosh-core` – shared business logic for parsing TEI/XML, building indices, and coordinating bulk
  loads into Elasticsearch.
- `kosh-api` – Axum + Async-GraphQL HTTP layer serving REST endpoints, GraphQL schemas, and
  Prometheus metrics from a single binary.
- `kosh-cli` – operational entrypoint; packaged as `/usr/local/bin/kosh` inside the Docker image and
  responsible for configuration parsing, hot-reload loops, and CLI tooling.

Operational guidance for these crates lives in two places:

- `KOSH_RUST_GUIDE.md` – quick reference covering local bring-up, flags, and metrics.
- `kosh_docs/kosh-rust-ops-runbook.md` – long-form runbook, alerts, and migration notes (external
  repo).

## Quick Start (Docker Compose)

Provision Elasticsearch, the Rust API, Prometheus, and Grafana in one shot:

```bash
docker compose build kosh-rust
docker compose up -d
```

Key expectations for the default stack:

- Source data: `kosh-rust` mounts `../kosh_data` (relative to the repo root) read-only at
  `/var/lib/kosh`. Populate this directory with TEI/XML sources and `.kosh` mapping files before
  starting the stack.
- Endpoints: the API is exposed on `http://localhost:5001`, Prometheus on `http://localhost:9090`,
  and Grafana on `http://localhost:3000` with the bundled dashboard.
- Remote Elasticsearch: point the service at a managed cluster by setting `KOSH_DATA_HOST` in the
  Compose file or via the CLI:

  ```bash
  docker compose run --rm kosh-rust \
    --data-host https://search.example.com:9243 \
    --config /etc/kosh/kosh.ini
  ```

- GraphQL explorer: browse any lexicon locally via GraphiQL at
  `http://localhost:5001/api/<lexicon_uid>/graphql`; the static assets ship with the API container,
  so the IDE works without external CDN access.

For production deployments you can `docker build -f Dockerfile.rust` and run the resulting image in
your orchestrator, passing flags such as `--data-host` and `--data-parse-concurrency` directly to the
`kosh` binary.

## Configuration Essentials

Configuration precedence mirrors the legacy Python stack: built-in defaults → optional INI
(`--config` or `--config-text`) → environment variables (`KOSH_*`) → explicit CLI flags. The sample
`kosh.ini` documents every knob. Highlights:

- The CLI autoloads `kosh.ini` when present (current directory, `/app/kosh.ini`, or
  `/etc/kosh/kosh.ini`); override with `--config` or `KOSH_CONFIG` if you need a different file.

- `[api]` – host, port, and root path; defaults to `0.0.0.0:5000` with `/` as the context.
- `[data]` – Elasticsearch endpoint (`host`), retry policy (`loop`), index namespace (`pool`), XML
  roots (`root`), mapping filename (`spec`), and watcher cadence (`sync` seconds).
- `[info]` – metadata exposed via `/info` for dashboards or external status pages.
- `[logger]` – sink (`file`) and verbosity (`level`).
- `[namespaces]` / `[query_types]` – opt-in namespaces for TEI parsing and Elasticsearch query types
  you want to expose (e.g., enable `wildcard` only for trusted clients).

Common overrides can be applied at runtime without touching the INI:

```bash
docker compose run --rm kosh-rust \
  --config /etc/kosh/kosh.ini \
  --data-parse-concurrency 8 \
  --data-bulk-batch-size 2500 \
  --dump-artifacts /tmp/kosh-schemas
```

Refer to `KOSH_RUST_GUIDE.md` for the exhaustive list of CLI flags and environment variables.

## Observability & Ops

- Metrics: `/metrics` publishes the Prometheus registry (`kosh_http_requests_total`,
  `kosh_rebuild_latency_seconds`, `kosh_parser_duration_seconds`, etc.). The container health check
  depends on a successful metrics scrape.
- Dashboards: `monitoring/prometheus.yml` configures scraping, and Grafana auto-loads dashboards and
  data sources from `monitoring/grafana/` (including `monitoring/grafana/dashboards/kosh-metrics-overview.json`).
- Artefacts: run `docker compose run --rm kosh-rust --dump-artifacts /tmp/out` to export the OpenAPI
  and GraphQL bundles used by downstream clients and documentation.

For release coordination, escalation paths, and detailed operational playbooks, see the external
documentation repository alongside the public deployment notes on <https://kosh.uni-koeln.de>.
