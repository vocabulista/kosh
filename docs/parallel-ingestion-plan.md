# Parallel Ingestion Plan

The streaming parser has reduced parse latency for simple XPaths. We now extend that work by
parallelising entry parsing so multi-file lexica index faster without starving Elasticsearch or
breaking deterministic behaviour.

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

3. Run the bounded worker pool that ships in Rust (`XmlEntryParser` now fan-outs entries via
   `crossbeam-channel` workers):
   - Configure pool size with `KOSH_DATA_PARSE_CONCURRENCY` or `--data-parse-concurrency` (leave
     it unset to let kosh pick `max(1, cpu_limit - 1)` automatically, capped at 12).
   - Verify that the coordinator drains results in lexical order before forwarding batches to
     Elasticsearch (sequential `IndexService::bulk_in_chunks`).
   - Confirm complex XPath roots (predicates, namespace qualifiers) still stream correctly—the
     matcher now sanitises predicates before descent.

4. Re-run parity tests (`cargo run --example streaming_parity -- /path/to/kosh_data`) to confirm behaviour
   matches the DOM parser.

5. Stress-test Elasticsearch under concurrent parsing to ensure CPU/IO utilisation stays within
   acceptable bounds and that the worker pool respects container CPU limits.

## Implementation Highlights

- Entry concurrency happens inside `XmlEntryParser::parse_file_streaming`: streaming events are
  serialised into entry blobs, then processed on a bounded pool of blocking worker threads.
- Each worker instantiates its own XPath evaluator, keeping `sxd_xpath`'s `Rc`-backed handles
  thread-confined while still benefiting from cached namespace metadata.
- Ordering is preserved by collecting worker results into indexed slots before handing them to
  `IndexService`, so downstream bulk requests remain deterministic.
- When a root XPath cannot be safely simplified (wildcards, attribute axes), the code still falls
  back to the DOM parser. For supported predicates, the streaming parser now strips the predicate
  portion to match the structural element while the worker re-evaluates the full XPath on the entry.

## Decision Gates

- ✅ Parallel parsing reduces total rebuild time by ≥20% on multi-file lexica without increasing bulk latency p95.
- ✅ Memory and CPU consumption remain within container limits during benchmarks.
- ✅ No regressions in parity tests or Prometheus alerts (parser/bulk latency) after enabling the worker pool.

If any gate fails, document findings and keep the worker pool behind a feature flag for further tuning.
