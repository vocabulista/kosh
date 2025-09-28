# Kosh -> Rust Migration Plan

## 0. Objectives
- Preserve current functionality (REST + GraphQL APIs, Elasticsearch-backed indexing, filesystem watch-based sync) while reimplementing in Rust.
- Improve performance/latency and resource usage, reduce runtime dependencies, and strengthen type safety.
- Maintain a smooth operational transition: coexistence period, reproducible builds, and parity test suites.

## 1. Current Architecture Audit (Python)
- Entrypoint `kosh/kosh.py` sets up logging/config, imports API modules dynamically, connects to Elasticsearch, seeds indices, and starts Flask-based HTTP APIs plus filesystem watchers.
- API layer (`kosh/api/*`): Flask REST endpoints, GraphQL schema via Graphene, shared helper `_api.py`.
- Elasticsearch layer (`kosh/elastic/*`): indexing, schema management, filesystem ingestion.
- Utilities (`kosh/utility/*`): config defaults, dot-access dictionary, logging helper, singleton `instance` holding global state.
- Parameters layer (`kosh/param/*`): CLI flag handlers modifying runtime config.
- Deployment: Pyproject CLI entry `kosh`, Dockerfile orchestrating Python runtime, Compose wiring Elasticsearch.

## 2. Guiding Principles
- Avoid big-bang rewrite: deliver incremental parity milestones with measurable feature checkpoints.
- Prefer async Rust stack to match concurrent HTTP + watcher behaviour.
- Keep configuration shape compatible (`kosh.ini`, ENV overrides) to minimize operational churn.
- Automate regression testing (HTTP contract tests, index content fixtures).
- Document migration outcomes and owner hand-offs at each phase.

## 3. Rust Target Architecture Overview
- Create Rust workspace (Cargo) with crates:
  - `kosh-core`: configuration, domain types, Elasticsearch client abstractions, lexicon/index management.
  - `kosh-api`: HTTP + GraphQL services built atop Axum + Async-GraphQL (or Actix/Juniper) sharing core crate.
  - `kosh-cli`: binary crate providing current CLI behaviour (config parsing, service startup).
- Replace global mutable singletons with explicit state structs shared via `Arc`/`RwLock`.
- Use `tokio` runtime for async tasks, `notify` crate for filesystem watching, `serde`/`toml` for config.
- Employ official `elasticsearch` Rust client (or `opensearch` if compatibility demands) with typed request builders.

## 4. Migration Milestones
### Phase 1 - Discovery & Parity Definition
- Catalogue all runtime config keys, CLI flags, API endpoints, and GraphQL schema fields.
- Capture integration tests against current Python service (via Postman collection or pytest) to serve as parity suite.
- Inventory Docker/image requirements, Elasticsearch version compatibility, file formats under `lexicon` specs.

### Phase 2 - Rust Workspace Scaffolding
- Initialize Cargo workspace with crates listed above; configure Rust toolchain (stable + rustfmt/clippy).
- Set up CI workflow (GitHub Actions) running `cargo fmt`, `cargo clippy`, `cargo test`.
- Port default configuration (`kosh.ini` defaults) into `kosh-core` using `config` crate or custom parser.

### Phase 3 - Core Domain Port
- Model lexicon metadata, schema mappings, file pools as typed structs (serde-enabled).
- Implement config loader merging defaults + ini files + CLI overrides.
- Port logging setup using `tracing` + `tracing-subscriber` with JSON/text output compatible with current logs.
- Wrap Elasticsearch index operations (create/update, lookup) with async APIs returning structured errors (`thiserror`).

### Phase 4 - Data Pipeline & Watchers
- Reimplement filesystem watcher using `notify` or `watchexec` to mimic `inotify` behaviour.
- Port index refresh/update logic, ensuring safe parallel processing using task channels (`tokio::sync::mpsc`).
- Build migration utilities (e.g., CLI subcommands) to validate schema files before ingestion.

### Phase 5 - HTTP/GraphQL Layer
- Choose web framework (recommend `axum` for REST + router flexibility).
- Mirror Flask routes: `/api` metadata, per-lexicon endpoints, dynamic module registration via feature flags.
- Translate GraphQL schema using `async-graphql`, preserving query and mutation shapes; generate schema tests.
- Implement error handling middleware mapping domain errors to HTTP status codes.

### Phase 6 - CLI & Runtime Orchestration
- Reproduce `kosh` CLI options using `clap`/`structopt`, wiring to config overrides.
- Compose service startup: load config, init logging, connect to Elasticsearch, preload lexicons, spawn API + watcher tasks.
- Provide graceful shutdown handling (signal handling) mirroring Python behaviour.

### Phase 7 - Interop & Incremental Rollout
- Provide Dockerfile targeting Rust binary (multi-stage build) and update Compose to launch Rust service alongside Elasticsearch.
- Offer compatibility mode: ability to run Python or Rust service via feature flag during transition.
- Establish data migration checklist ensuring Elasticsearch indices remain unchanged or migrated safely.

### Phase 8 - Validation & Quality
- Implement unit tests (config parsing, domain logic), integration tests (Elasticsearch stub via `testcontainers`), end-to-end HTTP tests.
- Run parity tests captured in Phase 1 against Rust build; fix discrepancies.
- Add load testing harness (e.g., `k6`, `Vegeta`) to compare performance vs Python baseline.

### Phase 9 - Documentation & Adoption
- Update README, architecture diagrams, and operator runbooks to reflect Rust stack.
- Provide developer onboarding docs (toolchain install, cargo commands, debugging tips).
- Deliver migration report summarizing changes, risks, and deprecation plan for Python codebase.

## 5. Risk & Mitigation Highlights
- Elasticsearch client maturity in Rust -> evaluate crate support early; fall back to HTTP client + custom wrappers if needed.
- GraphQL feature parity (subscriptions, custom scalars) -> prototype schema early to surface gaps.
- File watcher semantics differences between `inotify` and cross-platform crates -> write thorough integration tests, consider Linux-only features if acceptable.
- Operational shift (binary vs Python package) -> coordinate with deployment teams for new build pipeline.

## 6. Next Actions (Week 0)
- Confirm target Elasticsearch version and compatibility matrix.
- Approve Rust framework choices (Axum + Async-GraphQL + Tokio).
- Stand up scaffolding branch with Cargo workspace, minimal CLI stub, CI wiring.
- Draft parity test plan and collect current API fixtures.
