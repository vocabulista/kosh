use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Map, Value as JsonValue};
use thiserror::Error;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

use super::{IndexBackend, IndexDocument, IndexError, IndexService, IndexStats};
use crate::lexicon::LexiconDescriptor;

/// Connection options for the Elasticsearch backend.
#[derive(Debug, Clone)]
pub struct ElasticsearchConfig {
    pub endpoint: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ca_bundle: Option<String>,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    pub retry_backoff_secs: u64,
}

impl Default for ElasticsearchConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:9200".into(),
            username: None,
            password: None,
            ca_bundle: None,
            request_timeout_secs: 60,
            max_retries: 3,
            retry_backoff_secs: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ElasticsearchBackend {
    config: ElasticsearchConfig,
    client: Client,
    base: String,
}

impl ElasticsearchBackend {
    pub fn new(config: ElasticsearchConfig) -> Result<Self, ElasticsearchBackendError> {
        let timeout = StdDuration::from_secs(config.request_timeout_secs.max(1));
        let mut builder = Client::builder().timeout(timeout);

        if let Some(bundle) = &config.ca_bundle {
            let cert = load_certificate(bundle)?;
            builder = builder.add_root_certificate(cert);
        }

        let client = builder
            .build()
            .map_err(|err| ElasticsearchBackendError::ClientInit {
                details: err.to_string(),
            })?;

        let base = config.endpoint.trim_end_matches('/').to_string();

        Ok(Self {
            config,
            client,
            base,
        })
    }

    pub fn into_service(self) -> IndexService<Self> {
        info!(endpoint = %self.base, "initialised Elasticsearch backend");
        IndexService::new(Arc::new(self))
    }

    pub async fn ping(&self) -> Result<(), IndexError> {
        let response = self
            .execute_with_retry("ping", || self.request(Method::GET, ""))
            .await?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(IndexError::backend(format!(
                "Elasticsearch health check failed: {}",
                response.status()
            )))
        }
    }

    pub(crate) fn endpoint(&self, path: &str) -> String {
        if path.is_empty() {
            return self.base.clone();
        }

        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    pub(crate) fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let url = self.endpoint(path);
        let builder = self.client.request(method, url);

        if let Some(user) = &self.config.username {
            builder.basic_auth(user, self.config.password.as_ref())
        } else {
            builder
        }
    }

    pub(crate) fn map_reqwest_error(err: reqwest::Error) -> IndexError {
        IndexError::backend(format!("Elasticsearch request failed: {err}"))
    }

    async fn execute_with_retry<F>(
        &self,
        operation: &str,
        mut build: F,
    ) -> Result<reqwest::Response, IndexError>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let attempts = self.config.max_retries.max(1);
        let mut last_error: Option<IndexError> = None;

        for attempt in 0..attempts {
            let allow_retry = attempt + 1 < attempts;
            let builder = build();

            match builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() || !allow_retry || !should_retry_status(status) {
                        return Ok(response);
                    }

                    warn!(
                        %operation,
                        %status,
                        attempt = attempt + 1,
                        retries = attempts,
                        "retrying after HTTP error"
                    );
                    last_error = Some(IndexError::backend(format!(
                        "{} failed with status {}",
                        operation, status
                    )));
                    sleep(self.retry_delay(attempt)).await;
                }
                Err(err) => {
                    let mapped = Self::map_reqwest_error(err);
                    if !allow_retry {
                        return Err(mapped);
                    }

                    warn!(
                        %operation,
                        attempt = attempt + 1,
                        retries = attempts,
                        error = %mapped,
                        "retrying after transport error"
                    );
                    last_error = Some(mapped);
                    sleep(self.retry_delay(attempt)).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            IndexError::backend(format!("{operation} failed after {} attempts", attempts))
        }))
    }

    fn retry_delay(&self, attempt: u32) -> Duration {
        let base = self.config.retry_backoff_secs.max(1);
        let power = attempt.min(10); // prevent overflow
        let multiplier = 1u64 << power;
        Duration::from_secs(base.saturating_mul(multiplier))
    }
}

