pub mod elasticsearch;
pub mod parser;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use thiserror::Error;

use crate::lexicon::LexiconDescriptor;

/// Captures a single document ready to be indexed into Elasticsearch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexDocument {
    pub id: String,
    pub fields: Map<String, JsonValue>,
}

impl IndexDocument {
    pub fn new(id: impl Into<String>, fields: Map<String, JsonValue>) -> Result<Self, IndexError> {
        let id = id.into();

        if id.trim().is_empty() {
            return Err(IndexError::InvalidDocument {
                reason: "document id cannot be empty".into(),
            });
        }

        Ok(Self { id, fields })
    }
}

/// Aggregated statistics returned after an indexing operation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct IndexStats {
    pub indexed: usize,
}

impl IndexStats {
    pub fn new(indexed: usize) -> Self {
        Self { indexed }
    }
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("backend error: {details}")]
    Backend { details: String },
    #[error("invalid document: {reason}")]
    InvalidDocument { reason: String },
    #[error("no documents supplied for lexicon {uid}")]
    NoDocuments { uid: String },
}

impl IndexError {
    pub fn backend(details: impl Into<String>) -> Self {
        IndexError::Backend {
            details: details.into(),
        }
    }
}

#[async_trait]
pub trait IndexBackend: Send + Sync {
    async fn create_index(
        &self,
        lexicon: &LexiconDescriptor,
        schema: &JsonValue,
    ) -> Result<(), IndexError>;

    async fn delete_index(&self, lexicon: &LexiconDescriptor) -> Result<(), IndexError>;

    // TODO: Support streaming document sources to avoid holding large batches in memory.
    async fn bulk_index(
        &self,
        lexicon: &LexiconDescriptor,
        documents: Vec<IndexDocument>,
    ) -> Result<IndexStats, IndexError>;
}

pub struct IndexService<B>
where
    B: IndexBackend + ?Sized,
{
    backend: Arc<B>,
}

impl<B> Clone for IndexService<B>
where
    B: IndexBackend + ?Sized,
{
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
        }
    }
}

impl<B> IndexService<B>
where
    B: IndexBackend + ?Sized,
{
    pub fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }

    pub async fn rebuild(
        &self,
        lexicon: &LexiconDescriptor,
        documents: Vec<IndexDocument>,
    ) -> Result<IndexStats, IndexError> {
        if documents.is_empty() {
            return Err(IndexError::NoDocuments {
                uid: lexicon.uid.clone(),
            });
        }

        self.backend.delete_index(lexicon).await?;
        self.backend.create_index(lexicon, &lexicon.schema).await?;
        self.backend.bulk_index(lexicon, documents).await
    }

    pub async fn append(
        &self,
        lexicon: &LexiconDescriptor,
        documents: Vec<IndexDocument>,
    ) -> Result<IndexStats, IndexError> {
        if documents.is_empty() {
            return Err(IndexError::NoDocuments {
                uid: lexicon.uid.clone(),
            });
        }

        self.backend.bulk_index(lexicon, documents).await
    }

    pub async fn delete(&self, lexicon: &LexiconDescriptor) -> Result<(), IndexError> {
        self.backend.delete_index(lexicon).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[test]
    fn document_requires_non_empty_id() {
        let result = IndexDocument::new("", Map::new());
        assert!(matches!(result, Err(IndexError::InvalidDocument { .. })));
    }

    struct RecordingBackend {
        events: Mutex<Vec<String>>,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn record(&self, event: String) {
            self.events.lock().unwrap().push(event);
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl IndexBackend for RecordingBackend {
        async fn create_index(
            &self,
            lexicon: &LexiconDescriptor,
            _schema: &JsonValue,
        ) -> Result<(), IndexError> {
            self.record(format!("create:{}", lexicon.uid));
            Ok(())
        }

        async fn delete_index(&self, lexicon: &LexiconDescriptor) -> Result<(), IndexError> {
            self.record(format!("delete:{}", lexicon.uid));
            Ok(())
        }

        async fn bulk_index(
            &self,
            lexicon: &LexiconDescriptor,
            documents: Vec<IndexDocument>,
        ) -> Result<IndexStats, IndexError> {
            self.record(format!("bulk:{}:{}", lexicon.uid, documents.len()));
            Ok(IndexStats::new(documents.len()))
        }
    }

    fn sample_lexicon() -> LexiconDescriptor {
        LexiconDescriptor {
            uid: "demo".into(),
            pool: "kosh[demo]".into(),
            files: vec![],
            schema: json!({"mappings": {}}),
            attributes: HashMap::new(),
            spec_path: PathBuf::from("demo/.kosh"),
            schema_path: PathBuf::from("demo/schema.json"),
        }
    }

    fn sample_document() -> IndexDocument {
        let mut fields = Map::new();
        fields.insert("field".into(), json!("value"));
        IndexDocument::new("doc-1", fields).expect("valid document")
    }

    #[tokio::test]
    async fn rebuild_executes_full_cycle() {
        let backend = Arc::new(RecordingBackend::new());
        let service = IndexService::new(Arc::clone(&backend));
        let lexicon = sample_lexicon();
        let docs = vec![sample_document()];

        let stats = service.rebuild(&lexicon, docs).await.expect("rebuild");

        assert_eq!(stats.indexed, 1);
        assert_eq!(
            backend.events(),
            vec![
                "delete:demo".to_string(),
                "create:demo".to_string(),
                "bulk:demo:1".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn append_skips_delete_create() {
        let backend = Arc::new(RecordingBackend::new());
        let service = IndexService::new(Arc::clone(&backend));
        let lexicon = sample_lexicon();
        let docs = vec![sample_document()];

        service.append(&lexicon, docs).await.expect("append");

        assert_eq!(backend.events(), vec!["bulk:demo:1".to_string()]);
    }

    #[tokio::test]
    async fn empty_batches_are_rejected() {
        let backend = Arc::new(RecordingBackend::new());
        let service = IndexService::new(Arc::clone(&backend));
        let lexicon = sample_lexicon();

        let result = service.append(&lexicon, Vec::new()).await;
        assert!(matches!(result, Err(IndexError::NoDocuments { uid }) if uid == "demo"));
    }
}
