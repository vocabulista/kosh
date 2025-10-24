# Kosh - APIs for Lexical Data

For deployment information and reference implementations, please visit:
<a href="https://kosh.uni-koeln.de">https://kosh.uni-koeln.de</a>

To search in the APIs of the reference implementations, please visit:
<a href="https://dicts.uni-koeln.de">https://dicts.uni-koeln.de</a>

## Rust Service

Operational guidance for the Rust implementation lives in two places:

- **Quick reference in-repo:** see `KOSH_RUST_GUIDE.md` for the commands, config flags, and metrics
  you need day-to-day.
- **Long-form docs:** the detailed runbook, alert catalogue, and migration history live in the
  companion documentation repository (`kosh_docs/kosh-rust-ops-runbook.md`).

Prometheus alert rules are provided alongside the external docs, and the bundled Grafana dashboard
ships under `monitoring/grafana/dashboards/kosh-metrics-overview.json` here.