#[async_trait]
impl IndexBackend for ElasticsearchBackend {
    async fn create_index(
        &self,
        lexicon: &LexiconDescriptor,
        schema: &JsonValue,
    ) -> Result<(), IndexError> {
        let body = Arc::new(build_index_body(schema)?);
        let response = self
            .execute_with_retry("create_index", || {
                self.request(Method::PUT, &lexicon.pool).json(&*body)
            })
            .await?;

        let status = response.status();
        let body = response.text().await.map_err(Self::map_reqwest_error)?;

        if status.is_success() {
            debug!(index = %lexicon.pool, "created index");
            return Ok(());
        }

        if status == StatusCode::BAD_REQUEST && body.contains("resource_already_exists_exception") {
            debug!(index = %lexicon.pool, "index already exists");
            return Ok(());
        }

        Err(IndexError::backend(format!(
            "failed to create index {} ({}): {}",
            lexicon.pool, status, body
        )))
    }

    async fn delete_index(&self, lexicon: &LexiconDescriptor) -> Result<(), IndexError> {
        let response = self
            .execute_with_retry("delete_index", || {
                self.request(Method::DELETE, &lexicon.pool)
            })
            .await?;

        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            debug!(index = %lexicon.pool, "index already absent");
            return Ok(());
        }

        if status.is_success() {
            debug!(index = %lexicon.pool, "deleted index");
            return Ok(());
        }

        let body = response.text().await.map_err(Self::map_reqwest_error)?;

        Err(IndexError::backend(format!(
            "failed to delete index {} ({}): {}",
            lexicon.pool, status, body
        )))
    }

    async fn bulk_index(
        &self,
        lexicon: &LexiconDescriptor,
        documents: Vec<IndexDocument>,
    ) -> Result<IndexStats, IndexError> {
        let payload = Arc::new(build_bulk_payload(&documents, lexicon)?.into_bytes());
        let response = self
            .execute_with_retry("bulk_index", || {
                self.request(Method::POST, &format!("{}/_bulk", lexicon.pool))
                    .header("content-type", "application/x-ndjson")
                    .body((*payload).clone())
            })
            .await?;

        let status = response.status();
        let body = response.text().await.map_err(Self::map_reqwest_error)?;

        if !status.is_success() {
            return Err(IndexError::backend(format!(
                "failed to bulk index into {} ({}): {}",
                lexicon.pool, status, body
            )));
        }

        let summary: JsonValue = serde_json::from_str(&body).map_err(|err| {
            IndexError::backend(format!(
                "failed to parse bulk response for {}: {}",
                lexicon.pool, err
            ))
        })?;

        if summary
            .get("errors")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(IndexError::backend(format!(
                "Elasticsearch reported bulk indexing errors for {}: {}",
                lexicon.pool, body
            )));
        }

        Ok(IndexStats::new(documents.len()))
    }
}

