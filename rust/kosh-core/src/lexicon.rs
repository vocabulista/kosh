use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use configparser::ini::Ini;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;
use walkdir::WalkDir;

/// Metadata describing a single lexicon as defined in a `.kosh` spec file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LexiconDescriptor {
    /// Unique identifier of the lexicon (section name in spec file).
    pub uid: String,
    /// Fully-qualified Elasticsearch pool name (e.g. `pool[uid]`).
    pub pool: String,
    /// Absolute paths to XML data files referenced by the spec.
    pub files: Vec<PathBuf>,
    /// Parsed JSON schema associated with the lexicon.
    pub schema: JsonValue,
    /// Additional metadata entries defined in the spec.
    pub attributes: HashMap<String, JsonValue>,
    /// Location of the spec file that produced this descriptor.
    pub spec_path: PathBuf,
    /// Resolved schema file path (useful for watchers).
    pub schema_path: PathBuf,
    // Cached size of indexed entries, useful for progress reporting (populated by future indexers).
    // TODO: Add `size` once indexing logic is ported.
    // pub size: Option<u64>,
}

#[derive(Debug, Error)]
pub enum LexiconError {
    #[error("failed to read spec file {path:?}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse spec file {path:?}: {details}")]
    ParseSpec { path: PathBuf, details: String },
    #[error("missing required key '{key}' in section '{section}' of {path:?}")]
    MissingField {
        path: PathBuf,
        section: String,
        key: String,
    },
    #[error("invalid schema JSON at {path:?}")]
    InvalidSchema {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Discover and parse all lexicon spec files under `root` matching `spec_filename`.
/// Returns a vector of lexicon descriptors akin to `kosh.elastic.index.lookup`.
pub fn discover_lexica(
    root: impl AsRef<Path>,
    spec_filename: &str,
    default_pool: &str,
) -> Result<Vec<LexiconDescriptor>, LexiconError> {
    let root = root.as_ref();
    let mut lexica = Vec::new();

    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }

        if entry.file_name() == spec_filename {
            let specs = parse_spec(entry.path(), default_pool)?;
            lexica.extend(specs);
        }
    }

    Ok(lexica)
}

/// Parse a single `.kosh` spec file and return the contained lexicon descriptors.
pub fn parse_spec(
    path: impl AsRef<Path>,
    default_pool: &str,
) -> Result<Vec<LexiconDescriptor>, LexiconError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path).map_err(|source| LexiconError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut ini = Ini::new();
    let map = ini
        .read(content)
        .map_err(|details| LexiconError::ParseSpec {
            path: path.to_path_buf(),
            details,
        })?;

    let mut descriptors = Vec::new();
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    for (section, values) in map {
        if section == "default" {
            continue;
        }

        let uid = section.to_owned();
        let files_raw = required_value(path, &uid, &values, "files")?;
        let schema_rel = required_value(path, &uid, &values, "schema")?;

        let schema_path = base_dir.join(schema_rel);
        let schema_content =
            fs::read_to_string(&schema_path).map_err(|source| LexiconError::Io {
                path: schema_path.clone(),
                source,
            })?;
        let schema_json = serde_json::from_str(&schema_content).map_err(|source| {
            LexiconError::InvalidSchema {
                path: schema_path.clone(),
                source,
            }
        })?;

        let files = parse_file_list(path, &uid, files_raw)?
            .into_iter()
            .map(|relative| base_dir.join(relative))
            .collect::<Vec<_>>();

        let pool = values
            .get("pool")
            .and_then(|value| value.as_ref())
            .map(|value| value.as_str())
            .unwrap_or(default_pool);
        let pool = format!("{pool}[{uid}]");

        let mut attributes = HashMap::new();
        for (key, value) in values.iter() {
            if matches!(key.as_str(), "files" | "pool" | "schema") {
                continue;
            }
            if let Some(value) = value.as_ref() {
                attributes.insert(key.to_owned(), parse_attribute(value));
            }
        }

        descriptors.push(LexiconDescriptor {
            uid,
            pool,
            files,
            schema: schema_json,
            attributes,
            spec_path: path.to_path_buf(),
            schema_path,
        });
    }

    Ok(descriptors)
}

fn required_value<'a>(
    path: &Path,
    section: &str,
    values: &'a HashMap<String, Option<String>>,
    key: &str,
) -> Result<&'a str, LexiconError> {
    values
        .get(&key.to_lowercase())
        .and_then(|value| value.as_deref())
        .ok_or_else(|| LexiconError::MissingField {
            path: path.to_path_buf(),
            section: section.to_owned(),
            key: key.to_owned(),
        })
}

fn parse_file_list(path: &Path, section: &str, raw: &str) -> Result<Vec<PathBuf>, LexiconError> {
    let trimmed = raw.trim();

    if trimmed.starts_with('[') {
        let list: Vec<String> =
            serde_json::from_str(trimmed).map_err(|_| LexiconError::ParseSpec {
                path: path.to_path_buf(),
                details: format!("failed to parse files array for section '{section}'"),
            })?;
        Ok(list.into_iter().map(PathBuf::from).collect())
    } else if trimmed.is_empty() {
        Err(LexiconError::MissingField {
            path: path.to_path_buf(),
            section: section.to_owned(),
            key: "files".into(),
        })
    } else {
        Ok(vec![PathBuf::from(trimmed)])
    }
}

fn parse_attribute(raw: &str) -> JsonValue {
    serde_json::from_str(raw).unwrap_or_else(|_| JsonValue::String(raw.to_owned()))
}
