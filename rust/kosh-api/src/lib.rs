use std::collections::{BTreeMap, HashSet};
use std::future::{pending, Future};
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Host, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use kosh_core::{ApiConfig, InfoConfig, KoshConfig, LexiconDescriptor, SearchService};
use serde::Deserialize;
use serde_json::{json, Map, Value as JsonValue};
use thiserror::Error;
use tokio::{net::TcpListener, sync::RwLock, task::JoinHandle};
use tracing::{info, warn};

/// Lightweight wrapper around the Axum HTTP server.
pub struct ApiServer {
    state: Arc<AppState>,
}

impl ApiServer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> Arc<AppState> {
        Arc::clone(&self.state)
    }

    pub async fn serve(self) -> Result<(), ApiError> {
        self.serve_with_shutdown(pending::<()>()).await
    }

    pub fn spawn(self) -> JoinHandle<Result<(), ApiError>> {
        tokio::spawn(async move { self.serve().await })
    }

    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<(), ApiError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let router = self.router();
        let addr = self.socket_addr();

        info!(%addr, "starting HTTP service");

        let listener = TcpListener::bind(addr).await.map_err(ApiError::Bind)?;

        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(ApiError::Server)?;

        Ok(())
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/health", get(healthcheck))
            .route("/api", get(api_root))
            .route("/api/:uid", get(api_lexicon))
            .route("/api/:uid/restful/entries", get(restful_entries))
            .route("/api/:uid/restful/ids", get(restful_ids))
            .route("/api/:uid/restful/spec", get(restful_spec))
            .with_state(self.state())
    }

    fn socket_addr(&self) -> SocketAddr {
        let api = &self.state.config.api;
        let host = api
            .host
            .parse()
            .unwrap_or_else(|_| "0.0.0.0".parse().expect("valid host"));
        SocketAddr::new(host, api.port)
    }

    pub fn api_config(&self) -> &ApiConfig {
        &self.state.config.api
    }
}

async fn healthcheck() -> &'static str {
    "ok"
}

async fn api_root(State(state): State<Arc<AppState>>) -> Json<JsonValue> {
    let lexica = state.lexica_snapshot().await;
    Json(json!({
        "about": info_payload(&state.config.info),
        "dicts": lexicon_map(&state, &lexica, None),
    }))
}

async fn api_lexicon(Path(uid): Path<String>, State(state): State<Arc<AppState>>) -> ApiResult {
    let runtime = state
        .lexicon(&uid)
        .await
        .ok_or_else(|| ApiProblem::not_found("Unknown lexicon"))?;

    Ok(Json(JsonValue::Object(lexicon_overview(&state, &runtime))))
}

async fn restful_entries(
    Path(uid): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<EntriesParams>,
) -> ApiResult {
    let runtime = state
        .lexicon(&uid)
        .await
        .ok_or_else(|| ApiProblem::not_found("Unknown lexicon"))?;

    let field = params
        .field
        .as_deref()
        .ok_or_else(|| ApiProblem::bad_request("Missing or invalid parameter: 'field'"))?;
    let query = params
        .query
        .as_deref()
        .ok_or_else(|| ApiProblem::bad_request("Missing or invalid parameter: 'query'"))?;
    let query_type = params
        .query_type
        .as_deref()
        .ok_or_else(|| ApiProblem::bad_request("Missing or invalid parameter: 'query_type'"))?;

    if runtime.field_type(field).is_none() {
        return Err(ApiProblem::bad_request(
            "Missing or invalid parameter: 'field'",
        ));
    }

    if !state.query_types.iter().any(|value| value == query_type) {
        return Err(ApiProblem::bad_request(
            "Missing or invalid parameter: 'query_type'",
        ));
    }

    let size = params.size.unwrap_or(10).min(10_000);

    let entries = match state
        .search
        .search_entries(&runtime.descriptor, field, query, query_type, size)
        .await
    {
        Ok(entries) => entries,
        Err(err) => {
            warn!(lexicon = %uid, error = %err, "search entries failed");
            Vec::new()
        }
    };

    let payload: Vec<JsonValue> = entries
        .into_iter()
        .map(|item| JsonValue::Object(item))
        .collect();

    Ok(Json(json!({
        "data": {
            "entries": payload
        }
    })))
}

async fn restful_ids(
    Path(uid): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<IdsParams>,
) -> ApiResult {
    let runtime = state
        .lexicon(&uid)
        .await
        .ok_or_else(|| ApiProblem::not_found("Unknown lexicon"))?;

    if params.ids.is_empty() {
        return Err(ApiProblem::bad_request(
            "Missing or invalid parameter: 'ids'",
        ));
    }

    let ids = match state
        .search
        .fetch_ids(&runtime.descriptor, &params.ids)
        .await
    {
        Ok(ids) => ids,
        Err(err) => {
            warn!(lexicon = %uid, error = %err, "fetch ids failed");
            Vec::new()
        }
    };

    let payload: Vec<JsonValue> = ids
        .into_iter()
        .map(|item| JsonValue::Object(item))
        .collect();

    Ok(Json(json!({
        "data": {
            "ids": payload
        }
    })))
}

