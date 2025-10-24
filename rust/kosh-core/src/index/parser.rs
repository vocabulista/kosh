//! Helpers for parsing XML lexicon entries into Elasticsearch documents.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use quick_xml::{events::Event, Reader, Writer};
use serde_json::{Map, Value as JsonValue};
use sha1::{Digest, Sha1};
use sxd_document::dom::{self, ChildOfElement, ChildOfRoot};
use sxd_document::parser as sxd_parser;
use sxd_document::Package;
use sxd_xpath::nodeset::Node;
use sxd_xpath::{
    Context as XPathContext, Error as XPathError, Factory as XPathFactory, Value as XPathValue,
    XPath,
};
use thiserror::Error;
use tracing::{debug, info, warn};
use unicode_normalization::UnicodeNormalization;

use super::{IndexDocument, IndexError};
use crate::lexicon::LexiconDescriptor;

/// XPath metadata embedded in the Elasticsearch schema of a lexicon.
#[derive(Debug, Clone)]
pub struct LexiconXpaths {
    pub root: String,
    pub id: String,
    pub fields: HashMap<String, String>,
}

impl LexiconXpaths {
    /// Extract the required XPath expressions from the `_meta._xpaths` section of a schema.
    pub fn from_schema(schema: &JsonValue) -> Result<Self, ParserError> {
        let mappings = schema
            .get("mappings")
            .ok_or_else(|| ParserError::MissingNode("mappings".into()))?;
        let meta = mappings
            .get("_meta")
            .ok_or_else(|| ParserError::MissingNode("mappings._meta".into()))?;
        let xpaths = meta
            .get("_xpaths")
            .ok_or_else(|| ParserError::MissingNode("mappings._meta._xpaths".into()))?;

        let root = xpaths
            .get("root")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ParserError::MissingNode("mappings._meta._xpaths.root".into()))?;
        let id = xpaths
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ParserError::MissingNode("mappings._meta._xpaths.id".into()))?;

        let field_map = xpaths
            .get("fields")
            .ok_or_else(|| ParserError::MissingNode("mappings._meta._xpaths.fields".into()))?;

        let mut fields = HashMap::new();
        if let Some(map) = field_map.as_object() {
            for (key, value) in map {
                if let Some(xpath) = value.as_str() {
                    fields.insert(key.clone(), xpath.to_owned());
                }
            }
        } else {
            return Err(ParserError::UnexpectedType(
                "mappings._meta._xpaths.fields".into(),
            ));
        }

        Ok(Self {
            root: root.to_owned(),
            id: id.to_owned(),
            fields,
        })
    }
}

/// Controls which parsing strategy should be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserMode {
    Dom,
    Streaming,
}

#[derive(Debug, Clone)]
struct StreamSegment {
    prefix: Option<String>,
    local: String,
}

