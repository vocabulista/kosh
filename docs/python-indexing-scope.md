# Python Indexing Scope Notes

## Modules Reviewed
- `kosh/elastic/index.py`
- `kosh/elastic/entry.py`
- `kosh/elastic/search.py`

## Responsibilities Identified
- **Lexicon lookup & watching**: glob `.kosh` spec files, parse via `ConfigParser`, turn into in-memory dicts, and emit updates via `inotify` watcher.
- **Index lifecycle**: `create`, `delete`, `update`, `append` orchestrate Elasticsearch indices using `elasticsearch.helpers.bulk`. `update` performs drop+create+append. Timeouts trigger retries (requeue file).
- **Document parsing**: `entry.parse` loads XML with `lxml`, walks configured XPath roots, derives ID (`xpaths.id` fallback to SHA1 hash), normalizes Unicode, and collects mapped fields (array fields inside brackets). Each record becomes `elasticsearch_dsl.Document` with metadata (`created`, `xml`).
- **Query surface**: `search.entries` and `search.ids` wrap Elasticsearch DSL searches for REST/GraphQL endpoints, translating query types (`term`, `match`, etc.) and mapping results to dicts.

## Behavioural Nuances
- XML parsing honors namespace map from config; missing or malformed XML is logged and skipped.
- Bulk failures due to `ConnectionTimeout` requeue the file for another pass; unexpected exceptions logged but do not halt service.
- XPath field definitions differentiate scalar vs repeated fields using `[...]` syntax.
- `search.entries` normalizes date strings by stripping non-digit characters and constructing `datetime` objects.

## Rust Port Targets
- Provide streaming XML parser honoring namespace config (likely `quick-xml` + XPath equivalent or ported logic).
- Implement `IndexBackend` backed by Elasticsearch HTTP client: **done** (basic create/delete/bulk via REST). Outstanding: analyzer parity, retry/backoff, health checks.
- Recreate timeout/requeue semantics and error logging strategy (health check now runs at bootstrap; retry/backoff still pending).
- Mirror query builder for REST/GraphQL parity (Axum handlers + async Elasticsearch queries).
- Leverage new `XmlEntryParser` implementation to populate `IndexDocument` batches (serialised XML now included; still need to add `created` timestamps and ensure namespace/prefix fidelity).
- Align fallback identifiers with Python's SHA1 of serialized XML (now hashing element XML; TODO to guarantee identical serialization once prefix/default-namespace handling is validated).
- Introduce error budget strategy for malformed lexicon files (CLI currently logs warnings; TODO to implement quarantining/retry counters similar to Python).

## Open Questions
- Preferred Rust Elasticsearch client (`elasticsearch` crate vs custom reqwest) and TLS/auth requirements.
- How to express XPath-like field extraction without heavy XML dependencies; acceptable to require XPath engine or translate to selector DSL?
- Should bulk ingestion stream parsed documents or buffer per file as Python currently does?
- Desired retry/backoff strategy and logging levels for transient failures.
