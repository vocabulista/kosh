# Rust Workspace Overview

## Crate Layout
- `kosh-core`: configuration loader, logging helper, lexicon discovery, and foundational domain types mirroring Python defaults.
- `kosh-api`: Axum-based HTTP shell with `/health` endpoint placeholder; supports graceful shutdown wiring.
- `kosh-cli`: binary crate (`kosh`) handling CLI parsing, config selection, logging initialisation, and service startup.

## Key Capabilities
- INI-driven config loader (`KoshConfig::from_optional_file`) keeps existing defaults and allows overrides.
- Logging initialised via `tracing`/`tracing-subscriber` with configurable log levels.
- Lexicon parser (`lexicon::discover_lexica`) translates `.kosh` spec files into typed descriptors for indexing.
- XPath metadata reader (`index::parser::XmlEntryParser`) now evaluates schema `_xpaths`, generating `IndexDocument` batches with serialized XML payloads, `created` timestamps, and SHA1 fallbacks matching the Python workflow.
- Indexing facade (`index::IndexService`) models create/rebuild/append flows and now drives the real Elasticsearch backend during CLI bootstrap.
- Elasticsearch backend (`index::elasticsearch::ElasticsearchBackend`) performs index lifecycle + bulk ingestion via `reqwest` and exposes a health check used during CLI bootstrap; TODOs remain for analyzer parity edge cases, retry/backoff, custom CA bundles, and monitoring.
- HTTP server scaffolding ready for future REST/GraphQL routes; exposes health check for smoke tests.
- CLI surfaces `--config` and `--log-level` flags, awaiting parity expansion with legacy Python flags.

## Next Steps
- Align Elasticsearch analyzers/settings with production schema nuances and add retry/backoff plus health checks.
- Connect `IndexService` to filesystem watcher-driven ingestion and coordinate reindex triggers.
- Flesh out REST/GraphQL routers within `kosh-api`, aligning endpoints to existing Flask contract.
- Introduce integration tests validating end-to-end ingestion (watcher → parser → backend) and HTTP behaviour.