#[derive(Debug, Error)]
pub enum ElasticsearchBackendError {
    #[error("unsupported authentication mode")]
    UnsupportedAuth,
    #[error("client construction error: {details}")]
    ClientInit { details: String },
    #[error("failed to read CA bundle at {path}")]
    CaBundleIo {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid CA bundle at {path}")]
    CaBundleParse {
        path: String,
        #[source]
        source: reqwest::Error,
    },
}

fn build_index_body(schema: &JsonValue) -> Result<JsonValue, IndexError> {
    let mut body = schema.clone();

    let mappings = body
        .as_object_mut()
        .ok_or_else(|| IndexError::backend("schema must be a JSON object"))?
        .entry("mappings")
        .or_insert_with(|| JsonValue::Object(Map::new()));

    let mappings_obj = mappings
        .as_object_mut()
        .ok_or_else(|| IndexError::backend("mappings must be a JSON object"))?;

    let properties = mappings_obj
        .entry("properties")
        .or_insert_with(|| JsonValue::Object(Map::new()));

    let props = properties
        .as_object_mut()
        .ok_or_else(|| IndexError::backend("mappings.properties must be a JSON object"))?;

    props.insert("created".into(), json!({"type": "date"}));
    props.insert(
        "xml".into(),
        json!({
            "type": "text",
            "analyzer": "strip_tags"
        }),
    );

    let meta_analysis = mappings_obj
        .get("_meta")
        .and_then(JsonValue::as_object)
        .and_then(|meta| meta.get("_analysis"))
        .and_then(JsonValue::as_object)
        .cloned();

    let settings = body
        .as_object_mut()
        .unwrap()
        .entry("settings")
        .or_insert_with(|| JsonValue::Object(Map::new()));

    let settings_obj = settings
        .as_object_mut()
        .ok_or_else(|| IndexError::backend("settings must be a JSON object"))?;

    let analysis = settings_obj
        .entry("analysis")
        .or_insert_with(|| JsonValue::Object(Map::new()));

    let analysis_obj = analysis
        .as_object_mut()
        .ok_or_else(|| IndexError::backend("settings.analysis must be a JSON object"))?;

    let analyzers = analysis_obj
        .entry("analyzer")
        .or_insert_with(|| JsonValue::Object(Map::new()));

    let analyzers_obj = analyzers
        .as_object_mut()
        .ok_or_else(|| IndexError::backend("settings.analysis.analyzer must be a JSON object"))?;

    analyzers_obj.insert(
        "strip_tags".into(),
        json!({
            "type": "custom",
            "tokenizer": "standard",
            "char_filter": ["html_strip"],
            "filter": ["lowercase"]
        }),
    );

    if let Some(analysis_meta) = meta_analysis {
        merge_analysis_section(analysis_obj, "analyzer", analysis_meta.get("analyzers"))?;
        merge_analysis_section(analysis_obj, "filter", analysis_meta.get("filters"))?;
        merge_analysis_section(analysis_obj, "tokenizer", analysis_meta.get("tokenizers"))?;
        merge_analysis_section(
            analysis_obj,
            "char_filter",
            analysis_meta.get("char_filters"),
        )?;
    }

    Ok(body)
}

fn build_bulk_payload(
    documents: &[IndexDocument],
    lexicon: &LexiconDescriptor,
) -> Result<String, IndexError> {
    let mut payload = String::new();

    for doc in documents {
        let action = json!({
            "index": {
                "_index": lexicon.pool,
                "_id": doc.id
            }
        });
        payload.push_str(&serde_json::to_string(&action).map_err(|err| {
            IndexError::backend(format!("failed to serialise bulk action: {err}"))
        })?);
        payload.push('\n');

        let body = JsonValue::Object(doc.fields.clone());
        payload.push_str(&serde_json::to_string(&body).map_err(|err| {
            IndexError::backend(format!("failed to serialise document body: {err}"))
        })?);
        payload.push('\n');
    }

    Ok(payload)
}

fn should_retry_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
}

fn load_certificate(
    path: impl AsRef<Path>,
) -> Result<reqwest::Certificate, ElasticsearchBackendError> {
    let path_ref = path.as_ref();
    let bytes = fs::read(path_ref).map_err(|source| ElasticsearchBackendError::CaBundleIo {
        path: path_ref.display().to_string(),
        source,
    })?;

    reqwest::Certificate::from_pem(&bytes).map_err(|source| {
        ElasticsearchBackendError::CaBundleParse {
            path: path_ref.display().to_string(),
            source,
        }
    })
}

