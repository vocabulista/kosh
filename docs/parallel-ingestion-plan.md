# Parallel Ingestion Plan

The streaming parser has reduced parse latency for simple XPaths. The next step is to evaluate
parallelising file ingestion so multi-file lexica index faster without starving Elasticsearch.

## Objectives

- Keep ingestion throughput high for lexica composed of dozens of XML files.
- Avoid overloading Elasticsearch or the Rust runtime with uncontrolled concurrency.
- Preserve deterministic behaviour (no duplicated documents, consistent ordering where required).

## Benchmark Checklist

1. Capture baseline timings with streaming enabled, `data.bulk_batch_size = 5_000`.
   ```bash
   docker compose run --rm -e KOSH_DATA_STREAMING_PARSER=true kosh-rust --dump-artifacts /tmp/out
   ```
   Record per-file metrics from Prometheus (`kosh_parser_duration_seconds`, `kosh_bulk_latency_seconds`).

2. Move the parity harness to a larger multi-file lexicon (e.g., `de_alcedo`) and measure end-to-end
   rebuild time.

3. Prototype a bounded worker pool in a feature branch:
   - Use `tokio::task::spawn_blocking` or rayon for parsing while keeping bulk uploads sequential.
   - Guard pool size with a config knob (`KOSH_DATA_PARSE_CONCURRENCY`).
   - Ensure back pressure: bulk indexing must not start until all documents from the previous batch are completed.

4. Re-run parity tests (`cargo run --example streaming_parity -- /path/to/kosh_data`) to confirm behaviour
   matches the DOM parser.

5. Stress-test Elasticsearch under concurrent parsing to ensure CPU/IO utilisation stays within acceptable bounds.

## Decision Gates

- ✅ Parallel parsing reduces total rebuild time by ≥20% on multi-file lexica without increasing bulk latency p95.
- ✅ Memory and CPU consumption remain within container limits during benchmarks.
- ✅ No regressions in parity tests or Prometheus alerts (parser/bulk latency) after enabling the worker pool.

If any gate fails, document findings and keep the worker pool behind a feature flag for further tuning.
