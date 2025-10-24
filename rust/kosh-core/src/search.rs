//! Query helpers bridging lexicon descriptors and Elasticsearch search APIs.

use std::collections::HashSet;
use std::sync::Arc;

use reqwest::Method;
use serde_json::{json, Map, Value as JsonValue};
use thiserror::Error;

use crate::index::elasticsearch::ElasticsearchBackend;
use crate::lexicon::LexiconDescriptor;

/// Provides read/query access to Elasticsearch for REST and GraphQL layers.
#[derive(Clone, Debug)]
pub struct SearchService {
    backend: Arc<ElasticsearchBackend>,
}

impl SearchService {
    /// Instantiate the service with a shared Elasticsearch backend.
    pub fn new(backend: Arc<ElasticsearchBackend>) -> Self {
        Self { backend }
    }

    /// Fetch specific entries by identifier via a `_mget` request.
    ///
    /// Duplicate identifiers are coalesced client-side to avoid unnecessary
    /// network overhead. The returned maps include the original `_id` under
    /// the `id` key to match REST payloads.
    pub async fn fetch_ids(
        &self,
        lexicon: &LexiconDescriptor,
        ids: &[String],
    ) -> Result<Vec<Map<String, JsonValue>>, SearchError> {
        let mut seen = HashSet::new();
        let mut docs = Vec::new();

        for id in ids {
            if seen.insert(id) {
                docs.push(json!({
                    "_index": lexicon.pool,
                    "_id": id,
                }));
            }
        }

        if docs.is_empty() {
            return Ok(Vec::new());
        }

        let response = self
            .backend
            .request(Method::POST, "/_mget")
            .json(&json!({ "docs": docs }))
            .send()
            .await
            .map_err(SearchError::from_reqwest)?;

        let status = response.status();
        let body = response.text().await.map_err(SearchError::from_reqwest)?;

        if !status.is_success() {
            return Err(SearchError::backend(format!(
                "failed to fetch ids for {} ({}): {}",
                lexicon.pool, status, body
            )));
        }

        let payload: JsonValue =
            serde_json::from_str(&body).map_err(|err| SearchError::invalid(format!("{err}")))?;
        let docs_value = payload
            .get("docs")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| SearchError::invalid("missing docs array in _mget response"))?;

        let mut results = Vec::new();

        for doc in docs_value {
            if !doc
                .get("found")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
            {
                continue;
            }

            let mut map = doc
                .get("_source")
                .and_then(JsonValue::as_object)
                .cloned()
                .ok_or_else(|| {
                    SearchError::invalid("missing _source for document in _mget response")
                })?;

            if let Some(id) = doc.get("_id").and_then(JsonValue::as_str) {
                map.insert("id".into(), JsonValue::String(id.to_owned()));
            }

            results.push(map);
        }

        Ok(results)
    }

    /// Execute a fielded search against the lexicon index, returning source documents.
    pub async fn search_entries(
        &self,
        lexicon: &LexiconDescriptor,
        field: &str,
        query: &str,
        query_type: &str,
        size: usize,
    ) -> Result<Vec<Map<String, JsonValue>>, SearchError> {
        if size == 0 {
            return Ok(Vec::new());
        }

        let es_field = if field == "id" { "_id" } else { field };

        let mut criterion = Map::new();
        criterion.insert(es_field.to_string(), JsonValue::String(query.to_owned()));

        let mut query_map = Map::new();
        query_map.insert(query_type.to_owned(), JsonValue::Object(criterion));

        let mut body = Map::new();
        body.insert("query".into(), JsonValue::Object(query_map));
        body.insert("size".into(), JsonValue::from(size as u64));

        let response = self
            .backend
            .request(Method::POST, &format!("{}/_search", lexicon.pool))
            .json(&JsonValue::Object(body))
            .send()
            .await
            .map_err(SearchError::from_reqwest)?;

        let status = response.status();
        let body = response.text().await.map_err(SearchError::from_reqwest)?;

        if !status.is_success() {
            return Err(SearchError::backend(format!(
                "failed to search entries for {} ({}): {}",
                lexicon.pool, status, body
            )));
        }

        let payload: JsonValue =
            serde_json::from_str(&body).map_err(|err| SearchError::invalid(format!("{err}")))?;

        let hits = payload
            .get("hits")
            .and_then(JsonValue::as_object)
            .and_then(|hits| hits.get("hits"))
            .and_then(JsonValue::as_array)
            .ok_or_else(|| SearchError::invalid("missing hits array in search response"))?;

        let mut results = Vec::new();
        for hit in hits {
            let mut map = hit
                .get("_source")
                .and_then(JsonValue::as_object)
                .cloned()
                .unwrap_or_default();

            if let Some(id) = hit.get("_id").and_then(JsonValue::as_str) {
                map.insert("id".into(), JsonValue::String(id.to_owned()));
            }

            results.push(map);
        }

        Ok(results)
    }

    /// Access the underlying Elasticsearch backend for advanced use cases.
    pub fn backend(&self) -> &Arc<ElasticsearchBackend> {
        &self.backend
    }
}

