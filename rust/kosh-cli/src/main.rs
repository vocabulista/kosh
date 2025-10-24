use std::collections::{BTreeSet, HashMap};
use std::env::{self, VarError};
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{CommandFactory, Parser};
use kosh_api::{
    export_artifacts, ApiError, ApiServer, AppState, ArtifactError, Metrics, MetricsError,
    RebuildOutcomeMetric,
};
use kosh_core::{
    discover_lexica, init_tracing, parse_spec, ConfigError, ElasticsearchBackend,
    ElasticsearchBackendError, ElasticsearchConfig, IndexError, IndexService, KoshConfig,
    LexiconDescriptor, LexiconError, LogFormat, LoggingError, ParserMode, SearchService,
    XmlEntryParser,
};
use notify::{
    event::{EventKind, ModifyKind, RenameMode},
    Event, RecommendedWatcher, RecursiveMode, Watcher,
};
use num_cpus;
use thiserror::Error;
use tokio::{
    signal,
    sync::{mpsc, RwLock},
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "kosh", version, about = "Rust service for lexical data APIs")]
struct Cli {
    /// Optional configuration file (INI format) mirroring the Python service.
    #[arg(
        long = "config",
        alias = "config-file",
        alias = "config_file",
        value_name = "FILE"
    )]
    config: Option<PathBuf>,

    /// Additional inline configuration strings (INI format) applied after files.
    #[arg(long = "config-text", alias = "config_text", value_name = "INI")]
    config_text: Vec<String>,

    /// Override logging level (e.g. trace, debug, info, warn, error).
    #[arg(long = "log-level", alias = "log_level", value_name = "LEVEL")]
    log_level: Option<String>,

    /// Write logs to the specified file instead of stdout.
    #[arg(long = "log-file", alias = "log_file", value_name = "FILE")]
    log_file: Option<PathBuf>,

    /// Choose the log output format (`text` or `json`).
    #[arg(long = "log-format", alias = "log_format", value_name = "FORMAT")]
    log_format: Option<String>,

    /// Override the Elasticsearch host used for indexing/search.
    #[arg(long = "data-host", alias = "data_host", value_name = "HOST")]
    data_host: Option<String>,

    /// Override the index pool prefix.
    #[arg(long = "data-pool", alias = "data_pool", value_name = "NAME")]
    data_pool: Option<String>,

    /// Override the path to XML lexicon data.
    #[arg(long = "data-root", alias = "data_root", value_name = "DIR")]
    data_root: Option<PathBuf>,

    /// Override filesystem sync interval.
    #[arg(long = "data-sync", alias = "data_sync", value_name = "SECONDS")]
    data_sync: Option<u32>,

    /// Override the maximum number of XML parser workers.
    #[arg(
        long = "data-parse-concurrency",
        alias = "data_parse_concurrency",
        value_name = "COUNT"
    )]
    data_parse_concurrency: Option<u32>,

    /// Override bulk indexing batch size.
    #[arg(
        long = "data-bulk-batch-size",
        alias = "data_bulk_batch_size",
        value_name = "COUNT"
    )]
    data_bulk_batch_size: Option<u32>,

    /// Enable the streaming XML parser for large dictionaries.
    #[arg(long = "data-streaming-parser", alias = "data_streaming_parser")]
    data_streaming_parser: bool,

    /// Override API root path (must start with '/').
    #[arg(long = "api-root", alias = "api_root", value_name = "PATH")]
    api_root: Option<String>,

    /// Override API listener port.
    #[arg(long = "api-port", alias = "api_port", value_name = "PORT")]
    api_port: Option<u16>,

    /// Render the CLI manual page and exit.
    #[arg(long = "man")]
    man: bool,

    /// Write OpenAPI and GraphQL artefacts to the given directory, then exit.
    #[arg(long = "dump-artifacts", alias = "dump_artifacts", value_name = "DIR")]
    dump_artifacts: Option<PathBuf>,
}

fn render_manpage() -> Result<(), CliError> {
    let mut output = Vec::new();
    let command = Cli::command();
    clap_mangen::Man::new(command)
        .render(&mut output)
        .map_err(CliError::Manpage)?;
    io::stdout().write_all(&output).map_err(CliError::Manpage)?;
    Ok(())
}

fn parse_env_value<T>(name: &str, raw: &str) -> Result<T, CliError>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    raw.parse::<T>().map_err(|err| CliError::InvalidArgument {
        message: format!("invalid value '{raw}' for {name}: {err}"),
    })
}

fn parse_log_format(name: &str, raw: &str) -> Result<LogFormat, CliError> {
    raw.parse::<LogFormat>()
        .map_err(|_| CliError::InvalidArgument {
            message: format!("invalid value '{raw}' for {name}: expected 'text' or 'json'"),
        })
}

fn env_var(name: &str) -> Result<Option<String>, CliError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(CliError::InvalidArgument {
            message: format!("environment variable {name} contains invalid unicode"),
        }),
    }
}

fn detect_cgroup_cpu_limit() -> Option<usize> {
    if let Ok(raw) = fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let mut parts = raw.split_whitespace();
        if let (Some(quota), Some(period)) = (parts.next(), parts.next()) {
            if quota != "max" {
                if let (Ok(quota), Ok(period)) = (quota.parse::<u64>(), period.parse::<u64>()) {
                    if let Some(cores) = compute_core_quota(quota, period) {
                        return Some(cores);
                    }
                }
            }
        }
    }

    let quota_path = "/sys/fs/cgroup/cpu/cpu.cfs_quota_us";
    let period_path = "/sys/fs/cgroup/cpu/cpu.cfs_period_us";
    if let (Ok(quota_raw), Ok(period_raw)) = (
        fs::read_to_string(quota_path),
        fs::read_to_string(period_path),
    ) {
        if let (Ok(quota), Ok(period)) = (
            quota_raw.trim().parse::<i64>(),
            period_raw.trim().parse::<i64>(),
        ) {
            if quota > 0 && period > 0 {
                if let Some(cores) = compute_core_quota(quota as u64, period as u64) {
                    return Some(cores);
                }
            }
        }
    }

    None
}

fn compute_core_quota(quota: u64, period: u64) -> Option<usize> {
    if period == 0 {
        return None;
    }
    let mut cores = quota / period;
    if quota % period != 0 {
        cores += 1;
    }
    Some(cores.max(1) as usize)
}

fn auto_parse_concurrency() -> usize {
    const AUTO_PARSE_MAX: usize = 12;
    let physical = num_cpus::get().max(1);
    let detected = detect_cgroup_cpu_limit().unwrap_or(physical);
    let capped = detected.min(physical);

    let mut usable = capped;
    if usable > 1 {
        usable -= 1;
    }
    if usable == 0 {
        usable = 1;
    }

    usable.min(AUTO_PARSE_MAX)
}

