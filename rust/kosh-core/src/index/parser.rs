use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{Map, Value as JsonValue};
use sha1::{Digest, Sha1};
use sxd_document::dom::{self, ChildOfElement};
use sxd_document::parser as sxd_parser;
use sxd_document::Package;
use sxd_xpath::nodeset::Node;
use sxd_xpath::{
    Context as XPathContext, Error as XPathError, Factory as XPathFactory, Value as XPathValue,
    XPath,
};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use super::{IndexDocument, IndexError};
use crate::lexicon::LexiconDescriptor;

#[derive(Debug, Clone)]
pub struct LexiconXpaths {
    pub root: String,
    pub id: String,
    pub fields: HashMap<String, String>,
}

impl LexiconXpaths {
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
    #[error(transparent)]
    Index(#[from] IndexError),
}

pub struct XmlEntryParser {
    lexicon: Arc<LexiconDescriptor>,
    namespaces: HashMap<String, String>,
    xpaths: LexiconXpaths,
}

impl XmlEntryParser {
    pub fn new(
        lexicon: Arc<LexiconDescriptor>,
        namespaces: HashMap<String, String>,
    ) -> Result<Self, ParserError> {
        let xpaths = LexiconXpaths::from_schema(&lexicon.schema)?;
        Ok(Self {
            lexicon,
            namespaces,
            xpaths,
        })
    }

    pub fn lexicon(&self) -> Arc<LexiconDescriptor> {
        Arc::clone(&self.lexicon)
    }

    pub fn namespaces(&self) -> &HashMap<String, String> {
        &self.namespaces
    }

    pub fn xpaths(&self) -> &LexiconXpaths {
        &self.xpaths
    }

    pub fn parse_file(&self, path: &Path) -> Result<Vec<IndexDocument>, ParserError> {
        let xml = fs::read_to_string(path).map_err(|source| ParserError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let package = sxd_parser::parse(&xml).map_err(|source| ParserError::MalformedXml {
            path: path.to_path_buf(),
            source,
        })?;
        let document = package.as_document();

        let mut context = XPathContext::new();
        for (prefix, uri) in &self.namespaces {
            context.set_namespace(prefix, uri);
        }

        let factory = XPathFactory::new();
        let root_expr = compile_xpath(&factory, &self.xpaths.root)?;
        let id_expr = compile_xpath(&factory, &self.xpaths.id)?;
        let field_exprs = self
            .xpaths
            .fields
            .iter()
            .map(|(field, raw)| Ok((field.clone(), compile_xpath(&factory, raw)?)))
            .collect::<Result<HashMap<_, _>, ParserError>>()?;
        let mut documents = Vec::new();

        let root_value = root_expr
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

        for root in nodes {
            let mut payload = Map::new();

            for (field, expr) in &field_exprs {
                let values = evaluate_strings(expr, field, &context, root)?;

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
                JsonValue::String(Utc::now().to_rfc3339()),
            );

            let mut identifiers = evaluate_strings(&id_expr, &self.xpaths.id, &context, root)?;
            let identifier = identifiers
                .drain(..)
                .find_map(normalize_and_trim)
                .unwrap_or_else(|| fallback_id(&xml_blob));

            let document = IndexDocument::new(identifier, payload)?;
            documents.push(document);
        }

        Ok(documents)
    }
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
                new_element.append_child(dest_document.create_text(text.text()));
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

    fn sample_lexicon() -> Arc<LexiconDescriptor> {
        Arc::new(LexiconDescriptor {
            uid: "demo".into(),
            pool: "kosh[demo]".into(),
            files: vec![],
            schema: json!({
                "mappings": {
                    "_meta": {
                        "_xpaths": {
                            "root": "/items/item",
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

    #[test]
    fn parses_basic_entries() {
        let mut temp = NamedTempFile::new().expect("temp xml");
        writeln!(
            temp,
            "<items>\n  <item>\n    <id>a</id>\n    <lemma>Foo</lemma>\n    <sense>one</sense>\n    <sense>two</sense>\n  </item>\n  <item>\n    <lemma>Bar</lemma>\n  </item>\n</items>"
        )
        .expect("write xml");

        let parser = XmlEntryParser::new(sample_lexicon(), HashMap::new()).expect("parser");
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

        assert_ne!(docs[1].id, "a");
        assert_eq!(
            docs[1].fields.get("lemma"),
            Some(&JsonValue::String("Bar".into()))
        );
        let xml_1 = docs[1]
            .fields
            .get("xml")
            .and_then(|v| v.as_str())
            .expect("xml field");
        assert!(xml_1.contains("<lemma>Bar</lemma>"));
        assert!(docs[1].fields.contains_key("created"));
    }
}
