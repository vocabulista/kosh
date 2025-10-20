use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use kosh_api::{ApiError, ApiServer, AppState};
use kosh_core::{
    discover_lexica, init_tracing, ConfigError, ElasticsearchBackend, ElasticsearchBackendError,
    ElasticsearchConfig, IndexError, IndexService, KoshConfig, LexiconDescriptor, LexiconError,
    LoggingError, SearchService, XmlEntryParser,
};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::{signal, sync::mpsc, time::Duration};
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

    /// Override API root path (must start with '/').
    #[arg(long = "api-root", alias = "api_root", value_name = "PATH")]
    api_root: Option<String>,

    /// Override API listener port.
    #[arg(long = "api-port", alias = "api_port", value_name = "PORT")]
    api_port: Option<u16>,
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
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
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

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        error!(error = %err, "kosh terminated with error");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), CliError> {
    let cli = Cli::parse();

    let mut config =
        KoshConfig::from_optional_file(cli.config.clone()).map_err(CliError::Config)?;

    for entry in &cli.config_text {
        config.merge_inline(entry).map_err(CliError::Config)?;
    }

    if let Some(level) = &cli.log_level {
        config.logger.level = level.clone();
    }

    if let Some(file) = &cli.log_file {
        config.logger.file = Some(file.clone());
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
    };

    let backend = Arc::new(ElasticsearchBackend::new(es_config).map_err(CliError::Elasticsearch)?);
    backend.ping().await.map_err(CliError::Index)?;
    info!(endpoint = %endpoint, "connected to Elasticsearch");
    let index_service = IndexService::new(Arc::clone(&backend));
    let search_service = SearchService::new(Arc::clone(&backend));

    let namespaces: HashMap<_, _> = config
        .namespaces
        .0
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let namespaces = Arc::new(namespaces);

    let shared_lexica: Vec<_> = lexica.into_iter().map(Arc::new).collect();

    let mut bootstrap_tasks = Vec::new();
    for lexicon in &shared_lexica {
        let namespaces = Arc::clone(&namespaces);
        let service = index_service.clone();
        let lexicon = Arc::clone(lexicon);
        bootstrap_tasks.push(tokio::spawn(rebuild_lexicon(
            lexicon,
            namespaces,
            service,
            "bootstrap",
        )));
    }

    for task in bootstrap_tasks {
        if let Err(err) = task.await {
            warn!(error = %err, "bootstrap task join error");
        }
    }

    let shared_state = Arc::new(AppState::new(
        Arc::clone(&config),
        shared_lexica.clone(),
        config.enabled_query_types(),
        search_service,
    ));

    let server = ApiServer::new(Arc::clone(&shared_state));

    if config.data.sync > 0 {
        let watch_config = Arc::clone(&config);
        let watch_state = Arc::clone(&shared_state);
        let watch_namespaces = Arc::clone(&namespaces);
        let watch_service = index_service.clone();
        tokio::spawn(async move {
            if let Err(err) =
                watch_lexica(watch_config, watch_state, watch_namespaces, watch_service).await
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

async fn rebuild_lexicon(
    lexicon: Arc<LexiconDescriptor>,
    namespaces: Arc<HashMap<String, String>>,
    service: IndexService<ElasticsearchBackend>,
    phase: &'static str,
) {
    debug!(phase, lexicon = %lexicon.uid, "rebuild task started");

    let parser = match XmlEntryParser::new(Arc::clone(&lexicon), namespaces.as_ref().clone()) {
        Ok(parser) => parser,
        Err(err) => {
            warn!(phase, lexicon = %lexicon.uid, error = %err, "failed to prepare XML parser");
            return;
        }
    };

    let mut documents = Vec::new();
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
                documents.append(&mut batch);
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

    match service.rebuild(lexicon.as_ref(), documents).await {
        Err(IndexError::NoDocuments { .. }) => debug!(
            phase,
            lexicon = %lexicon.uid,
            "skipping rebuild until document ingestion is available"
        ),
        Err(err) => warn!(phase, lexicon = %lexicon.uid, error = %err, "index rebuild failed"),
        Ok(stats) => {
            info!(phase, lexicon = %lexicon.uid, indexed = stats.indexed, "completed rebuild")
        }
    }
}

async fn rescan_lexica(
    config: Arc<KoshConfig>,
    state: Arc<AppState>,
    namespaces: Arc<HashMap<String, String>>,
    index_service: IndexService<ElasticsearchBackend>,
) -> Result<(), WatchError> {
    let lexica = discover_lexica(&config.data.root, &config.data.spec, &config.data.pool)
        .map_err(WatchError::Lexicon)?;
    info!(count = lexica.len(), "rescan discovered lexica");

    let shared: Vec<_> = lexica.into_iter().map(Arc::new).collect();

    for lexicon in &shared {
        rebuild_lexicon(
            Arc::clone(lexicon),
            Arc::clone(&namespaces),
            index_service.clone(),
            "watcher",
        )
        .await;
    }

    state.replace_lexica(shared).await;
    Ok(())
}

async fn watch_lexica(
    config: Arc<KoshConfig>,
    state: Arc<AppState>,
    namespaces: Arc<HashMap<String, String>>,
    index_service: IndexService<ElasticsearchBackend>,
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

    while let Some(_event) = rx.recv().await {
        tokio::time::sleep(debounce).await;

        while rx.try_recv().is_ok() {}

        if let Err(err) = rescan_lexica(
            Arc::clone(&config),
            Arc::clone(&state),
            Arc::clone(&namespaces),
            index_service.clone(),
        )
        .await
        {
            warn!(error = %err, "lexicon rescan failed");
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
    use super::normalise_endpoint;

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
}
