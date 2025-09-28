# Current Python Architecture Overview

## Runtime Entry & Lifecycle
- Entrypoint `kosh/kosh.py` configures logging, loads defaults, applies CLI parameter modules, and installs API modules dynamically.
- Establishes Elasticsearch connections, loads lexicon definitions from filesystem, rebuilds indices, and starts Flask service.
- Triggers filesystem watcher via `inotify` when `data.sync > 0`; otherwise blocks on process signals.

## API Layer (`kosh/api`)
- `_api.py` provides shared base with common Flask registration helpers.
- `restful.py` exposes `/entries`, `/ids`, `/spec` endpoints per lexicon and serves Swagger UI.
- `graphql.py` defines Graphene schema/resolvers mirroring search functionality via GraphQL.

## Elasticsearch Integration (`kosh/elastic`)
- `index.py` handles lexicon discovery, schema parsing, index lifecycle, and bulk ingestion via `elasticsearch.helpers`.
- `entry.py` parses XML dictionary files into structured documents; `search.py` builds queries covering query types defined in config.
- `index.notify` registers `inotify` watcher yielding lexicon updates for reindexing.

## Configuration & Utilities (`kosh/utility`)
- `defaultconfig.py` stores global defaults for API/data/logger/query type settings.
- `dotdictionary.py` and `instance.py` supply global state container shared across modules.
- `logger.py` wraps standard logging defaults; `concretemethod.py` enforces subclass overrides for API/param modules.

## CLI Parameter Modules (`kosh/param`)
- Each CLI flag lives in its own module (e.g., `data_root`, `api_port`, `log_level`).
- Flags mutate `instance.config` to override defaults prior to service startup.

## Deployment Artifacts
- `pyproject.toml` lists runtime dependencies (Flask, Graphene, Elasticsearch DSL, inotify, lxml).
- `Dockerfile` builds Python runtime image; Compose files orchestrate Kosh with Elasticsearch and supporting services.
- `kosh.ini` supplies production-ready overrides for defaults.

## Parity Targets for Rust Reimplementation
- Maintain CLI surface (`kosh` command) and config semantics (`kosh.ini`, CLI overrides).
- Preserve REST/GraphQL interfaces and generated Swagger metadata.
- Replicate filesystem-driven lexicon ingestion and Elasticsearch index lifecycle.
- Match logging format/severity filters and graceful shutdown behaviour.
