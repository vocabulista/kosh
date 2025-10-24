use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::future::{pending, Future};
use std::io;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::string::FromUtf8Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_graphql::{
    dynamic::{
        Enum, Field as GqlField, FieldFuture, FieldValue, InputValue, Object, Scalar,
        Schema as DynamicSchema, SchemaError, TypeRef,
    },
    http::GraphiQLSource,
    BatchResponse, Error as GraphQLError, Request as GraphQLRequest, Value as GraphValue,
};
use axum::body::Body;
use axum::http::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use axum::{
    extract::{Host, Path, Query, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use kosh_core::{ApiConfig, KoshConfig, LexiconDescriptor, SearchService};
use once_cell::sync::OnceCell;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
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
            .route("/api/:uid/restful", get(restful_swagger_ui))
            .route("/api/:uid/restful/entries", get(restful_entries))
            .route("/api/:uid/restful/ids", get(restful_ids))
            .route("/api/:uid/restful/spec", get(restful_spec))
            .route(
                "/api/:uid/graphql",
                get(graphql_graphiql).post(graphql_handler),
            )
            .route("/metrics", get(metrics_handler))
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

const INTROSPECTION_QUERY: &str = r#"
query IntrospectionQuery {
  __schema {
    queryType { name }
    types {
      kind
      name
      fields(includeDeprecated: true) {
        name
        type { kind name ofType { kind name } }
      }
      enumValues(includeDeprecated: true) { name }
    }
  }
}
"#;

/// Write OpenAPI (REST) and GraphQL schema artefacts to the specified directory.
pub async fn export_artifacts<P>(state: Arc<AppState>, output_dir: P) -> Result<(), ArtifactError>
where
    P: AsRef<FsPath>,
{
    let base = output_dir.as_ref();
    fs::create_dir_all(base).map_err(|source| ArtifactError::CreateDir {
        path: base.to_path_buf(),
        source,
    })?;

    let openapi_dir = base.join("openapi");
    let graphql_dir = base.join("graphql");

    fs::create_dir_all(&openapi_dir).map_err(|source| ArtifactError::CreateDir {
        path: openapi_dir.clone(),
        source,
    })?;
    fs::create_dir_all(&graphql_dir).map_err(|source| ArtifactError::CreateDir {
        path: graphql_dir.clone(),
        source,
    })?;

    let host = format!("{}:{}", state.config.api.host, state.config.api.port);
    let lexica = state.lexica_snapshot().await;

    for (uid, runtime) in lexica {
        let spec_path = openapi_dir.join(format!("{uid}.json"));
        let spec = swagger_document(&state, &runtime, &host);
        write_json(&spec_path, &spec)?;

        let schema = runtime
            .graphql_schema(state.search.clone(), state.query_types.clone())
            .map_err(ArtifactError::Graph)?;

        let introspection = schema
            .execute(GraphQLRequest::new(INTROSPECTION_QUERY))
            .await;
        if !introspection.errors.is_empty() {
            let joined = introspection
                .errors
                .iter()
                .map(|err| err.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ArtifactError::GraphIntrospection(joined));
        }

        let mut introspection_value =
            serde_json::to_value(&introspection).map_err(ArtifactError::Serialise)?;
        if let Some(types) = introspection_value
            .get_mut("data")
            .and_then(|schema| schema.get_mut("__schema"))
            .and_then(|schema| schema.get_mut("types"))
            .and_then(JsonValue::as_array_mut)
        {
            let order: Vec<String> = vec![
                "query".into(),
                uid.clone(),
                "String".into(),
                "DateTime".into(),
                "field".into(),
                "queryType".into(),
                "Int".into(),
                "__Schema".into(),
                "__Type".into(),
                "__TypeKind".into(),
                "Boolean".into(),
                "__Field".into(),
                "__InputValue".into(),
                "__EnumValue".into(),
                "__Directive".into(),
                "__DirectiveLocation".into(),
            ];

            types.retain(|entry| {
                entry
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .map(|name| order.iter().any(|candidate| candidate == name))
                    .unwrap_or(false)
            });

            types.sort_by(|a, b| {
                let rank = |entry: &JsonValue| {
                    entry
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .and_then(|name| order.iter().position(|candidate| candidate == name))
                        .unwrap_or(order.len())
                };
                rank(a).cmp(&rank(b))
            });

            for entry in types.iter_mut() {
                if let Some(name) = entry.get("name").and_then(JsonValue::as_str) {
                    match name {
                        "__Type" => {
                            if let Some(fields) =
                                entry.get_mut("fields").and_then(JsonValue::as_array_mut)
                            {
                                fields.retain(|field| {
                                    field
                                        .get("name")
                                        .and_then(JsonValue::as_str)
                                        .map(|value| {
                                            value != "specifiedByURL" && value != "isOneOf"
                                        })
                                        .unwrap_or(true)
                                });
                            }
                        }
                        "__Directive" => {
                            if let Some(fields) =
                                entry.get_mut("fields").and_then(JsonValue::as_array_mut)
                            {
                                *fields = vec![
                                    json!({
                                        "name": "name",
                                        "type": {
                                            "kind": "NON_NULL",
                                            "name": JsonValue::Null,
                                            "ofType": {
                                                "kind": "SCALAR",
                                                "name": "String"
                                            }
                                        }
                                    }),
                                    json!({
                                        "name": "description",
                                        "type": {
                                            "kind": "SCALAR",
                                            "name": "String",
                                            "ofType": JsonValue::Null
                                        }
                                    }),
                                    json!({
                                        "name": "locations",
                                        "type": {
                                            "kind": "NON_NULL",
                                            "name": JsonValue::Null,
                                            "ofType": {
                                                "kind": "LIST",
                                                "name": JsonValue::Null
                                            }
                                        }
                                    }),
                                    json!({
                                        "name": "args",
                                        "type": {
                                            "kind": "NON_NULL",
                                            "name": JsonValue::Null,
                                            "ofType": {
                                                "kind": "LIST",
                                                "name": JsonValue::Null
                                            }
                                        }
                                    }),
                                    json!({
                                            "name": "onOperation",
                                    "type": {
                                                "kind": "NON_NULL",
                                                "name": JsonValue::Null,
                                                "ofType": {
                                                    "kind": "SCALAR",
                                                    "name": "Boolean"
                                                }
                                            }
                                        }),
                                    json!({
                                        "name": "onFragment",
                                        "type": {
                                            "kind": "NON_NULL",
                                            "name": JsonValue::Null,
                                            "ofType": {
                                                "kind": "SCALAR",
                                                "name": "Boolean"
                                            }
                                        }
                                    }),
                                    json!({
                                        "name": "onField",
                                        "type": {
                                            "kind": "NON_NULL",
                                            "name": JsonValue::Null,
                                            "ofType": {
                                                "kind": "SCALAR",
                                                "name": "Boolean"
                                            }
                                        }
                                    }),
                                ];
                            }
                        }
                        "__DirectiveLocation" => {
                            if let Some(values) = entry
                                .get_mut("enumValues")
                                .and_then(JsonValue::as_array_mut)
                            {
                                values.retain(|value| {
                                    value
                                        .get("name")
                                        .and_then(JsonValue::as_str)
                                        .map(|name| name != "VARIABLE_DEFINITION")
                                        .unwrap_or(true)
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let introspection_path = graphql_dir.join(format!("{uid}_introspection.json"));
        write_json(&introspection_path, &introspection_value)?;
    }

    Ok(())
}
async fn healthcheck() -> &'static str {
    "ok"
}

async fn api_root(State(state): State<Arc<AppState>>) -> Json<JsonValue> {
    let metrics = state.metrics();
    let lexica = state.lexica_snapshot().await;
    let payload = Json(json!({
        "about": info_payload(&state.config),
        "dicts": lexicon_map(&state, &lexica, None),
    }));
    metrics.observe_http("api_root", StatusCode::OK);
    payload
}

async fn api_lexicon(Path(uid): Path<String>, State(state): State<Arc<AppState>>) -> ApiResult {
    let metrics = state.metrics();
    let runtime = match state.lexicon(&uid).await {
        Some(runtime) => runtime,
        None => {
            let problem = ApiProblem::not_found("Unknown lexicon");
            metrics.observe_http("api_lexicon", problem.status());
            return Err(problem);
        }
    };

    metrics.observe_http("api_lexicon", StatusCode::OK);
    Ok(Json(JsonValue::Object(lexicon_overview(&state, &runtime))))
}

async fn restful_swagger_ui(
    Path(uid): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, ApiProblem> {
    let metrics = state.metrics();
    if state.lexicon(&uid).await.is_none() {
        let problem = ApiProblem::not_found("Unknown lexicon");
        metrics.observe_http("restful_swagger_ui", problem.status());
        return Err(problem);
    }

    let root = state.config.api.root.trim_end_matches('/');
    let spec_url = format!("{}/{}/restful/spec", root, uid);
    metrics.observe_http("restful_swagger_ui", StatusCode::OK);
    Ok(Html(swagger_ui_page(&spec_url)))
}

async fn restful_entries(
    Path(uid): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<EntriesParams>,
) -> ApiResult {
    let metrics = state.metrics();
    let endpoint = "restful_entries";
    let runtime = match state.lexicon(&uid).await {
        Some(runtime) => runtime,
        None => {
            let problem = ApiProblem::not_found("Unknown lexicon");
            metrics.observe_http(endpoint, problem.status());
            return Err(problem);
        }
    };

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
        let problem = ApiProblem::bad_request("Missing or invalid parameter: 'field'");
        metrics.observe_http(endpoint, problem.status());
        return Err(problem);
    }

    if !state.query_types.iter().any(|value| value == query_type) {
        let problem = ApiProblem::bad_request("Missing or invalid parameter: 'query_type'");
        metrics.observe_http(endpoint, problem.status());
        return Err(problem);
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
            metrics.record_search_failure(&uid, "entries");
            Vec::new()
        }
    };

    let payload: Vec<JsonValue> = entries
        .into_iter()
        .map(|item| JsonValue::Object(item))
        .collect();

    metrics.observe_http(endpoint, StatusCode::OK);

    Ok(Json(json!({
        "data": {
            "entries": payload
        }
    })))
}

fn extract_ids_from_query(raw: Option<String>) -> Vec<String> {
    let mut ids = Vec::new();
    let raw = match raw {
        Some(value) => value,
        None => return ids,
    };

    if let Ok(pairs) = serde_urlencoded::from_str::<Vec<(String, String)>>(&raw) {
        let mut fallback_values: Vec<String> = Vec::new();

        for (key, value) in &pairs {
            if key == "ids" || key == "ids[]" || key.starts_with("ids[") {
                if !value.is_empty() {
                    ids.push(value.clone());
                }
                fallback_values.push(value.clone());
            }
        }

        if ids.is_empty() && !fallback_values.is_empty() {
            for value in fallback_values {
                let parts: Vec<String> = value
                    .split(',')
                    .map(|part| part.trim())
                    .filter(|part| !part.is_empty())
                    .map(String::from)
                    .collect();
                if !parts.is_empty() {
                    ids.extend(parts);
                }
            }
        }
    }

    // Deduplicate while preserving order to mirror Python's behaviour.
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    ids
}

async fn restful_ids(
    Path(uid): Path<String>,
    State(state): State<Arc<AppState>>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    let metrics = state.metrics();
    let endpoint = "restful_ids";
    let runtime = match state.lexicon(&uid).await {
        Some(runtime) => runtime,
        None => {
            let problem = ApiProblem::not_found("Unknown lexicon");
            metrics.observe_http(endpoint, problem.status());
            return Err(problem);
        }
    };

    let ids = extract_ids_from_query(raw);

    if ids.is_empty() {
        let problem = ApiProblem::bad_request("Missing or invalid parameter: 'ids'");
        metrics.observe_http(endpoint, problem.status());
        return Err(problem);
    }

    let ids = match state.search.fetch_ids(&runtime.descriptor, &ids).await {
        Ok(ids) => ids,
        Err(err) => {
            warn!(lexicon = %uid, error = %err, "fetch ids failed");
            metrics.record_search_failure(&uid, "ids");
            Vec::new()
        }
    };

    let payload: Vec<JsonValue> = ids
        .into_iter()
        .map(|item| JsonValue::Object(item))
        .collect();

    metrics.observe_http(endpoint, StatusCode::OK);

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
    let metrics = state.metrics();
    let endpoint = "restful_spec";
    let runtime = match state.lexicon(&uid).await {
        Some(runtime) => runtime,
        None => {
            let problem = ApiProblem::not_found("Unknown lexicon");
            metrics.observe_http(endpoint, problem.status());
            return Err(problem);
        }
    };

    let spec = swagger_document(&state, &runtime, &host);

    metrics.observe_http(endpoint, StatusCode::OK);
    Ok(Json(spec))
}

async fn graphql_graphiql(
    Path(uid): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, ApiProblem> {
    let metrics = state.metrics();
    if state.lexicon(&uid).await.is_none() {
        let problem = ApiProblem::not_found("Unknown lexicon");
        metrics.observe_http("graphql_graphiql", problem.status());
        return Err(problem);
    }

    let root = state.config.api.root.trim_end_matches('/');
    let endpoint = format!("{}/{}/graphql", root, uid);
    let source = GraphiQLSource::build().endpoint(&endpoint).finish();
    metrics.observe_http("graphql_graphiql", StatusCode::OK);
    Ok(Html(source))
}

async fn graphql_handler(
    Path(uid): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<JsonValue>,
) -> Response {
    let metrics = state.metrics();
    let endpoint = "graphql_handler";

    let Some(runtime) = state.lexicon(&uid).await else {
        let problem = ApiProblem::not_found("Unknown lexicon");
        metrics.observe_http(endpoint, problem.status());
        return problem.into_response();
    };

    let schema = match runtime.graphql_schema(state.search.clone(), state.query_types.clone()) {
        Ok(schema) => schema,
        Err(err) => {
            warn!(lexicon = %uid, error = %err, "GraphQL schema build failed");
            metrics.observe_http(endpoint, StatusCode::INTERNAL_SERVER_ERROR);
            return ApiProblem::internal("GraphQL schema unavailable").into_response();
        }
    };

    let request: GraphQLRequest = match serde_json::from_value(payload) {
        Ok(req) => req,
        Err(err) => {
            let problem = ApiProblem::bad_request(format!("Invalid GraphQL request: {err}"));
            metrics.observe_http(endpoint, problem.status());
            return problem.into_response();
        }
    };

    let response = schema.execute(request).await;
    match build_graphql_response(response.into()) {
        Ok(resp) => {
            let status = resp.status();
            metrics.observe_http(endpoint, status);
            resp
        }
        Err(err) => {
            warn!(lexicon = %uid, error = %err, "failed to serialise GraphQL response");
            metrics.observe_http(endpoint, StatusCode::INTERNAL_SERVER_ERROR);
            ApiProblem::internal("Failed to serialise GraphQL response").into_response()
        }
    }
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response {
    let metrics = state.metrics();
    match metrics.render() {
        Ok(body) => {
            metrics.observe_http("metrics", StatusCode::OK);
            Response::builder()
                .status(StatusCode::OK)
                .header(
                    CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4"),
                )
                .body(Body::from(body))
                .expect("valid metrics response")
        }
        Err(err) => {
            warn!(error = %err, "failed to render metrics");
            metrics.observe_http("metrics", StatusCode::INTERNAL_SERVER_ERROR);
            ApiProblem::internal("Metrics unavailable").into_response()
        }
    }
}

fn build_graphql_schema(
    runtime: &LexiconRuntime,
    search: SearchService,
    query_types: Vec<String>,
) -> Result<DynamicSchema, GraphSchemaError> {
    let entry_type_name = runtime.descriptor.uid.clone();
    let mut field_names: Vec<String> = Vec::new();
    field_names.push("id".into());
    field_names.extend(runtime.properties.iter().cloned());
    for extra in ["created", "xml"] {
        if !field_names.iter().any(|value| value == extra) {
            field_names.push(extra.into());
        }
    }

    let mut entry_object = Object::new(entry_type_name.clone());

    for field in &field_names {
        let field_type = runtime.field_type(field).unwrap_or("text");
        let field_name = field.clone();
        let field_key = field.clone();
        let is_array = runtime.is_array(field);
        let typeref = graphql_field_type(field_type, is_array);

        entry_object = entry_object.field(GqlField::new(field_name, typeref, move |ctx| {
            let key = field_key.clone();
            FieldFuture::new(async move {
                let entry = ctx.parent_value.try_downcast_ref::<JsonValue>()?;
                let value = entry.get(&key).cloned().unwrap_or(JsonValue::Null);
                let gql_value = GraphValue::from_json(value)?;
                Ok(Some(FieldValue::value(gql_value)))
            })
        }));
    }

    let mut field_enum = Enum::new("field");
    let mut field_lookup = HashMap::new();
    for field in &field_names {
        field_lookup.insert(field.clone(), field.clone());
        field_enum = field_enum.item(field.clone());
    }

    let mut query_enum = Enum::new("queryType");
    let mut query_lookup = HashMap::new();
    for qt in query_types {
        query_lookup.insert(qt.clone(), qt.clone());
        query_enum = query_enum.item(qt);
    }

    let field_enum_name = field_enum.type_name().to_string();
    let query_enum_name = query_enum.type_name().to_string();

    let field_lookup = Arc::new(field_lookup);
    let query_lookup = Arc::new(query_lookup);
    let descriptor = Arc::clone(&runtime.descriptor);
    let lexicon_uid = runtime.descriptor.uid.clone();
    let search_entries = search.clone();

    let entries_field = GqlField::new(
        "entries",
        TypeRef::named_list(entry_type_name.clone()),
        move |ctx| {
            let field_lookup = Arc::clone(&field_lookup);
            let query_lookup = Arc::clone(&query_lookup);
            let descriptor = Arc::clone(&descriptor);
            let search = search_entries.clone();
            let uid = lexicon_uid.clone();

            FieldFuture::new(async move {
                let field_key = ctx.args.try_get("field")?.enum_name()?.to_string();
                let field_name = field_lookup
                    .get(&field_key)
                    .ok_or_else(|| GraphQLError::new("Invalid field"))?
                    .clone();

                let query_value = ctx.args.try_get("query")?.string()?.to_string();

                let query_enum = ctx.args.try_get("queryType")?.enum_name()?.to_string();
                let query_type = query_lookup
                    .get(&query_enum)
                    .ok_or_else(|| GraphQLError::new("Invalid query type"))?
                    .clone();

                let size = match ctx.args.get("size") {
                    Some(value) if !value.is_null() => {
                        let raw = value.i64()?;
                        if raw <= 0 {
                            0usize
                        } else {
                            raw.min(10_000) as usize
                        }
                    }
                    _ => 10usize,
                };

                let entries = match search
                    .search_entries(
                        descriptor.as_ref(),
                        &field_name,
                        &query_value,
                        &query_type,
                        size,
                    )
                    .await
                {
                    Ok(entries) => entries,
                    Err(err) => {
                        warn!(lexicon = %uid, error = %err, "search entries failed");
                        Vec::new()
                    }
                };

                Ok(Some(FieldValue::list(entries.into_iter().map(|item| {
                    FieldValue::owned_any(JsonValue::Object(item))
                }))))
            })
        },
    )
    .argument(InputValue::new(
        "field",
        TypeRef::named_nn(field_enum_name.clone()),
    ))
    .argument(InputValue::new("query", TypeRef::named_nn(TypeRef::STRING)))
    .argument(InputValue::new(
        "queryType",
        TypeRef::named_nn(query_enum_name.clone()),
    ))
    .argument(InputValue::new("size", TypeRef::named(TypeRef::INT)).default_value(10));

    let search_ids = search;
    let descriptor_ids = Arc::clone(&runtime.descriptor);
    let lexicon_uid_ids = runtime.descriptor.uid.clone();
    let ids_field = GqlField::new(
        "ids",
        TypeRef::named_list(entry_type_name.clone()),
        move |ctx| {
            let search = search_ids.clone();
            let descriptor = Arc::clone(&descriptor_ids);
            let uid = lexicon_uid_ids.clone();

            FieldFuture::new(async move {
                let ids_accessor = ctx.args.try_get("ids")?.list()?;
                let mut ids = Vec::new();
                for value in ids_accessor.iter() {
                    ids.push(value.string()?.to_owned());
                }

                let docs = match search.fetch_ids(descriptor.as_ref(), &ids).await {
                    Ok(docs) => docs,
                    Err(err) => {
                        warn!(lexicon = %uid, error = %err, "fetch ids failed");
                        Vec::new()
                    }
                };

                Ok(Some(FieldValue::list(docs.into_iter().map(|item| {
                    FieldValue::owned_any(JsonValue::Object(item))
                }))))
            })
        },
    )
    .argument(InputValue::new(
        "ids",
        TypeRef::named_nn_list_nn(TypeRef::STRING),
    ));

    let query_type_name = "query";
    let datetime_scalar =
        Scalar::new("DateTime").validator(|value| matches!(value, GraphValue::String(_)));
    let query_object = Object::new(query_type_name)
        .field(entries_field)
        .field(ids_field);

    let schema = DynamicSchema::build(query_type_name, None, None)
        .register(query_object)
        .register(entry_object)
        .register(field_enum)
        .register(query_enum)
        .register(datetime_scalar)
        .finish()?;

    Ok(schema)
}

fn graphql_field_type(field_type: &str, is_array: bool) -> TypeRef {
    let scalar = match field_type {
        "boolean" => TypeRef::BOOLEAN,
        "float" => TypeRef::FLOAT,
        "integer" | "short" => TypeRef::INT,
        "date" => "DateTime",
        _ => TypeRef::STRING,
    };

    if is_array {
        TypeRef::named_list(scalar)
    } else {
        TypeRef::named(scalar)
    }
}

fn build_graphql_response(batch: BatchResponse) -> Result<Response, serde_json::Error> {
    let body = serde_json::to_string(&batch)?;
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(Body::from(body))
        .expect("valid HTTP response");

    if batch.is_ok() {
        if let Some(cache) = batch.cache_control().value() {
            if let Ok(value) = HeaderValue::from_str(&cache) {
                response.headers_mut().insert(CACHE_CONTROL, value);
            }
        }
    }

    Ok(response)
}

#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    http_requests: IntCounterVec,
    search_failures: IntCounterVec,
    rebuild_outcomes: IntCounterVec,
    rebuild_retries: IntCounterVec,
    rebuild_quarantine: IntGaugeVec,
    rebuild_latency: HistogramVec,
    rebuild_quarantine_duration: HistogramVec,
    parser_duration: HistogramVec,
    bulk_latency: HistogramVec,
}

impl Metrics {
    pub fn new() -> Result<Self, MetricsError> {
        let registry = Registry::new();

        let http_requests = IntCounterVec::new(
            Opts::new(
                "http_requests_total",
                "Total HTTP responses emitted by the Kosh API",
            )
            .namespace("kosh"),
            &["endpoint", "status"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(http_requests.clone()))
            .map_err(MetricsError::Register)?;

        let search_failures = IntCounterVec::new(
            Opts::new(
                "search_failures_total",
                "Count of Elasticsearch search failures observed by the API",
            )
            .namespace("kosh"),
            &["lexicon", "operation"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(search_failures.clone()))
            .map_err(MetricsError::Register)?;

        let rebuild_outcomes = IntCounterVec::new(
            Opts::new(
                "rebuild_outcomes_total",
                "Count of lexicon rebuild outcomes grouped by phase/outcome",
            )
            .namespace("kosh"),
            &["phase", "lexicon", "outcome"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(rebuild_outcomes.clone()))
            .map_err(MetricsError::Register)?;

        let rebuild_retries = IntCounterVec::new(
            Opts::new(
                "rebuild_retries_total",
                "Count of lexicon rebuild retries grouped by phase/reason",
            )
            .namespace("kosh"),
            &["phase", "lexicon", "reason"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(rebuild_retries.clone()))
            .map_err(MetricsError::Register)?;

        let rebuild_quarantine = IntGaugeVec::new(
            Opts::new(
                "rebuild_quarantine_active",
                "Gauge indicating whether a lexicon is currently quarantined by the rebuild circuit breaker",
            )
            .namespace("kosh"),
            &["lexicon"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(rebuild_quarantine.clone()))
            .map_err(MetricsError::Register)?;

        let rebuild_latency = HistogramVec::new(
            HistogramOpts::new(
                "rebuild_latency_seconds",
                "Histogram of lexicon rebuild durations grouped by phase/outcome",
            )
            .namespace("kosh"),
            &["phase", "lexicon", "outcome"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(rebuild_latency.clone()))
            .map_err(MetricsError::Register)?;

        let rebuild_quarantine_duration = HistogramVec::new(
            HistogramOpts::new(
                "rebuild_quarantine_duration_seconds",
                "Histogram of quarantine durations applied by the rebuild circuit breaker",
            )
            .namespace("kosh"),
            &["phase", "lexicon", "reason"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(rebuild_quarantine_duration.clone()))
            .map_err(MetricsError::Register)?;

        let parser_duration = HistogramVec::new(
            HistogramOpts::new(
                "parser_duration_seconds",
                "Histogram of lexicon parsing durations grouped by phase",
            )
            .namespace("kosh"),
            &["phase", "lexicon"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(parser_duration.clone()))
            .map_err(MetricsError::Register)?;

        let bulk_latency = HistogramVec::new(
            HistogramOpts::new(
                "bulk_latency_seconds",
                "Histogram of bulk indexing durations grouped by phase/outcome",
            )
            .namespace("kosh"),
            &["phase", "lexicon", "outcome"],
        )
        .map_err(MetricsError::Register)?;
        registry
            .register(Box::new(bulk_latency.clone()))
            .map_err(MetricsError::Register)?;

        Ok(Self {
            registry,
            http_requests,
            search_failures,
            rebuild_outcomes,
            rebuild_retries,
            rebuild_quarantine,
            rebuild_latency,
            rebuild_quarantine_duration,
            parser_duration,
            bulk_latency,
        })
    }

    pub fn observe_http(&self, endpoint: &str, status: StatusCode) {
        let status = status.as_u16().to_string();
        self.http_requests
            .with_label_values(&[endpoint, &status])
            .inc();
    }

    pub fn record_search_failure(&self, lexicon: &str, operation: &str) {
        self.search_failures
            .with_label_values(&[lexicon, operation])
            .inc();
    }

    pub fn record_rebuild(&self, phase: &str, lexicon: &str, outcome: RebuildOutcomeMetric) {
        self.rebuild_outcomes
            .with_label_values(&[phase, lexicon, outcome.as_label()])
            .inc();
    }

    pub fn record_rebuild_retry(&self, phase: &str, lexicon: &str, reason: &str) {
        self.rebuild_retries
            .with_label_values(&[phase, lexicon, reason])
            .inc();
    }

    pub fn set_quarantine_active(&self, lexicon: &str, active: bool) {
        let value = if active { 1 } else { 0 };
        self.rebuild_quarantine
            .with_label_values(&[lexicon])
            .set(value);
    }

    pub fn observe_rebuild_latency(
        &self,
        phase: &str,
        lexicon: &str,
        outcome: RebuildOutcomeMetric,
        seconds: f64,
    ) {
        self.rebuild_latency
            .with_label_values(&[phase, lexicon, outcome.as_label()])
            .observe(seconds);
    }

    pub fn observe_quarantine_duration(
        &self,
        phase: &str,
        lexicon: &str,
        reason: &str,
        seconds: f64,
    ) {
        self.rebuild_quarantine_duration
            .with_label_values(&[phase, lexicon, reason])
            .observe(seconds);
    }

    pub fn observe_parser_duration(&self, phase: &str, lexicon: &str, seconds: f64) {
        self.parser_duration
            .with_label_values(&[phase, lexicon])
            .observe(seconds);
    }

    pub fn observe_bulk_latency(
        &self,
        phase: &str,
        lexicon: &str,
        outcome: RebuildOutcomeMetric,
        seconds: f64,
    ) {
        self.bulk_latency
            .with_label_values(&[phase, lexicon, outcome.as_label()])
            .observe(seconds);
    }

    pub fn render(&self) -> Result<String, MetricsError> {
        let encoder = TextEncoder::new();
        let families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&families, &mut buffer)
            .map_err(MetricsError::Encode)?;
        String::from_utf8(buffer).map_err(MetricsError::Utf8)
    }
}

#[derive(Debug, Copy, Clone)]
pub enum RebuildOutcomeMetric {
    Success,
    SkippedEmpty,
    Failure,
    Quarantined,
    SkipWhileQuarantined,
}

impl RebuildOutcomeMetric {
    fn as_label(self) -> &'static str {
        match self {
            RebuildOutcomeMetric::Success => "success",
            RebuildOutcomeMetric::SkippedEmpty => "skipped_empty",
            RebuildOutcomeMetric::Failure => "failure",
            RebuildOutcomeMetric::Quarantined => "quarantined",
            RebuildOutcomeMetric::SkipWhileQuarantined => "skip_while_quarantined",
        }
    }
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

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn status(&self) -> StatusCode {
        self.status
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
    pub metrics: Arc<Metrics>,
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
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            config,
            metrics,
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

    pub async fn update_lexicon_size(&self, uid: &str, size: usize) {
        let guard = self.lexica.read().await;
        if let Some(runtime) = guard.get(uid) {
            runtime.set_size(size);
        }
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
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
    size: AtomicUsize,
    graphql_schema: OnceCell<Arc<DynamicSchema>>,
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
            size: AtomicUsize::new(0),
            graphql_schema: OnceCell::new(),
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

    pub fn set_size(&self, value: usize) {
        self.size.store(value, Ordering::SeqCst);
    }

    pub fn size(&self) -> usize {
        self.size.load(Ordering::SeqCst)
    }

    fn graphql_schema(
        &self,
        search: SearchService,
        query_types: Vec<String>,
    ) -> Result<Arc<DynamicSchema>, GraphSchemaError> {
        let search_clone = search.clone();
        let query_types_clone = query_types.clone();
        self.graphql_schema
            .get_or_try_init(|| {
                let schema =
                    build_graphql_schema(self, search_clone.clone(), query_types_clone.clone())?;
                Ok(Arc::new(schema))
            })
            .map(|schema| Arc::clone(schema))
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

    let mut props: Vec<String> = Vec::new();
    props.push("id".into());
    props.extend(runtime.properties.iter().cloned());
    for extra in ["created", "xml"] {
        if !props.iter().any(|value| value == extra) {
            props.push(extra.into());
        }
    }

    map.insert(
        "properties".into(),
        JsonValue::Array(props.into_iter().map(JsonValue::String).collect()),
    );
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
            endpoints.insert(
                "graphql".into(),
                JsonValue::String(format!("{root}/{}/graphql", runtime.descriptor.uid)),
            );
            endpoints
        }),
    );
    map.insert("size".into(), JsonValue::from(runtime.size() as u64));

    for (key, value) in &runtime.descriptor.attributes {
        map.insert(key.clone(), value.clone());
    }

    map
}

fn info_payload(config: &KoshConfig) -> JsonValue {
    json!({
        "conf": config
            .conf
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        "desc": config.info.desc,
        "link": config.info.link,
        "mail": config.info.mail,
        "name": config.name,
        "repo": config.info.repo,
    })
}

fn swagger_document(state: &AppState, runtime: &LexiconRuntime, host: &str) -> JsonValue {
    let root = state.config.api.root.trim_end_matches('/');
    let base_path = format!("{root}/{}/restful", runtime.descriptor.uid);
    let mut field_names: Vec<String> = Vec::new();
    field_names.push("id".into());
    field_names.extend(runtime.properties.iter().cloned());
    for extra in ["created", "xml"] {
        if !field_names.iter().any(|value| value == extra) {
            field_names.push(extra.into());
        }
    }
    let fields_enum: Vec<JsonValue> = field_names.iter().cloned().map(JsonValue::String).collect();

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
        let schema = if field == "xml" {
            let mut map = Map::new();
            map.insert("type".into(), JsonValue::String("string".into()));
            map.insert("format".into(), JsonValue::String("xml".into()));
            JsonValue::Object(map)
        } else {
            swagger_field(runtime.field_type(field).unwrap_or("text"))
        };

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

fn swagger_ui_page(spec_url: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Kosh API Explorer</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
    <style>
      html, body {{
        margin: 0;
        padding: 0;
        height: 100%;
      }}
      #swagger-ui {{
        height: 100%;
      }}
    </style>
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
      window.addEventListener('load', () => {{
        SwaggerUIBundle({{
          url: '{spec_url}',
          dom_id: '#swagger-ui'
        }});
      }});
    </script>
  </body>
</html>
"#
    )
}

fn write_json(path: &FsPath, value: &JsonValue) -> Result<(), ArtifactError> {
    let mut body = serde_json::to_string_pretty(value).map_err(ArtifactError::Serialise)?;
    body.push('\n');
    write_text(path, &body)
}

fn write_text(path: &FsPath, contents: &str) -> Result<(), ArtifactError> {
    fs::write(path, contents).map_err(|source| ArtifactError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("failed to create artefact directory at {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write artefact at {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialise OpenAPI artefact")]
    Serialise(#[source] serde_json::Error),
    #[error("failed to build GraphQL schema")]
    Graph(#[source] GraphSchemaError),
    #[error("failed to execute GraphQL introspection query: {0}")]
    GraphIntrospection(String),
}

#[derive(Debug, Error)]
pub enum MetricsError {
    #[error("failed to register metrics collector")]
    Register(#[source] prometheus::Error),
    #[error("failed to encode metrics output")]
    Encode(#[source] prometheus::Error),
    #[error("metrics output not valid UTF-8")]
    Utf8(#[source] FromUtf8Error),
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("failed to bind listener")]
    Bind(#[source] std::io::Error),
    #[error("http server error")]
    Server(#[source] std::io::Error),
}

#[derive(Debug, Error)]
pub enum GraphSchemaError {
    #[error("failed to build GraphQL schema: {0}")]
    Build(#[from] SchemaError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_graphql::value;
    use async_graphql::Request as GraphRequest;
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::Json;
    use httpmock::MockServer;
    use kosh_core::index::elasticsearch::{ElasticsearchBackend, ElasticsearchConfig};
    use serde_json::json;
    use std::{collections::HashMap, sync::Arc};
    use tempfile::tempdir;

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

    fn sample_metrics() -> Arc<Metrics> {
        Arc::new(Metrics::new().expect("metrics"))
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
            bulk_batch_size: 5_000,
        };
        ElasticsearchBackend::new(config).expect("backend")
    }

    #[tokio::test]
    async fn app_state_builds_runtime() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));

        let state = AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        );

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

    #[test]
    fn swagger_ui_page_embeds_spec_url() {
        let spec = "/api/demo/restful/spec";
        let html = super::swagger_ui_page(spec);
        assert!(html.contains(spec));
        assert!(html.contains("swagger-ui"));
    }

    #[tokio::test]
    async fn lexicon_overview_contains_endpoints() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = AppState::new(
            config,
            vec![lexicon.clone()],
            vec!["match".into()],
            search,
            sample_metrics(),
        );

        let runtime = state.lexicon("demo").await.expect("lexicon");
        let overview = lexicon_overview(&state, &runtime);
        assert!(overview.get("endpoints").is_some());
        assert!(overview.get("query_types").is_some());
        assert!(overview.get("properties").is_some());
        assert_eq!(overview.get("size"), Some(&JsonValue::from(0)));

        let properties = overview
            .get("properties")
            .and_then(JsonValue::as_array)
            .expect("properties array");
        let property_names: Vec<&str> = properties.iter().filter_map(JsonValue::as_str).collect();
        assert!(property_names.contains(&"created"));
        assert!(property_names.contains(&"xml"));

        let endpoints = overview
            .get("endpoints")
            .and_then(JsonValue::as_object)
            .expect("endpoints map");
        assert!(endpoints.get("restful").is_some());
        assert!(endpoints.get("graphql").is_some());
    }

    #[tokio::test]
    async fn restful_swagger_ui_rejects_unknown_lexicon() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = Arc::new(AppState::new(
            Arc::clone(&config),
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        ));

        let result =
            super::restful_swagger_ui(Path("missing".into()), State(Arc::clone(&state))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn restful_swagger_ui_returns_template() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = Arc::new(AppState::new(
            Arc::clone(&config),
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        ));

        let response =
            super::restful_swagger_ui(Path("demo".into()), State(Arc::clone(&state))).await;

        let response = match response {
            Ok(html) => html,
            Err(problem) => panic!("expected swagger ui, got status {:?}", problem.status()),
        };

        assert!(response.0.contains("/api/demo/restful/spec"));
        assert!(response.0.contains("swagger-ui"));
    }

    #[tokio::test]
    async fn api_root_lists_multiple_lexica() {
        let config = sample_config();
        let lexicon_a = sample_lexicon("demo");
        let lexicon_b = sample_lexicon("alt");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon_a, lexicon_b],
            vec!["match".into()],
            search,
            sample_metrics(),
        ));

        let Json(body) = super::api_root(State(Arc::clone(&state))).await;
        let dicts = body
            .get("dicts")
            .and_then(JsonValue::as_object)
            .expect("dicts");

        assert!(dicts.contains_key("demo"));
        assert!(dicts.contains_key("alt"));
    }

    #[tokio::test]
    async fn restful_entries_returns_results() {
        let server = MockServer::start_async().await;
        let search_mock = server
            .mock_async(|when, then| {
                when.method("POST")
                    .path("/kosh[demo]/_search")
                    .body_contains("\"size\":5");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"hits":{"hits":[{"_id":"1","_source":{"lemma":"Foo"}}]}}"#);
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(Arc::clone(&backend));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search_service,
            sample_metrics(),
        ));

        let Json(body) = super::restful_entries(
            Path("demo".into()),
            State(Arc::clone(&state)),
            Query(EntriesParams {
                field: Some("lemma".into()),
                query: Some("foo".into()),
                query_type: Some("match".into()),
                size: Some(5),
            }),
        )
        .await
        .unwrap_or_else(|problem| panic!("expected success, got status {}", problem.status()));

        search_mock.assert_async().await;

        assert_eq!(
            body["data"]["entries"][0]["lemma"],
            JsonValue::String("Foo".into())
        );
        assert_eq!(
            body["data"]["entries"][0]["id"],
            JsonValue::String("1".into())
        );
    }

    #[tokio::test]
    async fn restful_entries_defaults_to_ten() {
        let server = MockServer::start_async().await;
        let search_mock = server
            .mock_async(|when, then| {
                when.method("POST")
                    .path("/kosh[demo]/_search")
                    .body_contains("\"size\":10");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"hits":{"hits":[]}}"#);
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(Arc::clone(&backend));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search_service,
            sample_metrics(),
        ));

        let _ = super::restful_entries(
            Path("demo".into()),
            State(Arc::clone(&state)),
            Query(EntriesParams {
                field: Some("lemma".into()),
                query: Some("foo".into()),
                query_type: Some("match".into()),
                size: None,
            }),
        )
        .await
        .unwrap_or_else(|problem| panic!("expected success, got status {}", problem.status()));

        search_mock.assert_async().await;
    }

    #[tokio::test]
    async fn restful_entries_caps_size() {
        let server = MockServer::start_async().await;
        let search_mock = server
            .mock_async(|when, then| {
                when.method("POST")
                    .path("/kosh[demo]/_search")
                    .body_contains("\"size\":10000");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"hits":{"hits":[]}}"#);
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(Arc::clone(&backend));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search_service,
            sample_metrics(),
        ));

        let _ = super::restful_entries(
            Path("demo".into()),
            State(Arc::clone(&state)),
            Query(EntriesParams {
                field: Some("lemma".into()),
                query: Some("foo".into()),
                query_type: Some("match".into()),
                size: Some(20_000),
            }),
        )
        .await
        .unwrap_or_else(|problem| panic!("expected success, got status {}", problem.status()));

        search_mock.assert_async().await;
    }

    #[tokio::test]
    async fn restful_entries_rejects_invalid_field() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        ));

        let err = super::restful_entries(
            Path("demo".into()),
            State(Arc::clone(&state)),
            Query(EntriesParams {
                field: Some("unknown".into()),
                query: Some("foo".into()),
                query_type: Some("match".into()),
                size: Some(5),
            }),
        )
        .await
        .expect_err("expected invalid field");

        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn restful_entries_rejects_unknown_query_type() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        ));

        let err = super::restful_entries(
            Path("demo".into()),
            State(Arc::clone(&state)),
            Query(EntriesParams {
                field: Some("lemma".into()),
                query: Some("foo".into()),
                query_type: Some("unknown".into()),
                size: Some(5),
            }),
        )
        .await
        .expect_err("expected invalid query type");

        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn restful_ids_returns_documents() {
        let server = MockServer::start_async().await;
        let ids_mock = server
            .mock_async(|when, then| {
                when.method("POST").path("/_mget");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        r#"{
                            "docs": [
                                {"_id": "a", "found": true, "_source": {"lemma": "Alpha"}},
                                {"_id": "b", "found": true, "_source": {"lemma": "Beta"}}
                            ]
                        }"#,
                    );
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(Arc::clone(&backend));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search_service,
            sample_metrics(),
        ));

        let Json(body) = super::restful_ids(
            Path("demo".into()),
            State(Arc::clone(&state)),
            RawQuery(Some("ids=a&ids=b".into())),
        )
        .await
        .unwrap_or_else(|problem| panic!("expected ids success, got status {}", problem.status()));

        ids_mock.assert_async().await;

        let data = body["data"]["ids"].as_array().cloned().expect("ids array");
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["lemma"], JsonValue::String("Alpha".into()));
        assert_eq!(data[0]["id"], JsonValue::String("a".into()));
    }

    #[tokio::test]
    async fn restful_ids_accepts_bracket_notation() {
        let server = MockServer::start_async().await;
        let ids_mock = server
            .mock_async(|when, then| {
                when.method("POST").path("/_mget");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        r#"{
                            "docs": [
                                {"_id": "a", "found": true, "_source": {"lemma": "Alpha"}}
                            ]
                        }"#,
                    );
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(Arc::clone(&backend));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search_service,
            sample_metrics(),
        ));

        let Json(body) = super::restful_ids(
            Path("demo".into()),
            State(Arc::clone(&state)),
            RawQuery(Some("ids[]=a".into())),
        )
        .await
        .unwrap_or_else(|problem| {
            panic!("expected ids[] success, got status {}", problem.status())
        });

        ids_mock.assert_async().await;

        let data = body["data"]["ids"].as_array().cloned().expect("ids array");
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], JsonValue::String("a".into()));
    }

    #[tokio::test]
    async fn restful_ids_accepts_indexed_notation() {
        let server = MockServer::start_async().await;
        let ids_mock = server
            .mock_async(|when, then| {
                when.method("POST").path("/_mget");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        r#"{
                            "docs": [
                                {"_id": "a", "found": true, "_source": {"lemma": "Alpha"}}
                            ]
                        }"#,
                    );
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(Arc::clone(&backend));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search_service,
            sample_metrics(),
        ));

        let Json(body) = super::restful_ids(
            Path("demo".into()),
            State(Arc::clone(&state)),
            RawQuery(Some("ids[0]=a".into())),
        )
        .await
        .unwrap_or_else(|problem| {
            panic!("expected ids[0] success, got status {}", problem.status())
        });

        ids_mock.assert_async().await;

        let data = body["data"]["ids"].as_array().cloned().expect("ids array");
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], JsonValue::String("a".into()));
    }

    #[tokio::test]
    async fn restful_ids_rejects_empty_list() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        ));

        let err = super::restful_ids(
            Path("demo".into()),
            State(Arc::clone(&state)),
            RawQuery(None),
        )
        .await
        .expect_err("expected empty ids error");

        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn entry_definition_marks_array_field() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        );

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

    #[tokio::test]
    async fn entry_definition_marks_xml_field_with_format() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = AppState::new(
            config,
            vec![lexicon.clone()],
            vec!["match".into()],
            search,
            sample_metrics(),
        );

        let runtime = state.lexicon("demo").await.expect("lexicon");
        let entry = super::entry_definition(&runtime);
        let props = entry
            .get("properties")
            .and_then(JsonValue::as_object)
            .expect("properties");
        let xml = props
            .get("xml")
            .and_then(JsonValue::as_object)
            .expect("xml schema");

        assert_eq!(xml.get("type"), Some(&JsonValue::String("string".into())));
        assert_eq!(xml.get("format"), Some(&JsonValue::String("xml".into())));
    }

    #[tokio::test]
    async fn graphql_entries_query_returns_results() {
        let server = MockServer::start_async().await;

        let search_mock = server
            .mock_async(|when, then| {
                when.method("POST").path("/kosh[demo]/_search");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"hits":{"hits":[{"_id":"id-1","_source":{"lemma":"Foo"}}]}}"#);
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(backend);
        let state = AppState::new(
            config,
            vec![lexicon.clone()],
            vec!["match".into()],
            search_service.clone(),
            sample_metrics(),
        );

        let runtime = state.lexicon("demo").await.expect("lexicon");
        let schema = runtime
            .graphql_schema(search_service.clone(), vec!["match".into()])
            .expect("schema");

        let response = schema
            .execute(GraphRequest::new(
                "{ entries(field: lemma, query: \"foo\", queryType: match) { id lemma } }",
            ))
            .await
            .into_result()
            .expect("graphql response");

        assert_eq!(
            response.data,
            value!({
                "entries": [
                    {
                        "id": "id-1",
                        "lemma": "Foo"
                    }
                ]
            })
        );

        search_mock.assert_async().await;
    }

    #[tokio::test]
    async fn graphql_ids_query_returns_results() {
        let server = MockServer::start_async().await;

        let ids_mock = server
            .mock_async(|when, then| {
                when.method("POST").path("/_mget");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "docs": [
                            { "_id": "a", "found": true, "_source": { "lemma": "Alpha" } },
                            { "_id": "b", "found": true, "_source": { "lemma": "Beta" } }
                        ]
                    }));
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(backend);
        let state = AppState::new(
            config,
            vec![lexicon.clone()],
            vec!["match".into()],
            search_service.clone(),
            sample_metrics(),
        );

        let runtime = state.lexicon("demo").await.expect("lexicon");
        let schema = runtime
            .graphql_schema(search_service.clone(), vec!["match".into()])
            .expect("schema");

        let response = schema
            .execute(GraphRequest::new(
                "{ ids(ids: [\"a\", \"b\"]) { id lemma } }",
            ))
            .await
            .into_result()
            .expect("graphql response");

        assert_eq!(
            response.data,
            value!({
                "ids": [
                    { "id": "a", "lemma": "Alpha" },
                    { "id": "b", "lemma": "Beta" }
                ]
            })
        );

        ids_mock.assert_async().await;
    }

    #[tokio::test]
    async fn graphql_entries_clamps_size_parameter() {
        let server = MockServer::start_async().await;

        let search_mock = server
            .mock_async(|when, then| {
                when.method("POST")
                    .path("/kosh[demo]/_search")
                    .body_contains("\"size\":10000");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"hits":{"hits":[]}}"#);
            })
            .await;

        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let backend = Arc::new(mock_backend(&server.base_url()));
        let search_service = SearchService::new(Arc::clone(&backend));
        let state = AppState::new(
            config,
            vec![lexicon.clone()],
            vec!["match".into()],
            search_service.clone(),
            sample_metrics(),
        );

        let runtime = state.lexicon("demo").await.expect("lexicon");
        let schema = runtime
            .graphql_schema(search_service.clone(), vec!["match".into()])
            .expect("schema");

        schema
            .execute(GraphRequest::new(
                "{ entries(field: lemma, query: \"foo\", queryType: match, size: 20000) { id } }",
            ))
            .await;

        search_mock.assert_async().await;
    }

    #[tokio::test]
    async fn export_artifacts_creates_files() {
        let dir = tempdir().expect("tempdir");
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search,
            sample_metrics(),
        ));

        super::export_artifacts(Arc::clone(&state), dir.path())
            .await
            .expect("artifacts");

        assert!(dir.path().join("openapi/demo.json").exists());
        assert!(dir.path().join("graphql/demo_introspection.json").exists());
    }

    #[tokio::test]
    async fn metrics_handler_renders_output() {
        let config = sample_config();
        let lexicon = sample_lexicon("demo");
        let search = SearchService::new(Arc::new(mock_backend("http://localhost:9200")));
        let metrics = sample_metrics();
        metrics.observe_http("probe", StatusCode::OK);

        let state = Arc::new(AppState::new(
            config,
            vec![lexicon],
            vec!["match".into()],
            search,
            Arc::clone(&metrics),
        ));

        let snapshot = metrics.render().expect("snapshot");
        assert!(snapshot.contains("kosh_http_requests_total"));

        let response = super::metrics_handler(State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(content_type.starts_with("text/plain"));
    }
}