async fn restful_spec(
    Path(uid): Path<String>,
    Host(host): Host,
    State(state): State<Arc<AppState>>,
) -> ApiResult {
    let runtime = state
        .lexicon(&uid)
        .await
        .ok_or_else(|| ApiProblem::not_found("Unknown lexicon"))?;

    let spec = swagger_document(&state, &runtime, &host);

    Ok(Json(spec))
}

type ApiResult = Result<Json<JsonValue>, ApiProblem>;

struct ApiProblem {
    status: StatusCode,
    message: String,
}

impl ApiProblem {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiProblem {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

#[derive(Debug)]
pub struct AppState {
    pub config: Arc<KoshConfig>,
    lexica: RwLock<BTreeMap<String, Arc<LexiconRuntime>>>,
    pub query_types: Vec<String>,
    pub search: SearchService,
}

impl AppState {
    pub fn new(
        config: Arc<KoshConfig>,
        lexica: Vec<Arc<LexiconDescriptor>>,
        query_types: Vec<String>,
        search: SearchService,
    ) -> Self {
        Self {
            config,
            lexica: RwLock::new(build_runtime_map(lexica)),
            query_types,
            search,
        }
    }

    pub async fn lexicon(&self, uid: &str) -> Option<Arc<LexiconRuntime>> {
        let guard = self.lexica.read().await;
        guard.get(uid).cloned()
    }

    pub async fn lexica_snapshot(&self) -> BTreeMap<String, Arc<LexiconRuntime>> {
        self.lexica.read().await.clone()
    }

