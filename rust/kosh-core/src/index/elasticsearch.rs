use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Map, Value as JsonValue};
use thiserror::Error;
use tracing::{debug, info};

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
}

impl Default for ElasticsearchConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:9200".into(),
            username: None,
            password: None,
            ca_bundle: None,
            request_timeout_secs: 60,
        }
    }
}

pub struct ElasticsearchBackend {
    config: ElasticsearchConfig,
    client: Client,
    base: String,
}

impl ElasticsearchBackend {
    pub fn new(config: ElasticsearchConfig) -> Result<Self, ElasticsearchBackendError> {
        let timeout = Duration::from_secs(config.request_timeout_secs.max(1));
        let builder = Client::builder().timeout(timeout);

        if let Some(_bundle) = &config.ca_bundle {
            // TODO: support custom CA bundles once requirements are clarified.
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
            .request(Method::GET, "")
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(IndexError::backend(format!(
                "Elasticsearch health check failed: {}",
                response.status()
            )))
        }
    }

    fn endpoint(&self, path: &str) -> String {
        if path.is_empty() {
            return self.base.clone();
        }

        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let url = self.endpoint(path);
        let builder = self.client.request(method, url);

        if let Some(user) = &self.config.username {
            builder.basic_auth(user, self.config.password.as_ref())
        } else {
            builder
        }
    }

    fn map_reqwest_error(err: reqwest::Error) -> IndexError {
        IndexError::backend(format!("Elasticsearch request failed: {err}"))
    }
}

#[async_trait]
impl IndexBackend for ElasticsearchBackend {
    async fn create_index(
        &self,
        lexicon: &LexiconDescriptor,
        schema: &JsonValue,
    ) -> Result<(), IndexError> {
        let body = build_index_body(schema)?;
        let response = self
            .request(Method::PUT, &lexicon.pool)
            .json(&body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

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
        // TODO: introduce retry/backoff for transient failures similar to Python's loop behaviour.
        let response = self
            .request(Method::DELETE, &lexicon.pool)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

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
        let mut payload = String::new();
        for doc in &documents {
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

        let response = self
            .request(Method::POST, &format!("{}/_bulk", lexicon.pool))
            .header("content-type", "application/x-ndjson")
            .body(payload)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

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
            // TODO: implement retry/backoff for partial failures instead of failing fast.
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

    Ok(body)
}