/// Errors emitted while parsing lexicon entries into index documents.
#[derive(Debug, Error)]
pub enum ParserError {
    #[error("schema missing node: {0}")]
    MissingNode(String),
    #[error("unexpected type at {0}")]
    UnexpectedType(String),
    #[error("unsupported XPath expression: {0}")]
    UnsupportedXPath(String),
    #[error("failed to read XML file {path:?}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed XML in {path:?}")]
    MalformedXml {
        path: std::path::PathBuf,
        #[source]
        source: sxd_document::parser::Error,
    },
    #[error("failed to evaluate XPath '{expression}'")]
    Evaluate {
        expression: String,
        #[source]
        source: sxd_xpath::Error,
    },
    #[error("unable to serialise XML node: {0}")]
    Serialize(String),
    #[error("unsupported node type for XML serialisation")]
    UnsupportedNode,
    #[error("streaming parse error in {path:?}: {reason}")]
    Stream {
        path: std::path::PathBuf,
        reason: String,
    },
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// Parses XML entries referenced by a [`LexiconDescriptor`] into [`IndexDocument`]s.
pub struct XmlEntryParser {
    lexicon: Arc<LexiconDescriptor>,
    namespaces: HashMap<String, String>,
    xpaths: LexiconXpaths,
    root_expr: XPath,
    id_expr: XPath,
    field_exprs: HashMap<String, XPath>,
    streaming_segments: Option<Vec<StreamSegment>>,
}

impl XmlEntryParser {
    /// Build a parser for the supplied lexicon, loading XPath definitions from its schema.
    pub fn new(
        lexicon: Arc<LexiconDescriptor>,
        namespaces: HashMap<String, String>,
        mode: ParserMode,
    ) -> Result<Self, ParserError> {
        let xpaths = LexiconXpaths::from_schema(&lexicon.schema)?;
        let factory = XPathFactory::new();
        let root_expr = compile_xpath(&factory, &xpaths.root)?;
        let id_expr = compile_xpath(&factory, &xpaths.id)?;
        let mut field_exprs = HashMap::with_capacity(xpaths.fields.len());
        for (field, raw) in &xpaths.fields {
            let expr = compile_xpath(&factory, raw)?;
            field_exprs.insert(field.clone(), expr);
        }

        let streaming_segments = if matches!(mode, ParserMode::Streaming) {
            match parse_stream_segments(&xpaths.root) {
                Some(segments) => Some(segments),
                None => {
                    warn!(
                        lexicon = %lexicon.uid,
                        xpath = %xpaths.root,
                        "streaming parser disabled; falling back to DOM parsing"
                    );
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            lexicon,
            namespaces,
            xpaths,
            root_expr,
            id_expr,
            field_exprs,
            streaming_segments,
        })
    }

    /// Return the descriptor associated with this parser.
    pub fn lexicon(&self) -> Arc<LexiconDescriptor> {
        Arc::clone(&self.lexicon)
    }

    /// Borrow the namespace mappings applied when evaluating XPath expressions.
    pub fn namespaces(&self) -> &HashMap<String, String> {
        &self.namespaces
    }

    /// Expose the resolved XPath configuration used for parsing.
    pub fn xpaths(&self) -> &LexiconXpaths {
        &self.xpaths
    }

    /// Whether the parser will use the streaming implementation.
    pub fn supports_streaming(&self) -> bool {
        self.streaming_segments.is_some()
    }

    /// Parse an XML file, producing a document for every entry matched by the root XPath.
    ///
    /// Each generated document includes the raw XML snapshot and a `created`
    /// timestamp to support downstream pipelines.
    pub fn parse_file(&self, path: &Path) -> Result<Vec<IndexDocument>, ParserError> {
        if let Some(segments) = &self.streaming_segments {
            self.parse_file_streaming(path, segments)
        } else {
            self.parse_file_dom(path)
        }
    }

    fn parse_file_dom(&self, path: &Path) -> Result<Vec<IndexDocument>, ParserError> {
        let xml = fs::read_to_string(path).map_err(|source| ParserError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let package = sxd_parser::parse(&xml).map_err(|source| ParserError::MalformedXml {
            path: path.to_path_buf(),
            source,
        })?;
        let document = package.as_document();

        let context = self.build_context();

        let root_value = self
            .root_expr
            .evaluate(&context, document.root())
            .map_err(|source| ParserError::Evaluate {
                expression: self.xpaths.root.clone(),
                source: XPathError::Executing(source),
            })?;

        let nodes = match root_value {
            XPathValue::Nodeset(nodeset) => {
                nodeset.document_order().into_iter().collect::<Vec<_>>()
            }
            _ => return Err(ParserError::UnsupportedXPath(self.xpaths.root.clone())),
        };

        let created = Utc::now().to_rfc3339();
        let mut documents = Vec::new();

        for root in nodes {
            let document = self.build_document(&context, root, &created)?;
            documents.push(document);

            let processed = documents.len();
            if processed % 500 == 0 {
                info!(
                    lexicon = %self.lexicon.uid,
                    file = %path.display(),
                    processed,
                    "parsed lexicon records"
                );
            }
        }

        if !documents.is_empty() && documents.len() % 500 != 0 {
            info!(
                lexicon = %self.lexicon.uid,
                file = %path.display(),
                processed = documents.len(),
                "parsed lexicon records"
            );
        }

        Ok(documents)
    }

    fn parse_file_streaming(
        &self,
        path: &Path,
        segments: &[StreamSegment],
    ) -> Result<Vec<IndexDocument>, ParserError> {
        let xml = fs::read_to_string(path).map_err(|source| ParserError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let mut reader = Reader::from_str(&xml);
        reader.trim_text(true);
        let mut buf = Vec::new();

        let mut documents = Vec::new();
        let mut match_stack: Vec<bool> = Vec::new();
        let mut collecting = false;
        let mut entry_depth = 0usize;
        let mut writer: Option<Writer<Vec<u8>>> = None;
        let mut processed = 0usize;
        let created = Utc::now().to_rfc3339();

        loop {
            buf.clear();
            let event = reader
                .read_event_into(&mut buf)
                .map_err(|err| ParserError::Stream {
                    path: path.to_path_buf(),
                    reason: err.to_string(),
                })?;

            match event {
                Event::Start(e) => {
                    if segments.len() == 1 {
                        if collecting {
                            if let Some(writer) = writer.as_mut() {
                                writer
                                    .write_event(Event::Start(e.to_owned()))
                                    .map_err(|err| ParserError::Stream {
                                        path: path.to_path_buf(),
                                        reason: err.to_string(),
                                    })?;
                            }
                            entry_depth += 1;
                            continue;
                        } else if matches_segment(&segments[0], e.name().as_ref()) {
                            collecting = true;
                            entry_depth = 0;
                            let mut new_writer = Writer::new(Vec::new());
                            new_writer
                                .write_event(Event::Start(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                            writer = Some(new_writer);
                            continue;
                        }
                    }

                    let depth = match_stack.len();
                    let ancestors_match = match_stack.iter().all(|&m| m);
                    let seg_match = ancestors_match
                        && depth < segments.len()
                        && matches_segment(&segments[depth], e.name().as_ref());
                    match_stack.push(seg_match);

                    if collecting {
                        if let Some(writer) = writer.as_mut() {
                            writer
                                .write_event(Event::Start(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                        }
                        entry_depth += 1;
                    } else if seg_match && depth + 1 == segments.len() {
                        collecting = true;
                        entry_depth = 0;
                        let mut new_writer = Writer::new(Vec::new());
                        new_writer
                            .write_event(Event::Start(e.to_owned()))
                            .map_err(|err| ParserError::Stream {
                                path: path.to_path_buf(),
                                reason: err.to_string(),
                            })?;
                        writer = Some(new_writer);
                    }
                }
                Event::Empty(e) => {
                    if segments.len() == 1 {
                        if collecting {
                            if let Some(writer) = writer.as_mut() {
                                writer
                                    .write_event(Event::Empty(e.to_owned()))
                                    .map_err(|err| ParserError::Stream {
                                        path: path.to_path_buf(),
                                        reason: err.to_string(),
                                    })?;
                            }
                        } else if matches_segment(&segments[0], e.name().as_ref()) {
                            let mut new_writer = Writer::new(Vec::new());
                            new_writer
                                .write_event(Event::Empty(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                            finalize_entry(
                                self,
                                new_writer.into_inner(),
                                &mut documents,
                                &created,
                                path,
                                &mut processed,
                            )?;
                        }
                        continue;
                    }

                    let depth = match_stack.len();
                    let ancestors_match = match_stack.iter().all(|&m| m);
                    let seg_match = ancestors_match
                        && depth < segments.len()
                        && matches_segment(&segments[depth], e.name().as_ref());
                    match_stack.push(seg_match);

                    if collecting {
                        if let Some(writer) = writer.as_mut() {
                            writer
                                .write_event(Event::Empty(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                        }
                    } else if seg_match && depth + 1 == segments.len() {
                        let mut new_writer = Writer::new(Vec::new());
                        new_writer
                            .write_event(Event::Empty(e.to_owned()))
                            .map_err(|err| ParserError::Stream {
                                path: path.to_path_buf(),
                                reason: err.to_string(),
                            })?;
                        finalize_entry(
                            self,
                            new_writer.into_inner(),
                            &mut documents,
                            &created,
                            path,
                            &mut processed,
                        )?;
                    }

                    match_stack.pop();
                }
                Event::Text(e) => {
                    if segments.len() == 1 {
                        if collecting {
                            if let Some(writer) = writer.as_mut() {
                                writer
                                    .write_event(Event::Text(e.to_owned()))
                                    .map_err(|err| ParserError::Stream {
                                        path: path.to_path_buf(),
                                        reason: err.to_string(),
                                    })?;
                            }
                        }
                        continue;
                    }
                    if collecting {
                        if let Some(writer) = writer.as_mut() {
                            writer
                                .write_event(Event::Text(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                        }
                    }
                }
                Event::CData(e) => {
                    if segments.len() == 1 {
                        if collecting {
                            if let Some(writer) = writer.as_mut() {
                                writer
                                    .write_event(Event::CData(e.to_owned()))
                                    .map_err(|err| ParserError::Stream {
                                        path: path.to_path_buf(),
                                        reason: err.to_string(),
                                    })?;
                            }
                        }
                        continue;
                    }
                    if collecting {
                        if let Some(writer) = writer.as_mut() {
                            writer
                                .write_event(Event::CData(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                        }
                    }
                }
                Event::Comment(e) => {
                    if segments.len() == 1 {
                        if collecting {
                            if let Some(writer) = writer.as_mut() {
                                writer.write_event(Event::Comment(e.to_owned())).map_err(
                                    |err| ParserError::Stream {
                                        path: path.to_path_buf(),
                                        reason: err.to_string(),
                                    },
                                )?;
                            }
                        }
                        continue;
                    }
                    if collecting {
                        if let Some(writer) = writer.as_mut() {
                            writer
                                .write_event(Event::Comment(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                        }
                    }
                }
                Event::End(e) => {
                    if segments.len() == 1 {
                        if collecting {
                            if let Some(writer) = writer.as_mut() {
                                writer
                                    .write_event(Event::End(e.to_owned()))
                                    .map_err(|err| ParserError::Stream {
                                        path: path.to_path_buf(),
                                        reason: err.to_string(),
                                    })?;
                            }
                            if matches_segment(&segments[0], e.name().as_ref()) && entry_depth == 0
                            {
                                let entry_bytes = writer
                                    .take()
                                    .expect("streaming writer must exist")
                                    .into_inner();
                                collecting = false;
                                finalize_entry(
                                    self,
                                    entry_bytes,
                                    &mut documents,
                                    &created,
                                    path,
                                    &mut processed,
                                )?;
                            } else if entry_depth > 0 {
                                entry_depth -= 1;
                            }
                        }
                        continue;
                    }

                    let matched = match_stack.pop().unwrap_or(false);

                    if collecting {
                        if let Some(writer) = writer.as_mut() {
                            writer
                                .write_event(Event::End(e.to_owned()))
                                .map_err(|err| ParserError::Stream {
                                    path: path.to_path_buf(),
                                    reason: err.to_string(),
                                })?;
                        }

                        if matched
                            && match_stack.iter().all(|&m| m)
                            && match_stack.len() == segments.len() - 1
                        {
                            let entry_bytes = writer
                                .take()
                                .expect("streaming writer must exist")
                                .into_inner();
                            collecting = false;
                            finalize_entry(
                                self,
                                entry_bytes,
                                &mut documents,
                                &created,
                                path,
                                &mut processed,
                            )?;
                        } else if entry_depth > 0 {
                            entry_depth -= 1;
                        }
                    }
                }
                Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
                Event::Eof => break,
            }
        }

        if collecting {
            return Err(ParserError::Stream {
                path: path.to_path_buf(),
                reason: "unexpected EOF while parsing lexicon entry".into(),
            });
        }

        if processed > 0 && processed % 500 != 0 {
            info!(
                lexicon = %self.lexicon.uid,
                file = %path.display(),
                processed,
                "parsed lexicon records"
            );
        }

        Ok(documents)
    }

    fn build_context(&self) -> XPathContext<'_> {
        let mut context = XPathContext::new();
        for (prefix, uri) in &self.namespaces {
            context.set_namespace(prefix, uri);
        }
        context
    }

    fn build_document(
        &self,
        context: &XPathContext,
        root: Node,
        created: &str,
    ) -> Result<IndexDocument, ParserError> {
        let mut payload = Map::new();

        for (field, expr) in &self.field_exprs {
            let raw = self
                .xpaths
                .fields
                .get(field)
                .expect("compiled expression must have raw entry");
            let values = evaluate_strings(expr, raw, context, root)?;

            if field.starts_with('[') && field.ends_with(']') {
                let key = &field[1..field.len() - 1];
                if !values.is_empty() {
                    let json_values = values
                        .into_iter()
                        .map(JsonValue::String)
                        .collect::<Vec<_>>();
                    payload.insert(key.to_string(), JsonValue::Array(json_values));
                }
            } else if let Some(value) = values.into_iter().next() {
                payload.insert(field.clone(), JsonValue::String(value));
            }
        }

        let xml_blob = serialize_node(root)?;
        payload.insert("xml".to_string(), JsonValue::String(xml_blob.clone()));
        payload.insert(
            "created".to_string(),
            JsonValue::String(created.to_string()),
        );

        let mut identifiers = evaluate_strings(&self.id_expr, &self.xpaths.id, context, root)?;
        let identifier = identifiers
            .drain(..)
            .find_map(normalize_and_trim)
            .unwrap_or_else(|| fallback_id(&xml_blob));

        Ok(IndexDocument::new(identifier, payload)?)
    }
}

fn finalize_entry(
    parser: &XmlEntryParser,
    entry_bytes: Vec<u8>,
    documents: &mut Vec<IndexDocument>,
    created: &str,
    path: &Path,
    processed: &mut usize,
) -> Result<(), ParserError> {
    let entry_xml =
        String::from_utf8(entry_bytes).map_err(|err| ParserError::Serialize(err.to_string()))?;
    let package = sxd_parser::parse(&entry_xml).map_err(|source| ParserError::MalformedXml {
        path: path.to_path_buf(),
        source,
    })?;
    let document = package.as_document();
    let mut entry_node = None;
    for child in document.root().children() {
        if let ChildOfRoot::Element(element) = child {
            entry_node = Some(Node::Element(element));
            break;
        }
    }
    let entry_node = entry_node.ok_or(ParserError::UnsupportedNode)?;
    let context = parser.build_context();
    let document = parser.build_document(&context, entry_node, created)?;
    documents.push(document);
    *processed += 1;
    if *processed % 500 == 0 {
        info!(
            lexicon = %parser.lexicon.uid,
            file = %path.display(),
            processed = *processed,
            "parsed lexicon records"
        );
    }
    Ok(())
}

fn matches_segment(segment: &StreamSegment, name: &[u8]) -> bool {
    if let Ok(raw) = std::str::from_utf8(name) {
        if let Some((prefix, local)) = raw.split_once(':') {
            segment
                .prefix
                .as_deref()
                .map(|p| p == prefix)
                .unwrap_or(false)
                && segment.local == local
        } else {
            segment.prefix.is_none() && segment.local == raw
        }
    } else {
        false
    }
}

fn parse_stream_segments(root: &str) -> Option<Vec<StreamSegment>> {
    let trimmed = root.trim();
    let simplified = if let Some(pos) = trimmed.rfind("//") {
        let suffix = trimmed[pos + 2..].trim_start_matches('/');
        if suffix.is_empty() {
            return None;
        }
        debug!(
            original = trimmed,
            simplified = suffix,
            "streaming parser using descendant suffix of root XPath"
        );
        format!("/{}", suffix)
    } else {
        trimmed.to_owned()
    };
    if !simplified.starts_with('/') {
        return None;
    }
    if simplified.contains('[')
        || simplified.contains('@')
        || simplified.contains('(')
        || simplified.contains(')')
        || simplified.contains('|')
        || simplified.contains('*')
    {
        return None;
    }

    let parts: Vec<&str> = simplified
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }

    let mut segments = Vec::with_capacity(parts.len());
    for part in parts {
        if let Some((prefix, local)) = part.split_once(':') {
            if prefix.is_empty() || local.is_empty() {
                return None;
            }
            segments.push(StreamSegment {
                prefix: Some(prefix.to_string()),
                local: local.to_string(),
            });
        } else {
            segments.push(StreamSegment {
                prefix: None,
                local: part.to_string(),
            });
        }
    }

    Some(segments)
}

fn compile_xpath(factory: &XPathFactory, raw: &str) -> Result<XPath, ParserError> {
    factory
        .build(raw)
        .map_err(|_| ParserError::UnsupportedXPath(raw.into()))?
        .ok_or_else(|| ParserError::UnsupportedXPath(raw.into()))
}

fn evaluate_strings(
    expr: &XPath,
    raw: &str,
    context: &XPathContext,
    node: Node,
) -> Result<Vec<String>, ParserError> {
    let value = expr
        .evaluate(context, node)
        .map_err(|source| ParserError::Evaluate {
            expression: raw.into(),
            source: XPathError::Executing(source),
        })?;

    let strings = match value {
        XPathValue::Boolean(boolean) => normalize_and_trim(boolean.to_string())
            .into_iter()
            .collect(),
        XPathValue::Number(number) => normalize_and_trim(number.to_string()).into_iter().collect(),
        XPathValue::String(string) => normalize_and_trim(string).into_iter().collect(),
        XPathValue::Nodeset(nodeset) => nodeset
            .document_order()
            .into_iter()
            .filter_map(|node| normalize_and_trim(node.string_value()))
            .collect(),
    };

    Ok(strings)
}

fn normalize_and_trim<S: AsRef<str>>(value: S) -> Option<String> {
    let normalized: String = value.as_ref().nfc().collect();
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn fallback_id(xml: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(xml.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn serialize_node(node: Node) -> Result<String, ParserError> {
    match node {
        Node::Element(element) => serialize_element(element),
        Node::Root(root) => {
            let mut xml = Vec::new();
            sxd_document::writer::format_document(&root.document(), &mut xml)
                .map_err(|err| ParserError::Serialize(err.to_string()))?;
            let xml =
                String::from_utf8(xml).map_err(|err| ParserError::Serialize(err.to_string()))?;
            Ok(trim_xml_declaration(xml))
        }
        Node::Text(text) => Ok(text.text().to_string()),
        Node::Comment(comment) => Ok(format!("<!--{}-->", comment.text())),
        _ => Err(ParserError::UnsupportedNode),
    }
}

fn serialize_element(element: dom::Element) -> Result<String, ParserError> {
    let package = Package::new();
    let document = package.as_document();
    let cloned = clone_element(document, element)?;
    document.root().append_child(cloned);

    let mut xml = Vec::new();
    sxd_document::writer::format_document(&document, &mut xml)
        .map_err(|err| ParserError::Serialize(err.to_string()))?;
    let xml = String::from_utf8(xml).map_err(|err| ParserError::Serialize(err.to_string()))?;
    Ok(trim_xml_declaration(xml))
}

fn clone_element<'d>(
    dest_document: dom::Document<'d>,
    source: dom::Element,
) -> Result<dom::Element<'d>, ParserError> {
    let name = source.name();
    let new_element = match name.namespace_uri() {
        Some(ns) => dest_document.create_element((ns, name.local_part())),
        None => dest_document.create_element(name.local_part()),
    };

    if let Some(ns) = source.default_namespace_uri() {
        new_element.set_default_namespace_uri(Some(ns));
    }

    for namespace in source.namespaces_in_scope() {
        if !namespace.prefix().is_empty() {
            new_element.register_prefix(namespace.prefix(), namespace.uri());
        }
    }

    for attribute in source.attributes() {
        let attr_name = attribute.name();
        let value = attribute.value();
        match attr_name.namespace_uri() {
            Some(ns) => {
                new_element.set_attribute_value((ns, attr_name.local_part()), value);
            }
            None => {
                new_element.set_attribute_value(attr_name.local_part(), value);
            }
        }
    }

    for child in source.children() {
        match child {
            ChildOfElement::Element(element) => {
                let cloned = clone_element(dest_document, element)?;
                new_element.append_child(cloned);
            }
            ChildOfElement::Text(text) => {
                if !text.text().trim().is_empty() {
                    new_element.append_child(dest_document.create_text(text.text()));
                }
            }
            ChildOfElement::Comment(comment) => {
                new_element.append_child(dest_document.create_comment(comment.text()));
            }
            ChildOfElement::ProcessingInstruction(pi) => {
                new_element.append_child(
                    dest_document.create_processing_instruction(pi.target(), pi.value()),
                );
            }
        }
    }

    Ok(new_element)
}

fn trim_xml_declaration(mut xml: String) -> String {
    if let Some(pos) = xml.find("?>") {
        xml = xml[pos + 2..].to_string();
    }
    xml.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    fn lexicon_with_root(root: &str) -> Arc<LexiconDescriptor> {
        Arc::new(LexiconDescriptor {
            uid: "demo".into(),
            pool: "kosh[demo]".into(),
            files: vec![],
            schema: json!({
                "mappings": {
                    "_meta": {
                        "_xpaths": {
                            "root": root,
                            "id": "id/text()",
                            "fields": {
                                "lemma": "lemma/text()",
                                "[senses]": "sense/text()"
                            }
                        }
                    },
                    "properties": {}
                }
            }),
            attributes: HashMap::new(),
            spec_path: PathBuf::from("demo/.kosh"),
            schema_path: PathBuf::from("demo/schema.json"),
        })
    }

    fn sample_lexicon() -> Arc<LexiconDescriptor> {
        lexicon_with_root("/items/item")
    }

    #[test]
    fn parses_basic_entries() {
        let mut temp = NamedTempFile::new().expect("temp xml");
        writeln!(
            temp,
            "<items>\n  <item>\n    <id>a</id>\n    <lemma>Foo</lemma>\n    <sense>one</sense>\n    <sense>two</sense>\n  </item>\n  <item>\n    <lemma>Bar</lemma>\n  </item>\n</items>"
        )
        .expect("write xml");

        let parser =
            XmlEntryParser::new(sample_lexicon(), HashMap::new(), ParserMode::Dom).expect("parser");
        let docs = parser.parse_file(temp.path()).expect("parse file");

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].id, "a");
        assert_eq!(
            docs[0].fields.get("lemma"),
            Some(&JsonValue::String("Foo".into()))
        );
        assert_eq!(
            docs[0].fields.get("senses"),
            Some(&JsonValue::Array(vec![
                JsonValue::String("one".into()),
                JsonValue::String("two".into())
            ]))
        );
        let xml_0 = docs[0]
            .fields
            .get("xml")
            .and_then(|v| v.as_str())
            .expect("xml field");
        assert!(xml_0.contains("<lemma>Foo</lemma>"));
        assert!(docs[0].fields.contains_key("created"));
        let created_0 = docs[0]
            .fields
            .get("created")
            .and_then(|v| v.as_str())
            .expect("created timestamp");

        let xml_1 = docs[1]
            .fields
            .get("xml")
            .and_then(|v| v.as_str())
            .expect("xml field");
        let mut hasher = Sha1::new();
        hasher.update(xml_1.as_bytes());
        let expected_hash = format!("{:x}", hasher.finalize());

        assert_eq!(docs[1].id, expected_hash);
        assert_eq!(
            docs[1].fields.get("lemma"),
            Some(&JsonValue::String("Bar".into()))
        );
        assert!(xml_1.contains("<lemma>Bar</lemma>"));
        let created_1 = docs[1]
            .fields
            .get("created")
            .and_then(|v| v.as_str())
            .expect("created timestamp");
        assert_eq!(created_0, created_1);
    }

    #[test]
    fn streaming_parser_matches_dom_output() {
        let mut temp = NamedTempFile::new().expect("temp xml");
        writeln!(
            temp,
            "<items>\n  <item>\n    <id>a</id>\n    <lemma>Foo</lemma>\n  </item>\n  <item>\n    <id>b</id>\n    <lemma>Bar</lemma>\n  </item>\n</items>"
        )
        .expect("write xml");

        let dom_parser = XmlEntryParser::new(sample_lexicon(), HashMap::new(), ParserMode::Dom)
            .expect("dom parser");
        let streaming_parser =
            XmlEntryParser::new(sample_lexicon(), HashMap::new(), ParserMode::Streaming)
                .expect("streaming parser");

        let dom_docs = dom_parser.parse_file(temp.path()).expect("dom parse");
        let streaming_docs = streaming_parser
            .parse_file(temp.path())
            .expect("stream parse");

        assert_eq!(dom_docs.len(), streaming_docs.len());

        for (dom, stream) in dom_docs.iter().zip(streaming_docs.iter()) {
            assert_eq!(dom.id, stream.id);

            let mut dom_fields = dom.fields.clone();
            let mut stream_fields = stream.fields.clone();
            dom_fields.remove("created");
            stream_fields.remove("created");
            assert_eq!(dom_fields, stream_fields);
        }
    }

    #[test]
    fn streaming_parser_supports_descendant_root() {
        let mut temp = NamedTempFile::new().expect("temp xml");
        writeln!(
            temp,
            "<root>\n  <wrapper>\n    <item><id>a</id><lemma>Foo</lemma></item>\n  </wrapper>\n  <item><id>b</id><lemma>Bar</lemma></item>\n</root>"
        )
        .expect("write xml");

        let lexicon = lexicon_with_root("//item");
        let dom_parser = XmlEntryParser::new(Arc::clone(&lexicon), HashMap::new(), ParserMode::Dom)
            .expect("dom parser");
        let streaming_parser = XmlEntryParser::new(lexicon, HashMap::new(), ParserMode::Streaming)
            .expect("streaming parser");

        let dom_docs = dom_parser.parse_file(temp.path()).expect("dom parse");
        let streaming_docs = streaming_parser
            .parse_file(temp.path())
            .expect("stream parse");

        assert_eq!(dom_docs.len(), streaming_docs.len());
        assert_eq!(
            dom_docs
                .iter()
                .map(|doc| doc.id.clone())
                .collect::<Vec<_>>(),
            streaming_docs
                .iter()
                .map(|doc| doc.id.clone())
                .collect::<Vec<_>>()
        );
    }
}