/// Errors produced during search operations.
#[derive(Debug, Error)]
pub enum SearchError {
    #[error("backend error: {details}")]
    Backend { details: String },
    #[error("invalid response: {details}")]
    InvalidResponse { details: String },
}

impl SearchError {
    pub fn backend(details: impl Into<String>) -> Self {
        SearchError::Backend {
            details: details.into(),
        }
    }

    pub fn invalid(details: impl Into<String>) -> Self {
        SearchError::InvalidResponse {
            details: details.into(),
        }
    }

    fn from_reqwest(err: reqwest::Error) -> Self {
        SearchError::backend(format!("Elasticsearch request failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serde_json::json;

    fn backend_for(server: &MockServer) -> Arc<ElasticsearchBackend> {
        let config = ElasticsearchConfig {
            endpoint: server.base_url(),
            username: None,
            password: None,
            ca_bundle: None,
            request_timeout_secs: 5,
            max_retries: 1,
            retry_backoff_secs: 1,
            bulk_batch_size: 5_000,
        };

        Arc::new(ElasticsearchBackend::new(config).expect("backend"))
    }

    fn sample_lexicon() -> LexiconDescriptor {
        LexiconDescriptor {
            uid: "demo".into(),
            pool: "kosh[demo]".into(),
            files: vec![],
            schema: json!({}),
            attributes: Default::default(),
            spec_path: Default::default(),
            schema_path: Default::default(),
        }
    }

    use crate::index::elasticsearch::ElasticsearchConfig;

    #[tokio::test]
    async fn fetch_ids_returns_found_documents() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method("POST").path("/_mget").json_body(json!({
                    "docs": [
                        {"_index": "kosh[demo]", "_id": "a"},
                        {"_index": "kosh[demo]", "_id": "b"}
                    ]
                }));
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "docs": [
                            {"found": true, "_id": "a", "_source": {"lemma": "Foo"}},
                            {"found": false, "_id": "b"}
                        ]
                    }));
            })
            .await;

        let backend = backend_for(&server);
        let service = SearchService::new(backend);
        let lexicon = sample_lexicon();

        let result = service
            .fetch_ids(&lexicon, &[String::from("a"), String::from("b")])
            .await
            .expect("fetch ids");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].get("id"), Some(&JsonValue::String("a".into())));
        assert_eq!(
            result[0].get("lemma"),
            Some(&JsonValue::String("Foo".into()))
        );

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn search_entries_returns_hits() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method("POST")
                    .path("/kosh[demo]/_search")
                    .json_body(json!({
                        "query": { "match": { "lemma": "foo" } },
                        "size": 5
                    }));
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "hits": {
                            "hits": [
                                {"_id": "doc-1", "_source": {"lemma": "Foo"}}
                            ]
                        }
                    }));
            })
            .await;

        let backend = backend_for(&server);
        let service = SearchService::new(backend);
        let lexicon = sample_lexicon();

        let result = service
            .search_entries(&lexicon, "lemma", "foo", "match", 5)
            .await
            .expect("search entries");

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].get("id"),
            Some(&JsonValue::String("doc-1".into()))
        );

        mock.assert_async().await;
    }
}