    pub async fn replace_lexica(
        &self,
        lexica: Vec<Arc<LexiconDescriptor>>,
    ) -> BTreeMap<String, Arc<LexiconRuntime>> {
        let mut guard = self.lexica.write().await;
        *guard = build_runtime_map(lexica);
        guard.clone()
    }
}

fn build_runtime_map(lexica: Vec<Arc<LexiconDescriptor>>) -> BTreeMap<String, Arc<LexiconRuntime>> {
    lexica
        .into_iter()
        .map(|lexicon| {
            (
                lexicon.uid.clone(),
                Arc::new(LexiconRuntime::new(Arc::clone(&lexicon))),
            )
        })
        .collect()
}

#[derive(Debug)]
pub struct LexiconRuntime {
    pub descriptor: Arc<LexiconDescriptor>,
    pub properties: Vec<String>,
    pub field_types: BTreeMap<String, String>,
    pub array_fields: HashSet<String>,
}

impl LexiconRuntime {
    fn new(descriptor: Arc<LexiconDescriptor>) -> Self {
        let mut field_types = BTreeMap::new();
        field_types.insert("id".into(), "keyword".into());
        field_types.insert("created".into(), "date".into());
        field_types.insert("xml".into(), "text".into());

        let mut properties = Vec::new();
        if let Some(mapping) = descriptor
            .schema
            .get("mappings")
            .and_then(JsonValue::as_object)
        {
            if let Some(props) = mapping.get("properties").and_then(JsonValue::as_object) {
                let mut names = props.keys().cloned().collect::<Vec<_>>();
                names.sort();
                for name in &names {
                    let es_type = props
                        .get(name)
                        .and_then(JsonValue::as_object)
                        .and_then(|node| node.get("type"))
                        .and_then(JsonValue::as_str)
                        .unwrap_or("text")
                        .to_string();
                    field_types.insert(name.clone(), es_type);
                }
                properties = names;
            }
        }

        let array_fields = descriptor
            .schema
            .get("mappings")
            .and_then(JsonValue::as_object)
            .and_then(|mappings| mappings.get("_meta"))
            .and_then(JsonValue::as_object)
            .and_then(|meta| meta.get("_xpaths"))
            .and_then(JsonValue::as_object)
            .and_then(|xpaths| xpaths.get("fields"))
            .and_then(JsonValue::as_object)
            .map(|fields| {
                fields
                    .keys()
                    .filter_map(|key| {
                        if key.starts_with('[') && key.ends_with(']') && key.len() > 2 {
                            Some(key[1..key.len() - 1].to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();

        Self {
            descriptor,
            properties,
            field_types,
            array_fields,
        }
    }

    pub fn field_type(&self, field: &str) -> Option<&str> {
        self.field_types.get(field).map(String::as_str)
    }

    pub fn is_array(&self, field: &str) -> bool {
        self.array_fields.contains(field)
    }

    pub fn mapping_keys(&self) -> impl Iterator<Item = &String> {
        self.field_types.keys()
    }
}

#[derive(Debug, Deserialize)]
struct EntriesParams {
    field: Option<String>,
    query: Option<String>,
    #[serde(rename = "query_type")]
    query_type: Option<String>,
    size: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct IdsParams {
    #[serde(default)]
    ids: Vec<String>,
}

fn lexicon_map(
    state: &AppState,
    lexica: &BTreeMap<String, Arc<LexiconRuntime>>,
    filter: Option<&str>,
) -> JsonValue {
    let mut dicts = Map::new();

    for (uid, runtime) in lexica {
        if filter.map(|value| value != uid).unwrap_or(false) {
            continue;
        }

        dicts.insert(
            uid.clone(),
            JsonValue::Object(lexicon_overview(state, runtime)),
        );
    }

    JsonValue::Object(dicts)
}

fn lexicon_overview(state: &AppState, runtime: &LexiconRuntime) -> Map<String, JsonValue> {
    let mut map = Map::new();

    let mut props = vec![JsonValue::String("id".into())];
    props.extend(runtime.properties.iter().cloned().map(JsonValue::String));

    map.insert("properties".into(), JsonValue::Array(props));
    map.insert(
        "query_types".into(),
        JsonValue::Array(
            state
                .query_types
                .iter()
                .cloned()
                .map(JsonValue::String)
                .collect(),
        ),
    );

    let root = state.config.api.root.trim_end_matches('/');
    map.insert(
        "endpoints".into(),
        JsonValue::Object({
            let mut endpoints = Map::new();
            endpoints.insert(
                "restful".into(),
                JsonValue::String(format!("{root}/{}/restful", runtime.descriptor.uid)),
            );
            endpoints
        }),
    );

    for (key, value) in &runtime.descriptor.attributes {
        map.insert(key.clone(), value.clone());
    }

    map
}

fn info_payload(info: &InfoConfig) -> JsonValue {
    json!({
        "desc": info.desc,
        "link": info.link,
        "mail": info.mail,
        "repo": info.repo,
    })
}

fn swagger_document(state: &AppState, runtime: &LexiconRuntime, host: &str) -> JsonValue {
    let root = state.config.api.root.trim_end_matches('/');
    let base_path = format!("{root}/{}/restful", runtime.descriptor.uid);
    let fields_enum: Vec<JsonValue> = runtime
        .mapping_keys()
        .cloned()
        .map(JsonValue::String)
        .collect();

    let query_types_enum: Vec<JsonValue> = state
        .query_types
        .iter()
        .cloned()
        .map(JsonValue::String)
        .collect();

    let entry_definition = entry_definition(runtime);

    json!({
        "swagger": "2.0",
        "host": host,
        "basePath": base_path,
        "tags": [
            {
                "name": runtime.descriptor.uid
            }
        ],
        "info": {
            "title": state.config.info.desc,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "definitions": {
            "Ids": collection_definition("ids"),
            "Entries": collection_definition("entries"),
            "Entry": entry_definition,
        },
        "paths": {
            "/entries": {
                "get": {
                    "responses": replies("Entries"),
                    "tags": [runtime.descriptor.uid],
                    "parameters": [
                        {
                            "name": "field",
                            "in": "query",
                            "required": true,
                            "type": "string",
                            "enum": fields_enum,
                        },
                        {
                            "name": "query",
                            "in": "query",
                            "required": true,
                            "type": "string",
                        },
                        {
                            "name": "query_type",
                            "in": "query",
                            "required": true,
                            "enum": query_types_enum,
                            "type": "string",
                        },
                        {
                            "name": "size",
                            "in": "query",
                            "required": false,
                            "type": "integer",
                        }
                    ],
                }
            },
            "/ids": {
                "get": {
                    "responses": replies("Ids"),
                    "tags": [runtime.descriptor.uid],
                    "parameters": [
                        {
                            "name": "ids",
                            "in": "query",
                            "required": true,
                            "type": "array",
                            "collectionFormat": "multi",
                            "items": { "type": "string" },
                        }
                    ]
                }
            }
        }
    })
}

fn entry_definition(runtime: &LexiconRuntime) -> JsonValue {
    let mut properties = Map::new();

    for field in runtime.mapping_keys() {
        let schema = swagger_field(runtime.field_type(field).unwrap_or("text"));

        if runtime.is_array(field) {
            let mut array = Map::new();
            array.insert("type".into(), JsonValue::String("array".into()));
            array.insert("items".into(), schema);
            properties.insert(field.clone(), JsonValue::Object(array));
        } else {
            properties.insert(field.clone(), schema);
        }
    }

    JsonValue::Object({
        let mut map = Map::new();
        map.insert("properties".into(), JsonValue::Object(properties));
        map
    })
}

fn collection_definition(item: &str) -> JsonValue {
    let mut root = Map::new();
    let mut data_props = Map::new();
    let mut collection = Map::new();

    let mut entry_ref = Map::new();
    entry_ref.insert(
        "$ref".into(),
        JsonValue::String("#/definitions/Entry".into()),
    );

    let mut array = Map::new();
    array.insert("type".into(), JsonValue::String("array".into()));
    array.insert("items".into(), JsonValue::Object(entry_ref));

    collection.insert(item.into(), JsonValue::Object(array));
    data_props.insert(
        "data".into(),
        JsonValue::Object({
            let mut map = Map::new();
            map.insert("properties".into(), JsonValue::Object(collection));
            map
        }),
    );

    root.insert("properties".into(), JsonValue::Object(data_props));
    JsonValue::Object(root)
}

fn replies(name: &str) -> JsonValue {
    json!({
        "200": {
            "description": "Matching dictionary entries",
            "schema": {
                "$ref": format!("#/definitions/{name}")
            }
        },
        "400": {
            "description": "Missing or invalid parameter"
        }
    })
}

fn swagger_field(field_type: &str) -> JsonValue {
    match field_type {
        "boolean" => json!({ "type": "boolean" }),
        "date" => json!({ "type": "string", "format": "date-time" }),
        "float" => json!({ "type": "number", "format": "float" }),
        "integer" => json!({ "type": "integer" }),
        "keyword" => json!({ "type": "string" }),
        "short" => json!({ "type": "integer" }),
        "text" => json!({ "type": "string" }),
        _ => json!({ "type": "string" }),
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("failed to bind listener")]
    Bind(#[source] std::io::Error),
    #[error("http server error")]
    Server(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use kosh_core::index::elasticsearch::{ElasticsearchBackend, ElasticsearchConfig};
    use serde_json::json;
    use std::{collections::HashMap, sync::Arc};

    fn sample_config() -> Arc<KoshConfig> {
        Arc::new(KoshConfig::default())
    }

    fn sample_lexicon(uid: &str) -> Arc<LexiconDescriptor> {
        Arc::new(LexiconDescriptor {
            uid: uid.into(),
            pool: format!("kosh[{uid}]"),
            files: vec![],
            schema: json!({
                "mappings": {
                    "properties": {
                        "lemma": { "type": "text" },
                        "pos": { "type": "keyword" },
                        "senses": { "type": "text" }
                    },
                    "_meta": {
                        "_xpaths": {
                            "fields": {
                                "[senses]": "sense/text()"
                            }
                        }
                    }
                }
            }),
            attributes: HashMap::from([("language".into(), JsonValue::String("en".into()))]),
            spec_path: Default::default(),
            schema_path: Default::default(),
        })
    }

    fn mock_backend(endpoint: &str) -> ElasticsearchBackend {
        let config = ElasticsearchConfig {
            endpoint: endpoint.into(),
            username: None,
            password: None,
            ca_bundle: None,
            request_timeout_secs: 1,
            max_retries: 1,
            retry_backoff_secs: 1,
        };
        ElasticsearchBackend::new(config).expect("backend")
    }

    #[tokio::test]
    async fn app_state_builds_runtime() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));

        let state = AppState::new(config, vec![lexicon], vec!["match".into()], search);

        let runtime = state.lexicon("demo").await.expect("lexicon");
        assert_eq!(
            runtime.properties,
            vec!["lemma".to_string(), "pos".to_string(), "senses".to_string()]
        );
        assert!(runtime.is_array("senses"));
        assert_eq!(state.query_types, vec!["match"]);
    }

    #[test]
    fn swagger_field_defaults_to_string() {
        let value = super::swagger_field("custom");
        assert_eq!(value, json!({ "type": "string" }));
    }

    #[tokio::test]
    async fn lexicon_overview_contains_endpoints() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = AppState::new(config, vec![lexicon.clone()], vec!["match".into()], search);

        let runtime = state.lexicon("demo").await.expect("lexicon");
        let overview = lexicon_overview(&state, &runtime);
        assert!(overview.get("endpoints").is_some());
        assert!(overview.get("query_types").is_some());
        assert!(overview.get("properties").is_some());
    }

    #[tokio::test]
    async fn entry_definition_marks_array_field() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = AppState::new(config, vec![lexicon], vec!["match".into()], search);

        let runtime = state.lexicon("demo").await.expect("lexicon");
        let entry = super::entry_definition(&runtime);
        let props = entry
            .get("properties")
            .and_then(JsonValue::as_object)
            .expect("properties");
        let senses = props
            .get("senses")
            .and_then(JsonValue::as_object)
            .expect("senses schema");

        assert_eq!(senses.get("type"), Some(&JsonValue::String("array".into())));
        assert!(senses.get("items").is_some());
    }
}