fn merge_analysis_section(
    analysis: &mut Map<String, JsonValue>,
    section: &str,
    source: Option<&JsonValue>,
) -> Result<(), IndexError> {
    let Some(source) = source else {
        return Ok(());
    };

    let JsonValue::Object(source_map) = source else {
        return Err(IndexError::backend(format!(
            "_meta._analysis.{section} must be an object"
        )));
    };

    let target = analysis
        .entry(section)
        .or_insert_with(|| JsonValue::Object(Map::new()));

    let JsonValue::Object(target_map) = target else {
        return Err(IndexError::backend(format!(
            "settings.analysis.{section} must be an object"
        )));
    };

    for (key, value) in source_map {
        target_map.insert(key.clone(), value.clone());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn build_index_body_injects_created_and_xml_fields() {
        let schema = json!({
            "mappings": {
                "properties": {
                    "lemma": { "type": "text" }
                }
            }
        });

        let result = build_index_body(&schema).expect("body");
        let props = result["mappings"]["properties"]
            .as_object()
            .expect("properties object");

        assert!(props.contains_key("lemma"));
        assert_eq!(props["created"], json!({"type": "date"}));
        assert_eq!(
            props["xml"],
            json!({
                "type": "text",
                "analyzer": "strip_tags"
            })
        );

        // Original schema remains untouched.
        assert!(!schema["mappings"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("created"));
    }

    #[test]
    fn build_index_body_ensures_strip_tags_analyzer() {
        let schema = json!({});

        let result = build_index_body(&schema).expect("body");
        let analyzer = result["settings"]["analysis"]["analyzer"]["strip_tags"]
            .as_object()
            .expect("strip_tags analyzer");

        assert_eq!(analyzer.get("type"), Some(&json!("custom")));
        assert_eq!(analyzer.get("char_filter"), Some(&json!(["html_strip"])));
    }

    #[test]
    fn build_index_body_merges_analysis_from_meta() {
        let schema = json!({
            "mappings": {
                "properties": {},
                "_meta": {
                    "_analysis": {
                        "analyzers": {
                            "keyword_lower": {"type": "custom", "tokenizer": "keyword", "filter": ["lowercase"]}
                        },
                        "filters": {
                            "edge_filter": {"type": "edge_ngram", "min_gram": 2, "max_gram": 10}
                        }
                    }
                }
            },
            "settings": {
                "analysis": {
                    "tokenizer": {
                        "custom_tokenizer": {"type": "standard"}
                    }
                }
            }
        });

        let result = build_index_body(&schema).expect("body");
        let analysis = result["settings"]["analysis"]
            .as_object()
            .expect("analysis");

        let analyzers = analysis
            .get("analyzer")
            .and_then(JsonValue::as_object)
            .expect("analyzers");
        assert!(analyzers.contains_key("strip_tags"));
        assert!(analyzers.contains_key("keyword_lower"));

        let filters = analysis
            .get("filter")
            .and_then(JsonValue::as_object)
            .expect("filters");
        assert!(filters.contains_key("edge_filter"));

        let tokenizers = analysis
            .get("tokenizer")
            .and_then(JsonValue::as_object)
            .expect("tokenizers");
        assert!(tokenizers.contains_key("custom_tokenizer"));
    }

    #[test]
    fn bulk_payload_serialises_documents() {
        let lexicon = LexiconDescriptor {
            uid: "demo".into(),
            pool: "kosh[demo]".into(),
            files: vec![],
            schema: json!({}),
            attributes: Default::default(),
            spec_path: Default::default(),
            schema_path: Default::default(),
        };

        let mut fields = Map::new();
        fields.insert("lemma".into(), JsonValue::String("Foo".into()));
        let doc = IndexDocument::new("doc-1", fields).expect("doc");

        let payload = build_bulk_payload(&[doc], &lexicon).expect("payload");
        assert!(payload.contains("\"_id\":\"doc-1\""));
        assert!(payload.contains("\"lemma\":\"Foo\""));
    }

    #[test]
    fn invalid_ca_bundle_returns_error() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(file, "not a valid cert").expect("write");

        let config = ElasticsearchConfig {
            endpoint: "http://localhost:9200".into(),
            username: None,
            password: None,
            ca_bundle: Some(file.path().display().to_string()),
            request_timeout_secs: 5,
            max_retries: 1,
            retry_backoff_secs: 1,
        };

        let result = ElasticsearchBackend::new(config);
        assert!(matches!(
            result,
            Err(ElasticsearchBackendError::CaBundleParse { .. })
        ));
    }
}