fn effective_parse_concurrency(config: &KoshConfig) -> usize {
    config
        .data
        .parse_concurrency
        .map(|value| value.max(1) as usize)
        .unwrap_or_else(auto_parse_concurrency)
}

fn apply_env_overrides(config: &mut KoshConfig) -> Result<(), CliError> {
    if let Some(value) = env_var("KOSH_LOG_LEVEL")? {
        if !value.trim().is_empty() {
            config.logger.level = value;
        }
    }

    if let Some(value) = env_var("KOSH_LOG_FILE")? {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            config.logger.file = None;
        } else {
            config.logger.file = Some(PathBuf::from(trimmed));
        }
    }

    if let Some(value) = env_var("KOSH_LOG_FORMAT")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.logger.format = parse_log_format("KOSH_LOG_FORMAT", trimmed)?;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_HOST")? {
        if !value.trim().is_empty() {
            config.data.host = value;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_POOL")? {
        if !value.trim().is_empty() {
            config.data.pool = value;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_ROOT")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.data.root = PathBuf::from(trimmed);
        }
    }

    if let Some(value) = env_var("KOSH_DATA_SPEC")? {
        if !value.trim().is_empty() {
            config.data.spec = value;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_SYNC")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.data.sync = parse_env_value("KOSH_DATA_SYNC", trimmed)?;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_WAIT")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.data.wait = parse_env_value("KOSH_DATA_WAIT", trimmed)?;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_LOOP")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.data.r#loop = parse_env_value("KOSH_DATA_LOOP", trimmed)?;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_RETRY_BACKOFF_SECS")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.data.retry_backoff_secs =
                parse_env_value("KOSH_DATA_RETRY_BACKOFF_SECS", trimmed)?;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_BULK_BATCH_SIZE")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.data.bulk_batch_size = parse_env_value("KOSH_DATA_BULK_BATCH_SIZE", trimmed)?;
        }
    }

    if let Some(value) = env_var("KOSH_DATA_PARSE_CONCURRENCY")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            if trimmed.eq_ignore_ascii_case("auto") {
                config.data.parse_concurrency = None;
            } else {
                let parsed: u32 = parse_env_value("KOSH_DATA_PARSE_CONCURRENCY", trimmed)?;
                if parsed == 0 {
                    config.data.parse_concurrency = None;
                } else {
                    config.data.parse_concurrency = Some(parsed.max(1));
                }
            }
        }
    }

    if let Some(value) = env_var("KOSH_DATA_STREAMING_PARSER")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            let lowered = trimmed.to_ascii_lowercase();
            config.data.streaming_parser = match lowered.as_str() {
                "1" | "true" | "yes" => true,
                "0" | "false" | "no" => false,
                _ => {
                    return Err(CliError::InvalidArgument {
                        message: format!(
                            "invalid value '{trimmed}' for KOSH_DATA_STREAMING_PARSER: expected true/false"
                        ),
                    })
                }
            };
        }
    }

    if let Some(value) = env_var("KOSH_API_HOST")? {
        if !value.trim().is_empty() {
            config.api.host = value;
        }
    }

    if let Some(value) = env_var("KOSH_API_ROOT")? {
        if !value.trim().is_empty() {
            config.api.root = value;
        }
    }

    if let Some(value) = env_var("KOSH_API_PORT")? {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            config.api.port = parse_env_value("KOSH_API_PORT", trimmed)?;
        }
    }

    Ok(())
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("configuration error")]
    Config(#[source] ConfigError),
    #[error("logging initialisation error")]
    Logging(#[source] LoggingError),
    #[error("api server error")]
    Api(#[source] ApiError),
    #[error("lexicon discovery error")]
    Lexicon(#[source] LexiconError),
    #[error("index orchestration error")]
    Index(#[source] IndexError),
    #[error("elasticsearch backend error")]
    Elasticsearch(#[source] ElasticsearchBackendError),
    #[error("failed to generate API artefacts")]
    Artifacts(#[source] ArtifactError),
    #[error("metrics initialisation error")]
    Metrics(#[source] MetricsError),
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
    #[error("failed to render manual page")]
    Manpage(#[source] io::Error),
}

#[derive(Debug, Error)]
enum WatchError {
    #[error("failed to create filesystem watcher")]
    Create(#[source] notify::Error),
    #[error("failed to watch path {path:?}")]
    Watch {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },
    #[error("lexicon discovery error")]
    Lexicon(#[source] LexiconError),
}

#[derive(Debug, Clone)]
struct PathChange {
    path: PathBuf,
    kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Create,
    Modify,
    Remove,
    Unknown,
}

#[derive(Debug)]
struct RegistryChanges {
    to_rebuild: Vec<Arc<LexiconDescriptor>>,
    removed: Vec<Arc<LexiconDescriptor>>,
    lexica_snapshot: Option<Vec<Arc<LexiconDescriptor>>>,
}

#[derive(Debug)]
struct SpecUpdate {
    applied: Vec<Arc<LexiconDescriptor>>,
    removed: Vec<Arc<LexiconDescriptor>>,
}

#[derive(Debug)]
struct FailureBudget {
    max_failures: u32,
    quarantine: Duration,
    entries: HashMap<String, FailureEntry>,
}

#[derive(Debug, Default)]
struct FailureEntry {
    failures: u32,
    quarantined_until: Option<Instant>,
}

#[derive(Debug)]
struct QuarantineState {
    until: Instant,
}

impl FailureBudget {
    fn new(max_failures: u32, quarantine: Duration) -> Self {
        let max_failures = max_failures.max(1);
        Self {
            max_failures,
            quarantine,
            entries: HashMap::new(),
        }
    }

    fn should_attempt(&mut self, uid: &str) -> bool {
        let entry = self
            .entries
            .entry(uid.to_string())
            .or_insert_with(FailureEntry::default);

        if let Some(until) = entry.quarantined_until {
            if Instant::now() < until {
                return false;
            }

            entry.quarantined_until = None;
            entry.failures = 0;
        }

        true
    }

    fn record_success(&mut self, uid: &str) {
        self.entries.remove(uid);
    }

    fn record_failure(&mut self, uid: &str) -> Option<QuarantineState> {
        let entry = self
            .entries
            .entry(uid.to_string())
            .or_insert_with(FailureEntry::default);

        entry.failures = entry.failures.saturating_add(1);
        if entry.failures >= self.max_failures {
            let until = Instant::now() + self.quarantine;
            entry.failures = 0;
            entry.quarantined_until = Some(until);
            Some(QuarantineState { until })
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct LexiconRegistry {
    root_hint: PathBuf,
    root_canonical: PathBuf,
    spec_filename: String,
    default_pool: String,
    lexica: HashMap<String, Arc<LexiconDescriptor>>,
    path_index: HashMap<PathBuf, BTreeSet<String>>,
    spec_index: HashMap<PathBuf, BTreeSet<String>>,
}

impl LexiconRegistry {
    fn new(
        root_hint: PathBuf,
        spec_filename: String,
        default_pool: String,
        lexica: &[Arc<LexiconDescriptor>],
    ) -> Self {
        let root_canonical = canonicalise_root(&root_hint);
        let mut registry = Self {
            root_hint,
            root_canonical,
            spec_filename,
            default_pool,
            lexica: HashMap::new(),
            path_index: HashMap::new(),
            spec_index: HashMap::new(),
        };
        registry.replace_all(lexica);
        registry
    }

    fn process_changes(&mut self, changes: &[PathChange]) -> Result<RegistryChanges, LexiconError> {
        let mut to_rebuild = BTreeSet::new();
        let mut removed = Vec::new();
        let mut spec_changed = false;

        for change in changes {
            if self.is_spec_file(&change.path) {
                spec_changed = true;
                match change.kind {
                    ChangeKind::Remove => {
                        removed.extend(self.remove_spec(&change.path));
                    }
                    ChangeKind::Create | ChangeKind::Modify => {
                        let update = self.refresh_spec(&change.path)?;
                        removed.extend(update.removed);
                        for descriptor in &update.applied {
                            to_rebuild.insert(descriptor.uid.clone());
                        }
                    }
                    ChangeKind::Unknown => {}
                }
                continue;
            }

            for descriptor in self.lexica_for_path(&change.path) {
                to_rebuild.insert(descriptor.uid.clone());
            }
        }

        let rebuild_targets = to_rebuild
            .into_iter()
            .filter_map(|uid| self.lexica.get(&uid).cloned())
            .collect::<Vec<_>>();

        let lexica_snapshot = spec_changed.then(|| self.snapshot());

        Ok(RegistryChanges {
            to_rebuild: rebuild_targets,
            removed,
            lexica_snapshot,
        })
    }

    fn replace_all(&mut self, lexica: &[Arc<LexiconDescriptor>]) -> Vec<Arc<LexiconDescriptor>> {
        let previous = std::mem::take(&mut self.lexica);
        self.path_index.clear();
        self.spec_index.clear();

        for descriptor in lexica {
            self.lexica
                .insert(descriptor.uid.clone(), Arc::clone(descriptor));
            self.record_spec(descriptor);
            self.register_lexicon_paths(descriptor);
        }

        previous
            .into_iter()
            .filter_map(|(uid, descriptor)| {
                if self.lexica.contains_key(&uid) {
                    None
                } else {
                    Some(descriptor)
                }
            })
            .collect()
    }

    fn refresh_spec(&mut self, path: &Path) -> Result<SpecUpdate, LexiconError> {
        let spec_path = self.make_absolute(path);
        let descriptors = parse_spec(&spec_path, &self.default_pool)?;
        let parsed: Vec<_> = descriptors.into_iter().map(Arc::new).collect();

        let previous = self.take_spec_mapping(path);

        let mut removed = Vec::new();
        let mut new_uids = BTreeSet::new();

        for descriptor in &parsed {
            new_uids.insert(descriptor.uid.clone());
            if let Some(existing) = self
                .lexica
                .insert(descriptor.uid.clone(), Arc::clone(descriptor))
            {
                self.unregister_lexicon_paths(&existing);
            }
            self.register_lexicon_paths(descriptor);
        }

        for uid in &previous {
            if !new_uids.contains(uid) {
                if let Some(descriptor) = self.lexica.remove(uid) {
                    self.unregister_lexicon_paths(&descriptor);
                    removed.push(descriptor);
                }
            }
        }

        self.set_spec_mapping(path, &new_uids);

        Ok(SpecUpdate {
            applied: parsed,
            removed,
        })
    }

    fn remove_spec(&mut self, path: &Path) -> Vec<Arc<LexiconDescriptor>> {
        let uids = self.take_spec_mapping(path).into_iter().collect::<Vec<_>>();

        let mut removed = Vec::new();
        for uid in uids {
            if let Some(descriptor) = self.lexica.remove(&uid) {
                self.unregister_lexicon_paths(&descriptor);
                removed.push(descriptor);
            }
        }
        removed
    }

    fn lexica_for_path(&self, path: &Path) -> Vec<Arc<LexiconDescriptor>> {
        let mut collected = BTreeSet::new();
        let normalised = self.make_absolute(path);

        if let Some(uids) = self.path_index.get(&normalised) {
            collected.extend(uids.iter().cloned());
        }

        if let Ok(canonical) = fs::canonicalize(&normalised) {
            if let Some(uids) = self.path_index.get(&canonical) {
                collected.extend(uids.iter().cloned());
            }
        }

        if path.is_absolute() {
            if let Some(uids) = self.path_index.get(path) {
                collected.extend(uids.iter().cloned());
            }
        }

        collected
            .into_iter()
            .filter_map(|uid| self.lexica.get(&uid).cloned())
            .collect()
    }

    fn snapshot(&self) -> Vec<Arc<LexiconDescriptor>> {
        let mut lexica: Vec<_> = self.lexica.values().cloned().collect();
        lexica.sort_by(|a, b| a.uid.cmp(&b.uid));
        lexica
    }

    fn record_spec(&mut self, descriptor: &Arc<LexiconDescriptor>) {
        let mut entries = self.take_spec_mapping(&descriptor.spec_path);
        entries.insert(descriptor.uid.clone());
        self.set_spec_mapping(&descriptor.spec_path, &entries);
    }

    fn register_lexicon_paths(&mut self, descriptor: &Arc<LexiconDescriptor>) {
        for path in &descriptor.files {
            let absolute = self.make_absolute(path);
            self.insert_path_mapping(&absolute, &descriptor.uid);
        }

        let schema = self.make_absolute(&descriptor.schema_path);
        self.insert_path_mapping(&schema, &descriptor.uid);
    }

    fn unregister_lexicon_paths(&mut self, descriptor: &Arc<LexiconDescriptor>) {
        for path in &descriptor.files {
            let absolute = self.make_absolute(path);
            self.remove_path_mapping(&absolute, &descriptor.uid);
        }

        let schema = self.make_absolute(&descriptor.schema_path);
        self.remove_path_mapping(&schema, &descriptor.uid);
    }

    fn insert_path_mapping(&mut self, path: &Path, uid: &str) {
        self.path_index
            .entry(path.to_path_buf())
            .or_insert_with(BTreeSet::new)
            .insert(uid.to_string());

        if let Ok(canonical) = fs::canonicalize(path) {
            self.path_index
                .entry(canonical)
                .or_insert_with(BTreeSet::new)
                .insert(uid.to_string());
        }
    }

    fn remove_path_mapping(&mut self, path: &Path, uid: &str) {
        remove_uid(&mut self.path_index, path, uid);
        if let Ok(canonical) = fs::canonicalize(path) {
            remove_uid(&mut self.path_index, &canonical, uid);
        }
    }

    fn take_spec_mapping(&mut self, path: &Path) -> BTreeSet<String> {
        let absolute = self.make_absolute(path);
        let mut collected = self.spec_index.remove(&absolute).unwrap_or_default();

        if let Ok(canonical) = fs::canonicalize(&absolute) {
            if let Some(other) = self.spec_index.remove(&canonical) {
                collected.extend(other);
            }
        }

        collected
    }

    fn set_spec_mapping(&mut self, path: &Path, uids: &BTreeSet<String>) {
        let absolute = self.make_absolute(path);

        if uids.is_empty() {
            self.spec_index.remove(&absolute);
            if let Ok(canonical) = fs::canonicalize(&absolute) {
                self.spec_index.remove(&canonical);
            }
            return;
        }

        self.spec_index.insert(absolute.clone(), uids.clone());

        if let Ok(canonical) = fs::canonicalize(&absolute) {
            self.spec_index.insert(canonical, uids.clone());
        }
    }

    fn is_spec_file(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == self.spec_filename)
            .unwrap_or(false)
    }

    fn make_absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            return path.to_path_buf();
        }

        if let Ok(stripped) = path.strip_prefix(&self.root_hint) {
            return self.root_canonical.join(stripped);
        }

        if let Ok(stripped) = path.strip_prefix(&self.root_canonical) {
            return self.root_canonical.join(stripped);
        }

        self.root_canonical.join(path)
    }
}

fn canonicalise_root(root: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(root) {
        return canonical;
    }

    if root.is_absolute() {
        return root.to_path_buf();
    }

    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(root)
}

fn remove_uid(map: &mut HashMap<PathBuf, BTreeSet<String>>, path: &Path, uid: &str) {
    if let Some(uids) = map.get_mut(path) {
        uids.remove(uid);
        if uids.is_empty() {
            map.remove(path);
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        error!(error = %err, error_debug = ?err, "kosh terminated with error");
        let mut source = err.source();
        let mut depth = 0;
        while let Some(cause) = source {
            error!(cause = %cause, cause_debug = ?cause, depth, "error source");
            source = cause.source();
            depth += 1;
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), CliError> {
    let cli = Cli::parse();

    if cli.man {
        render_manpage()?;
        return Ok(());
    }

    let mut config =
        KoshConfig::from_optional_file(cli.config.clone()).map_err(CliError::Config)?;

    for entry in &cli.config_text {
        config.merge_inline(entry).map_err(CliError::Config)?;
    }

    apply_env_overrides(&mut config)?;

    if let Some(level) = &cli.log_level {
        config.logger.level = level.clone();
    }

    if let Some(file) = &cli.log_file {
        config.logger.file = Some(file.clone());
    }

    if let Some(format) = &cli.log_format {
        config.logger.format = parse_log_format("--log-format", format)?;
    }

    if let Some(host) = &cli.data_host {
        config.data.host = host.clone();
    }

    if let Some(pool) = &cli.data_pool {
        config.data.pool = pool.clone();
    }

    if let Some(root) = &cli.data_root {
        config.data.root = root.clone();
    }

    if let Some(sync) = cli.data_sync {
        config.data.sync = sync;
    }

    if let Some(concurrency) = cli.data_parse_concurrency {
        config.data.parse_concurrency = Some(concurrency.max(1));
    }

    if let Some(batch) = cli.data_bulk_batch_size {
        config.data.bulk_batch_size = batch.max(1);
    }

    if cli.data_streaming_parser {
        config.data.streaming_parser = true;
    }

    if let Some(api_root) = &cli.api_root {
        if !api_root.starts_with('/') {
            return Err(CliError::InvalidArgument {
                message: "api-root must start with '/'".into(),
            });
        }
        config.api.root = api_root.clone();
    }

    if let Some(api_port) = cli.api_port {
        config.api.port = api_port;
    }

    config.refresh_name_dependents();

    init_tracing(&config.logger).map_err(CliError::Logging)?;

    info!(?config, "loaded configuration");

    let config = Arc::new(config);

    let lexica = discover_lexica(&config.data.root, &config.data.spec, &config.data.pool)
        .map_err(CliError::Lexicon)?;
    info!(count = lexica.len(), "discovered lexica");

    let endpoint = normalise_endpoint(&config.data.host);
    let es_config = ElasticsearchConfig {
        endpoint: endpoint.clone(),
        username: None,
        password: None,
        ca_bundle: None,
        request_timeout_secs: u64::from(config.data.wait.max(1)),
        max_retries: config.data.r#loop.max(1),
        retry_backoff_secs: u64::from(config.data.retry_backoff_secs.max(1)),
        bulk_batch_size: config.data.bulk_batch_size.max(1) as usize,
    };

    let metrics = Arc::new(Metrics::new().map_err(CliError::Metrics)?);
    let backend = Arc::new(ElasticsearchBackend::new(es_config).map_err(CliError::Elasticsearch)?);

    let namespaces: HashMap<_, _> = config
        .namespaces
        .0
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let namespaces = Arc::new(namespaces);
    let parser_mode = if config.data.streaming_parser {
        ParserMode::Streaming
    } else {
        ParserMode::Dom
    };
    let parse_concurrency = effective_parse_concurrency(&config);
    info!(
        parse_concurrency,
        auto = config.data.parse_concurrency.is_none(),
        "resolved parser concurrency"
    );

    let shared_lexica: Vec<_> = lexica.into_iter().map(Arc::new).collect();
    let registry = Arc::new(RwLock::new(LexiconRegistry::new(
        config.data.root.clone(),
        config.data.spec.clone(),
        config.data.pool.clone(),
        &shared_lexica,
    )));
    let failure_budget = Arc::new(RwLock::new(FailureBudget::new(
        config.data.r#loop.max(1),
        Duration::from_secs(config.data.retry_backoff_secs.max(1).into()),
    )));

    let shared_state = Arc::new(AppState::new(
        Arc::clone(&config),
        shared_lexica.clone(),
        config.enabled_query_types(),
        SearchService::new(Arc::clone(&backend)),
        Arc::clone(&metrics),
    ));

    if let Some(dir) = &cli.dump_artifacts {
        export_artifacts(Arc::clone(&shared_state), dir)
            .await
            .map_err(CliError::Artifacts)?;
        info!(path = %dir.display(), "wrote API artefacts");
        return Ok(());
    }

    backend.ping().await.map_err(CliError::Index)?;
    info!(endpoint = %endpoint, "connected to Elasticsearch");
    let index_service = IndexService::new(Arc::clone(&backend));

    let mut bootstrap_tasks = Vec::new();
    for lexicon in &shared_lexica {
        let namespaces = Arc::clone(&namespaces);
        let service = index_service.clone();
        let lexicon = Arc::clone(lexicon);
        let budget = Arc::clone(&failure_budget);
        let metrics_clone = Arc::clone(&metrics);
        bootstrap_tasks.push(tokio::spawn(execute_rebuild(
            lexicon,
            namespaces,
            service,
            budget,
            Arc::clone(&shared_state),
            metrics_clone,
            "bootstrap",
            false,
            parser_mode,
            parse_concurrency,
        )));
    }

    for task in bootstrap_tasks {
        if let Err(err) = task.await {
            warn!(error = %err, "bootstrap task join error");
        }
    }

    let server = ApiServer::new(Arc::clone(&shared_state));

    if config.data.sync > 0 {
        let watch_config = Arc::clone(&config);
        let watch_state = Arc::clone(&shared_state);
        let watch_namespaces = Arc::clone(&namespaces);
        let watch_service = index_service.clone();
        let watch_registry = Arc::clone(&registry);
        let watch_budget = Arc::clone(&failure_budget);
        tokio::spawn(async move {
            if let Err(err) = watch_lexica(
                watch_config,
                watch_state,
                watch_namespaces,
                watch_service,
                watch_registry,
                watch_budget,
                parser_mode,
                parse_concurrency,
            )
            .await
            {
                warn!(error = %err, "filesystem watcher terminated");
            }
        });
    }

    let shutdown_signal = async {
        match signal::ctrl_c().await {
            Ok(_) => info!("received shutdown signal"),
            Err(err) => warn!(%err, "failed to install CTRL+C handler"),
        }
    };

    server
        .serve_with_shutdown(shutdown_signal)
        .await
        .map_err(CliError::Api)?;

    Ok(())
}

fn path_changes_from_event(event: Event) -> Vec<PathChange> {
    let Event { kind, paths, .. } = event;

    if paths.is_empty() {
        return Vec::new();
    }

    match kind {
        EventKind::Create(_) => paths
            .into_iter()
            .map(|path| PathChange {
                path,
                kind: ChangeKind::Create,
            })
            .collect(),
        EventKind::Remove(_) => paths
            .into_iter()
            .map(|path| PathChange {
                path,
                kind: ChangeKind::Remove,
            })
            .collect(),
        EventKind::Modify(ModifyKind::Name(mode)) => match mode {
            RenameMode::Both => {
                let mut iter = paths.into_iter();
                let mut changes = Vec::new();
                if let Some(from) = iter.next() {
                    changes.push(PathChange {
                        path: from,
                        kind: ChangeKind::Remove,
                    });
                }
                if let Some(to) = iter.next() {
                    changes.push(PathChange {
                        path: to,
                        kind: ChangeKind::Create,
                    });
                }
                changes.extend(iter.map(|path| PathChange {
                    path,
                    kind: ChangeKind::Modify,
                }));
                changes
            }
            RenameMode::From => paths
                .into_iter()
                .map(|path| PathChange {
                    path,
                    kind: ChangeKind::Remove,
                })
                .collect(),
            RenameMode::To => paths
                .into_iter()
                .map(|path| PathChange {
                    path,
                    kind: ChangeKind::Create,
                })
                .collect(),
            _ => paths
                .into_iter()
                .map(|path| PathChange {
                    path,
                    kind: ChangeKind::Modify,
                })
                .collect(),
        },
        EventKind::Modify(_) => paths
            .into_iter()
            .map(|path| PathChange {
                path,
                kind: ChangeKind::Modify,
            })
            .collect(),
        _ => paths
            .into_iter()
            .map(|path| PathChange {
                path,
                kind: ChangeKind::Unknown,
            })
            .collect(),
    }
}

async fn apply_incremental_updates(
    state: Arc<AppState>,
    namespaces: Arc<HashMap<String, String>>,
    index_service: IndexService<ElasticsearchBackend>,
    registry: Arc<RwLock<LexiconRegistry>>,
    budget: Arc<RwLock<FailureBudget>>,
    parser_mode: ParserMode,
    parse_concurrency: usize,
    changes: Vec<PathChange>,
) -> Result<(), WatchError> {
    if changes.is_empty() {
        return Ok(());
    }

    let outcome = {
        let mut guard = registry.write().await;
        guard
            .process_changes(&changes)
            .map_err(WatchError::Lexicon)?
    };

    let RegistryChanges {
        to_rebuild,
        removed,
        lexica_snapshot,
    } = outcome;

    if let Some(snapshot) = lexica_snapshot {
        state.replace_lexica(snapshot).await;
    }

    for descriptor in &removed {
        if let Err(err) = index_service.delete(descriptor.as_ref()).await {
            warn!(
                lexicon = %descriptor.uid,
                error = %err,
                "failed to delete index for removed lexicon"
            );
        } else {
            info!(lexicon = %descriptor.uid, "deleted index for removed lexicon");
        }
    }

    let metrics = state.metrics();

    for lexicon in to_rebuild {
        execute_rebuild(
            lexicon,
            Arc::clone(&namespaces),
            index_service.clone(),
            Arc::clone(&budget),
            Arc::clone(&state),
            Arc::clone(&metrics),
            "watcher",
            true,
            parser_mode,
            parse_concurrency,
        )
        .await;
    }

    Ok(())
}

struct RebuildTimings {
    parse_seconds: f64,
    index_seconds: f64,
}

enum RebuildOutcome {
    Indexed {
        indexed: usize,
        timings: RebuildTimings,
    },
    SkippedNoDocuments {
        parse_seconds: f64,
    },
}

async fn rebuild_lexicon(
    lexicon: Arc<LexiconDescriptor>,
    namespaces: Arc<HashMap<String, String>>,
    service: IndexService<ElasticsearchBackend>,
    phase: &'static str,
    parser_mode: ParserMode,
    parse_concurrency: usize,
) -> Result<RebuildOutcome, IndexError> {
    debug!(phase, lexicon = %lexicon.uid, "rebuild task started");
    let (documents, parse_seconds) = {
        let parser = match XmlEntryParser::new(
            Arc::clone(&lexicon),
            namespaces.as_ref().clone(),
            parser_mode,
            parse_concurrency,
        ) {
            Ok(parser) => parser,
            Err(err) => {
                warn!(
                    phase,
                    lexicon = %lexicon.uid,
                    error = %err,
                    "failed to prepare XML parser"
                );
                return Err(IndexError::backend(err.to_string()));
            }
        };

        let parse_start = Instant::now();
        let mut documents = Vec::new();
        let mut processed = 0usize;
        let mut next_log = 500usize;
        for file in &lexicon.files {
            match parser.parse_file(file) {
                Ok(mut batch) => {
                    debug!(
                        phase,
                        lexicon = %lexicon.uid,
                        file = %file.display(),
                        count = batch.len(),
                        "parsed lexicon file"
                    );
                    processed += batch.len();
                    documents.append(&mut batch);

                    while processed >= next_log {
                        info!(phase, lexicon = %lexicon.uid, processed, "rebuild progress");
                        next_log += 500;
                    }
                }
                Err(err) => warn!(
                    phase,
                    lexicon = %lexicon.uid,
                    file = %file.display(),
                    error = %err,
                    "failed to parse lexicon file"
                ),
            }
        }

        if documents.is_empty() {
            debug!(phase, lexicon = %lexicon.uid, "no documents produced during rebuild");
        }

        if processed > 0 && processed % 500 != 0 {
            info!(phase, lexicon = %lexicon.uid, processed, "rebuild progress");
        }

        let parse_seconds = parse_start.elapsed().as_secs_f64();
        (documents, parse_seconds)
    };

    let index_start = Instant::now();
    let rebuild_result = service.rebuild(lexicon.as_ref(), documents).await;
    let index_seconds = index_start.elapsed().as_secs_f64();

    match rebuild_result {
        Err(IndexError::NoDocuments { .. }) => {
            debug!(
                phase,
                lexicon = %lexicon.uid,
                "skipping rebuild until document ingestion is available"
            );
            Ok(RebuildOutcome::SkippedNoDocuments { parse_seconds })
        }
        Err(err) => {
            warn!(phase, lexicon = %lexicon.uid, error = %err, "index rebuild failed");
            Err(err)
        }
        Ok(stats) => {
            info!(
                phase,
                lexicon = %lexicon.uid,
                indexed = stats.indexed,
                "completed rebuild"
            );
            Ok(RebuildOutcome::Indexed {
                indexed: stats.indexed,
                timings: RebuildTimings {
                    parse_seconds,
                    index_seconds,
                },
            })
        }
    }
}

async fn execute_rebuild(
    lexicon: Arc<LexiconDescriptor>,
    namespaces: Arc<HashMap<String, String>>,
    service: IndexService<ElasticsearchBackend>,
    budget: Arc<RwLock<FailureBudget>>,
    state: Arc<AppState>,
    metrics: Arc<Metrics>,
    phase: &'static str,
    respect_quarantine: bool,
    parser_mode: ParserMode,
    parse_concurrency: usize,
) {
    let uid = lexicon.uid.clone();

    if respect_quarantine {
        let should_attempt = {
            let mut guard = budget.write().await;
            guard.should_attempt(&uid)
        };

        if !should_attempt {
            debug!(phase, lexicon = %uid, "skip rebuild while lexicon is quarantined");
            metrics.record_rebuild(phase, &uid, RebuildOutcomeMetric::SkipWhileQuarantined);
            metrics.set_quarantine_active(&uid, true);
            return;
        }
        metrics.set_quarantine_active(&uid, false);
    }

    let start = Instant::now();
    let result = rebuild_lexicon(
        Arc::clone(&lexicon),
        Arc::clone(&namespaces),
        service.clone(),
        phase,
        parser_mode,
        parse_concurrency,
    )
    .await;
    let elapsed = start.elapsed();
    let duration_secs = elapsed.as_secs_f64();

    match result {
        Ok(RebuildOutcome::Indexed { indexed, timings }) => {
            metrics.observe_parser_duration(phase, &uid, timings.parse_seconds);
            metrics.observe_bulk_latency(
                phase,
                &uid,
                RebuildOutcomeMetric::Success,
                timings.index_seconds,
            );
            metrics.record_rebuild(phase, &uid, RebuildOutcomeMetric::Success);
            metrics.observe_rebuild_latency(
                phase,
                &uid,
                RebuildOutcomeMetric::Success,
                duration_secs,
            );
            let mut guard = budget.write().await;
            guard.record_success(&uid);
            metrics.set_quarantine_active(&uid, false);
            state.update_lexicon_size(&uid, indexed).await;
        }
        Ok(RebuildOutcome::SkippedNoDocuments { parse_seconds }) => {
            metrics.observe_parser_duration(phase, &uid, parse_seconds);
            metrics.record_rebuild(phase, &uid, RebuildOutcomeMetric::SkippedEmpty);
            metrics.observe_rebuild_latency(
                phase,
                &uid,
                RebuildOutcomeMetric::SkippedEmpty,
                duration_secs,
            );
            let quarantine = {
                let mut guard = budget.write().await;
                guard.record_failure(&uid)
            };
            metrics.record_rebuild_retry(phase, &uid, "empty");
            if let Some(quarantine) = quarantine {
                metrics.set_quarantine_active(&uid, true);
                let remaining = quarantine.until.saturating_duration_since(Instant::now());
                metrics.observe_quarantine_duration(phase, &uid, "empty", remaining.as_secs_f64());
                info!(
                    phase,
                    lexicon = %uid,
                    wait_seconds = remaining.as_secs(),
                    "lexicon quarantined after repeated empty rebuilds"
                );
                metrics.record_rebuild(phase, &uid, RebuildOutcomeMetric::Quarantined);
            }
        }
        Err(err) => {
            metrics.record_rebuild(phase, &uid, RebuildOutcomeMetric::Failure);
            metrics.observe_rebuild_latency(
                phase,
                &uid,
                RebuildOutcomeMetric::Failure,
                duration_secs,
            );
            let quarantine = {
                let mut guard = budget.write().await;
                guard.record_failure(&uid)
            };
            metrics.record_rebuild_retry(phase, &uid, "error");
            if let Some(quarantine) = quarantine {
                metrics.set_quarantine_active(&uid, true);
                let remaining = quarantine.until.saturating_duration_since(Instant::now());
                metrics.observe_quarantine_duration(phase, &uid, "error", remaining.as_secs_f64());
                warn!(
                    phase,
                    lexicon = %uid,
                    wait_seconds = remaining.as_secs(),
                    error = %err,
                    "lexicon quarantined after repeated rebuild failures"
                );
                metrics.record_rebuild(phase, &uid, RebuildOutcomeMetric::Quarantined);
            }
        }
    }
}

async fn rescan_lexica(
    config: Arc<KoshConfig>,
    state: Arc<AppState>,
    namespaces: Arc<HashMap<String, String>>,
    index_service: IndexService<ElasticsearchBackend>,
    registry: Arc<RwLock<LexiconRegistry>>,
    budget: Arc<RwLock<FailureBudget>>,
    parser_mode: ParserMode,
    parse_concurrency: usize,
) -> Result<(), WatchError> {
    let lexica = discover_lexica(&config.data.root, &config.data.spec, &config.data.pool)
        .map_err(WatchError::Lexicon)?;
    info!(count = lexica.len(), "rescan discovered lexica");

    let shared: Vec<_> = lexica.into_iter().map(Arc::new).collect();
    let removed = {
        let mut guard = registry.write().await;
        guard.replace_all(&shared)
    };

    let snapshot = shared.clone();
    state.replace_lexica(snapshot).await;

    for descriptor in &removed {
        if let Err(err) = index_service.delete(descriptor.as_ref()).await {
            warn!(
                lexicon = %descriptor.uid,
                error = %err,
                "failed to delete index for removed lexicon"
            );
        } else {
            info!(lexicon = %descriptor.uid, "removed index after spec deletion");
        }
    }

    let metrics = state.metrics();

    for lexicon in &shared {
        execute_rebuild(
            Arc::clone(lexicon),
            Arc::clone(&namespaces),
            index_service.clone(),
            Arc::clone(&budget),
            Arc::clone(&state),
            Arc::clone(&metrics),
            "watcher",
            true,
            parser_mode,
            parse_concurrency,
        )
        .await;
    }

    Ok(())
}

async fn watch_lexica(
    config: Arc<KoshConfig>,
    state: Arc<AppState>,
    namespaces: Arc<HashMap<String, String>>,
    index_service: IndexService<ElasticsearchBackend>,
    registry: Arc<RwLock<LexiconRegistry>>,
    budget: Arc<RwLock<FailureBudget>>,
    parser_mode: ParserMode,
    parse_concurrency: usize,
) -> Result<(), WatchError> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();

    let mut watcher = RecommendedWatcher::new(
        move |res| match res {
            Ok(event) => {
                let _ = tx.send(event);
            }
            Err(err) => {
                warn!(error = %err, "filesystem watcher error");
            }
        },
        notify::Config::default(),
    )
    .map_err(WatchError::Create)?;

    watcher
        .watch(&config.data.root, RecursiveMode::Recursive)
        .map_err(|source| WatchError::Watch {
            path: config.data.root.clone(),
            source,
        })?;

    let debounce = Duration::from_secs(config.data.sync.max(1).into());

    while let Some(event) = rx.recv().await {
        let mut batch = vec![event];
        tokio::time::sleep(debounce).await;

        while let Ok(event) = rx.try_recv() {
            batch.push(event);
        }

        if batch.iter().any(|event| event.need_rescan()) {
            if let Err(err) = rescan_lexica(
                Arc::clone(&config),
                Arc::clone(&state),
                Arc::clone(&namespaces),
                index_service.clone(),
                Arc::clone(&registry),
                Arc::clone(&budget),
                parser_mode,
                parse_concurrency,
            )
            .await
            {
                warn!(error = %err, "lexicon rescan failed");
            }
            continue;
        }

        let changes: Vec<PathChange> = batch
            .into_iter()
            .flat_map(path_changes_from_event)
            .collect();

        if changes.is_empty() {
            continue;
        }

        if let Err(err) = apply_incremental_updates(
            Arc::clone(&state),
            Arc::clone(&namespaces),
            index_service.clone(),
            Arc::clone(&registry),
            Arc::clone(&budget),
            parser_mode,
            parse_concurrency,
            changes,
        )
        .await
        {
            warn!(error = %err, "incremental lexicon update failed");
        }
    }

    Ok(())
}

fn normalise_endpoint(host: &str) -> String {
    if host.starts_with("http://") || host.starts_with("https://") {
        host.to_owned()
    } else if host.contains(':') {
        format!("http://{}", host)
    } else {
        format!("http://{}:9200", host)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_env_overrides, apply_incremental_updates, effective_parse_concurrency,
        normalise_endpoint, path_changes_from_event, rescan_lexica, ChangeKind, FailureBudget,
        LexiconRegistry, LogFormat, ParserMode, PathChange,
    };
    use httpmock::MockServer;
    use kosh_api::{AppState, Metrics};
    use kosh_core::{
        discover_lexica, parse_spec, ElasticsearchBackend, ElasticsearchConfig, IndexService,
        KoshConfig, SearchService,
    };
    use notify::event::{DataChange, ModifyKind};
    use notify::{Event, EventKind};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::{fs, sync::Arc};
    use tempfile::tempdir;
    use tokio::sync::RwLock;
    use tokio::time::Duration;

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn normalise_endpoint_retains_full_urls() {
        let url = "https://search.example.com:9243";
        assert_eq!(normalise_endpoint(url), url);
    }

    #[test]
    fn normalise_endpoint_adds_scheme_when_port_present() {
        let host = "192.168.1.20:9300";
        assert_eq!(normalise_endpoint(host), "http://192.168.1.20:9300");
    }

    #[test]
    fn normalise_endpoint_adds_default_port() {
        let host = "elasticsearch.internal";
        assert_eq!(
            normalise_endpoint(host),
            "http://elasticsearch.internal:9200"
        );
    }

    #[test]
    fn environment_overrides_update_config() {
        let _log_level = EnvGuard::set("KOSH_LOG_LEVEL", "DEBUG");
        let _log_file = EnvGuard::set("KOSH_LOG_FILE", "/tmp/kosh.log");
        let _log_format = EnvGuard::set("KOSH_LOG_FORMAT", "json");
        let _data_sync = EnvGuard::set("KOSH_DATA_SYNC", "15");
        let _parse_concurrency = EnvGuard::set("KOSH_DATA_PARSE_CONCURRENCY", "4");
        let _api_port = EnvGuard::set("KOSH_API_PORT", "7001");

        let mut config = KoshConfig::default();
        apply_env_overrides(&mut config).expect("env overrides");

        assert_eq!(config.logger.level, "DEBUG");
        assert_eq!(config.logger.file, Some(PathBuf::from("/tmp/kosh.log")));
        assert_eq!(config.logger.format, LogFormat::Json);
        assert_eq!(config.data.sync, 15);
        assert_eq!(config.data.parse_concurrency, Some(4));
        assert_eq!(config.api.port, 7001);
    }

    #[test]
    fn data_change_triggers_rebuild_for_matching_lexicon() {
        let (_dir, mut registry, lexicon, _spec_path, data_path, _schema_path) = setup_registry();

        let outcome = registry
            .process_changes(&[PathChange {
                path: data_path.clone(),
                kind: ChangeKind::Modify,
            }])
            .expect("process changes");

        assert_eq!(outcome.to_rebuild.len(), 1);
        assert_eq!(outcome.to_rebuild[0].uid, lexicon.uid);
        assert!(outcome.removed.is_empty());
        assert!(outcome.lexica_snapshot.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_rebuilds_after_data_change() {
        let server = MockServer::start_async().await;

        let delete_mock = server
            .mock_async(|when, then| {
                when.method("DELETE").path("/kosh[demo]");
                then.status(200);
            })
            .await;
        let create_mock = server
            .mock_async(|when, then| {
                when.method("PUT").path("/kosh[demo]");
                then.status(200);
            })
            .await;
        let bulk_mock = server
            .mock_async(|when, then| {
                when.method("POST").path("/kosh[demo]/_bulk");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"errors": false}"#);
            })
            .await;

        let dir = tempdir().expect("temp dir");
        let root = dir.path();

        fs::write(
            root.join("schema.json"),
            r#"{
    "mappings": {
        "_meta": {
            "_xpaths": {
                "root": "/items/item",
                "id": "id/text()",
                "fields": {
                    "lemma": "lemma/text()"
                }
            }
        },
        "properties": {}
    }
}"#,
        )
        .expect("write schema");

        fs::write(
            root.join("entries.xml"),
            "<items><item><id>a</id><lemma>Alpha</lemma></item></items>",
        )
        .expect("write data");

        fs::write(
            root.join(".kosh"),
            "[demo]\nfiles = entries.xml\nschema = schema.json\n",
        )
        .expect("write spec");

        let mut base_config = KoshConfig::default();
        base_config.data.root = root.to_path_buf();
        base_config.data.spec = ".kosh".into();
        base_config.data.pool = "kosh".into();
        base_config.data.host = server.base_url();
        base_config.data.retry_backoff_secs = 1;
        base_config.data.r#loop = 2;
        base_config.data.sync = 1;

        let namespaces_map: HashMap<_, _> = base_config
            .namespaces
            .0
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let namespaces = Arc::new(namespaces_map);
        let config = Arc::new(base_config);

        let es_config = ElasticsearchConfig {
            endpoint: server.base_url(),
            username: None,
            password: None,
            ca_bundle: None,
            request_timeout_secs: 5,
            max_retries: 1,
            retry_backoff_secs: 1,
            bulk_batch_size: 5_000,
        };
        let backend = Arc::new(ElasticsearchBackend::new(es_config).expect("backend"));
        let index_service = IndexService::new(Arc::clone(&backend));
        let search_service = SearchService::new(Arc::clone(&backend));

        let lexica = discover_lexica(&config.data.root, &config.data.spec, &config.data.pool)
            .expect("discover lexica");
        assert_eq!(lexica.len(), 1);
        let shared_lexica: Vec<_> = lexica.into_iter().map(Arc::new).collect();

        let registry = Arc::new(RwLock::new(LexiconRegistry::new(
            config.data.root.clone(),
            config.data.spec.clone(),
            config.data.pool.clone(),
            &shared_lexica,
        )));
        let budget = Arc::new(RwLock::new(FailureBudget::new(
            config.data.r#loop,
            Duration::from_secs(config.data.retry_backoff_secs.max(1).into()),
        )));

        let metrics = Arc::new(Metrics::new().expect("metrics"));

        let state = Arc::new(AppState::new(
            Arc::clone(&config),
            shared_lexica.clone(),
            config.enabled_query_types(),
            search_service,
            Arc::clone(&metrics),
        ));

        let test_concurrency = effective_parse_concurrency(&config);

        rescan_lexica(
            Arc::clone(&config),
            Arc::clone(&state),
            Arc::clone(&namespaces),
            index_service.clone(),
            Arc::clone(&registry),
            Arc::clone(&budget),
            ParserMode::Dom,
            test_concurrency,
        )
        .await
        .expect("initial rescan");

        delete_mock.assert_hits_async(1).await;
        create_mock.assert_hits_async(1).await;
        bulk_mock.assert_hits_async(1).await;

        fs::write(
            root.join("entries.xml"),
            "<items><item><id>a</id><lemma>Alpha</lemma></item><item><id>b</id><lemma>Beta</lemma></item></items>",
        )
        .expect("update data file");

        let event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
            .add_path(root.join("entries.xml"));
        let changes = path_changes_from_event(event);

        apply_incremental_updates(
            Arc::clone(&state),
            Arc::clone(&namespaces),
            index_service.clone(),
            Arc::clone(&registry),
            Arc::clone(&budget),
            ParserMode::Dom,
            test_concurrency,
            changes,
        )
        .await
        .expect("incremental update");

        delete_mock.assert_hits_async(2).await;
        create_mock.assert_hits_async(2).await;
        bulk_mock.assert_hits_async(2).await;
    }

    #[test]
    fn spec_removal_evicts_lexicon_and_updates_snapshot() {
        let (_dir, mut registry, lexicon, spec_path, _data_path, _schema_path) = setup_registry();

        let outcome = registry
            .process_changes(&[PathChange {
                path: spec_path.clone(),
                kind: ChangeKind::Remove,
            }])
            .expect("process changes");

        assert!(outcome.to_rebuild.is_empty());
        assert_eq!(outcome.removed.len(), 1);
        assert_eq!(outcome.removed[0].uid, lexicon.uid);
        let snapshot = outcome.lexica_snapshot.expect("snapshot");
        assert!(snapshot.is_empty());
    }

    #[test]
    fn spec_modification_replaces_lexicon_and_marks_rebuild() {
        let (_dir, mut registry, lexicon, spec_path, data_path, schema_path) = setup_registry();

        let data_name = data_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("data file name");
        let schema_name = schema_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("schema file name");

        fs::write(
            &spec_path,
            format!("[demo2]\nfiles = {}\nschema = {}\n", data_name, schema_name),
        )
        .expect("rewrite spec file");

        let outcome = registry
            .process_changes(&[PathChange {
                path: spec_path.clone(),
                kind: ChangeKind::Modify,
            }])
            .expect("process changes");

        assert_eq!(outcome.removed.len(), 1);
        assert_eq!(outcome.removed[0].uid, lexicon.uid);
        assert_eq!(outcome.to_rebuild.len(), 1);
        assert_eq!(outcome.to_rebuild[0].uid, "demo2");

        let snapshot = outcome.lexica_snapshot.expect("snapshot");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].uid, "demo2");
    }

    fn setup_registry() -> (
        tempfile::TempDir,
        LexiconRegistry,
        Arc<kosh_core::LexiconDescriptor>,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let dir = tempdir().expect("temp dir");
        let root = dir.path();

        let schema_name = "schema.json";
        let data_name = "entries.xml";
        let schema_path = root.join(schema_name);
        fs::write(
            &schema_path,
            r#"{
    "mappings": {
        "_meta": {
            "_xpaths": {
                "root": "/items/item",
                "id": "id/text()",
                "fields": {
                    "lemma": "lemma/text()"
                }
            }
        },
        "properties": {}
    }
}"#,
        )
        .expect("write schema");

        let data_path = root.join(data_name);
        fs::write(&data_path, "<items/>").expect("write data file");

        let spec_path = root.join(".kosh");
        let spec_contents = format!("[demo]\nfiles = {}\nschema = {}\n", data_name, schema_name);
        fs::write(&spec_path, spec_contents).expect("write spec file");

        let descriptors = parse_spec(&spec_path, "pool").expect("parse spec");
        let arc_descriptors: Vec<_> = descriptors.into_iter().map(Arc::new).collect();

        let registry = LexiconRegistry::new(
            root.to_path_buf(),
            ".kosh".into(),
            "pool".into(),
            &arc_descriptors,
        );

        let lexicon = arc_descriptors
            .first()
            .cloned()
            .expect("at least one lexicon");

        (dir, registry, lexicon, spec_path, data_path, schema_path)
    }
}
