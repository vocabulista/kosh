use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use kosh_api::{ApiError, ApiServer};
use kosh_core::{
    discover_lexica, init_tracing, ConfigError, ElasticsearchBackend, ElasticsearchBackendError,
    ElasticsearchConfig, IndexError, KoshConfig, LexiconError, LoggingError, XmlEntryParser,
};
use thiserror::Error;
use tokio::signal;
use tracing::{debug, error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "kosh", version, about = "Rust service for lexical data APIs")]
struct Cli {
    /// Optional configuration file (INI format) mirroring the Python service.
    #[arg(long = "config", value_name = "FILE")]
    config: Option<PathBuf>,

    /// Override logging level (e.g. trace, debug, info, warn, error).
    #[arg(long = "log-level", value_name = "LEVEL", default_value = "info")]
    log_level: String,
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

    init_tracing(&cli.log_level).map_err(CliError::Logging)?;

    let config = KoshConfig::from_optional_file(cli.config).map_err(CliError::Config)?;
    info!(?config, "loaded configuration");

    let lexica = discover_lexica(&config.data.root, &config.data.spec, &config.data.pool)
        .map_err(CliError::Lexicon)?;
    info!(count = lexica.len(), "discovered lexica");

    let endpoint = normalise_endpoint(&config.data.host);
    let es_config = ElasticsearchConfig {
        endpoint: endpoint.clone(),
        username: None,
        password: None,
        ca_bundle: None,
        request_timeout_secs: u64::from(config.data.wait),
    };

    let backend = ElasticsearchBackend::new(es_config).map_err(CliError::Elasticsearch)?;
    backend.ping().await.map_err(CliError::Index)?;
    info!(endpoint = %endpoint, "connected to Elasticsearch");
    let index_service = backend.into_service();

    let namespaces: HashMap<_, _> = config
        .namespaces
        .0
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let namespaces = Arc::new(namespaces);

    let shared_lexica: Vec<_> = lexica.into_iter().map(Arc::new).collect();

    let _bootstrap_handles: Vec<_> = shared_lexica
        .iter()
        .cloned()
        .map(|lexicon| {
            let namespaces = Arc::clone(&namespaces);
            let service = index_service.clone();
            tokio::spawn(async move {
                debug!(lexicon = %lexicon.uid, "bootstrap index task queued");

                let parser = match XmlEntryParser::new(Arc::clone(&lexicon), namespaces.as_ref().clone()) {
                    Ok(parser) => parser,
                    Err(err) => {
                        warn!(lexicon = %lexicon.uid, error = %err, "failed to prepare XML parser");
                        return;
                    }
                };

                let mut documents = Vec::new();
                for file in &lexicon.files {
                    match parser.parse_file(file) {
                        Ok(mut batch) => {
                            debug!(
                                lexicon = %lexicon.uid,
                                file = %file.display(),
                                count = batch.len(),
                                "parsed lexicon file"
                            );
                            documents.append(&mut batch);
                        }
                        Err(err) => warn!(
                            lexicon = %lexicon.uid,
                            file = %file.display(),
                            error = %err,
                            "failed to parse lexicon file"
                        ), // TODO: escalate repeated failures and consider quarantining corrupt files.
                    }
                }

                if documents.is_empty() {
                    debug!(lexicon = %lexicon.uid, "no documents produced during bootstrap");
                }

                match service.rebuild(lexicon.as_ref(), documents).await {
                    Err(IndexError::NoDocuments { .. }) => debug!(
                        lexicon = %lexicon.uid,
                        "skipping initial rebuild until document ingestion is available"
                    ),
                    Err(err) => warn!(lexicon = %lexicon.uid, error = %err, "index bootstrap failed"),
                    Ok(stats) => info!(lexicon = %lexicon.uid, indexed = stats.indexed, "completed bootstrap"),
                }
            })
        })
        .collect();

    let server = ApiServer::new(config);

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

fn normalise_endpoint(host: &str) -> String {
    if host.starts_with("http://") || host.starts_with("https://") {
        host.to_owned()
    } else if host.contains(':') {
        format!("http://{}", host)
    } else {
        format!("http://{}:9200", host)
    }
}
