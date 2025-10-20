use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::num::ParseIntError;
use std::path::{Path, PathBuf};

use configparser::ini::Ini;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_CONFIG_NAME: &str = "kosh";

/// Canonical API configuration values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    pub root: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 5000,
            root: "/api".into(),
        }
    }
}

/// Filesystem and Elasticsearch related configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataConfig {
    pub host: String,
    pub r#loop: u32,
    pub pool: String,
    pub root: PathBuf,
    pub spec: String,
    pub sync: u32,
    pub wait: u32,
    pub retry_backoff_secs: u32,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            r#loop: 3,
            pool: DEFAULT_CONFIG_NAME.into(),
            root: PathBuf::from(format!("/var/lib/{DEFAULT_CONFIG_NAME}")),
            spec: format!(".{DEFAULT_CONFIG_NAME}"),
            sync: 10,
            wait: 60,
            retry_backoff_secs: 5,
        }
    }
}

/// Metadata exposed through the API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InfoConfig {
    pub desc: String,
    pub link: String,
    pub mail: String,
    pub repo: String,
}

impl Default for InfoConfig {
    fn default() -> Self {
        Self {
            desc: format!("{DEFAULT_CONFIG_NAME} - APIs for Lexical Data"),
            link: "https://kosh.uni-koeln.de".into(),
            mail: "info-kosh@uni-koeln.de".into(),
            repo: "https://github.com/cceh/kosh".into(),
        }
    }
}

/// Logging related settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoggerConfig {
    pub level: String,
    pub file: Option<PathBuf>,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            level: "INFO".into(),
            file: None,
        }
    }
}

/// XML namespaces used when parsing dictionary content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamespacesConfig(pub BTreeMap<String, String>);

impl Default for NamespacesConfig {
    fn default() -> Self {
        let mut namespaces = BTreeMap::new();
        namespaces.insert("dc".into(), "http://purl.org/dc/elements/1.1".into());
        namespaces.insert("tei".into(), "http://www.tei-c.org/ns/1.0".into());
        Self(namespaces)
    }
}

/// Flags enabling or disabling supported query types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryTypesConfig {
    pub term: bool,
    pub fuzzy: bool,
    pub r#match: bool,
    pub match_phrase: bool,
    pub prefix: bool,
    pub wildcard: bool,
    pub regexp: bool,
}

impl Default for QueryTypesConfig {
    fn default() -> Self {
        Self {
            term: true,
            fuzzy: true,
            r#match: true,
            match_phrase: true,
            prefix: false,
            wildcard: false,
            regexp: false,
        }
    }
}

/// In-memory representation of the service configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KoshConfig {
    pub name: String,
    pub api: ApiConfig,
    pub data: DataConfig,
    pub info: InfoConfig,
    pub logger: LoggerConfig,
    pub namespaces: NamespacesConfig,
    pub query_types: QueryTypesConfig,
    pub conf: Option<PathBuf>,
}

impl Default for KoshConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_CONFIG_NAME.into(),
            api: ApiConfig::default(),
            data: DataConfig::default(),
            info: InfoConfig::default(),
            logger: LoggerConfig::default(),
            namespaces: NamespacesConfig::default(),
            query_types: QueryTypesConfig::default(),
            conf: None,
        }
    }
}

impl KoshConfig {
    /// Create configuration from defaults and optional INI override file.
    pub fn from_optional_file<P>(path: Option<P>) -> Result<Self, ConfigError>
    where
        P: AsRef<Path>,
    {
        let mut config = Self::default();

        if let Some(path) = path {
            let path_ref = path.as_ref();
            let raw = fs::read_to_string(path_ref).map_err(|source| ConfigError::Io {
                path: path_ref.to_path_buf(),
                source,
            })?;

            let mut parser = Ini::new();
            let map = parser
                .read(raw)
                .map_err(|details| ConfigError::ParseConfig {
                    path: path_ref.to_path_buf(),
                    details,
                })?;

            config.apply_ini(&map)?;
            config.conf = Some(path_ref.to_path_buf());
        }

        Ok(config)
    }

    /// Merge overrides defined in an INI string.
    pub fn merge_inline(&mut self, raw: &str) -> Result<(), ConfigError> {
        let mut parser = Ini::new();
        let map = parser
            .read(raw.to_string())
            .map_err(|details| ConfigError::ParseInline { details })?;
        self.apply_ini(&map)
    }

    /// Resynchronise values that inherit from the configured service name.
    pub fn refresh_name_dependents(&mut self) {
        self.sync_name_dependents();
    }

    /// Merge overrides from an INI document onto the current configuration.
    fn apply_ini(
        &mut self,
        map: &HashMap<String, HashMap<String, Option<String>>>,
    ) -> Result<(), ConfigError> {
        if let Some(name) = ini_value(map, "default", "name") {
            self.name = name.to_owned();
        }

        if let Some(host) = ini_value(map, "api", "host") {
            self.api.host = host.to_owned();
        }
        if let Some(port) = ini_value(map, "api", "port") {
            self.api.port = parse_u16("api.port", port)?;
        }
        if let Some(root) = ini_value(map, "api", "root") {
            self.api.root = root.to_owned();
        }

        if let Some(host) = ini_value(map, "data", "host") {
            self.data.host = host.to_owned();
        }
        if let Some(loop_value) = ini_value(map, "data", "loop") {
            self.data.r#loop = parse_u32("data.loop", loop_value)?;
        }
        if let Some(pool) = ini_value(map, "data", "pool") {
            self.data.pool = pool.to_owned();
        }
        if let Some(root) = ini_value(map, "data", "root") {
            self.data.root = PathBuf::from(root);
        }
        if let Some(spec) = ini_value(map, "data", "spec") {
            self.data.spec = spec.to_owned();
        }
        if let Some(sync) = ini_value(map, "data", "sync") {
            self.data.sync = parse_u32("data.sync", sync)?;
        }
        if let Some(wait) = ini_value(map, "data", "wait") {
            self.data.wait = parse_u32("data.wait", wait)?;
        }
        if let Some(backoff) = ini_value(map, "data", "retry_backoff_secs")
            .or_else(|| ini_value(map, "data", "retry_backoff"))
        {
            self.data.retry_backoff_secs = parse_u32("data.retry_backoff_secs", backoff)?;
        }

        if let Some(desc) = ini_value(map, "info", "desc") {
            self.info.desc = desc.to_owned();
        }
        if let Some(link) = ini_value(map, "info", "link") {
            self.info.link = link.to_owned();
        }
        if let Some(mail) = ini_value(map, "info", "mail") {
            self.info.mail = mail.to_owned();
        }
        if let Some(repo) = ini_value(map, "info", "repo") {
            self.info.repo = repo.to_owned();
        }

        if let Some(level) = ini_value(map, "logger", "level") {
            self.logger.level = level.to_owned();
        }
        if let Some(file) = ini_value(map, "logger", "file") {
            let trimmed = file.trim();
            self.logger.file = if trimmed.is_empty() {
                None
            } else {
                Some(PathBuf::from(trimmed))
            };
        }

        if let Some(namespaces) = ini_section(map, "namespaces") {
            for (prefix, uri) in namespaces {
                if let Some(uri) = uri.as_deref() {
                    self.namespaces.0.insert(prefix.clone(), uri.to_owned());
                }
            }
        }

        if let Some(query_types) = ini_section(map, "query_types") {
            for (name, value) in query_types {
                if let Some(value) = value.as_deref() {
                    let flag = parse_bool(&format!("query_types.{name}"), value)?;
                    match name.as_str() {
                        "term" => self.query_types.term = flag,
                        "fuzzy" => self.query_types.fuzzy = flag,
                        "match" => self.query_types.r#match = flag,
                        "match_phrase" => self.query_types.match_phrase = flag,
                        "prefix" => self.query_types.prefix = flag,
                        "wildcard" => self.query_types.wildcard = flag,
                        "regexp" => self.query_types.regexp = flag,
                        _ => {}
                    }
                }
            }
        }

        self.sync_name_dependents();

        Ok(())
    }

    fn sync_name_dependents(&mut self) {
        let default_pool = DEFAULT_CONFIG_NAME.to_string();
        if self.data.pool == default_pool {
            self.data.pool = self.name.clone();
        }

        let default_root = PathBuf::from(format!("/var/lib/{DEFAULT_CONFIG_NAME}"));
        if self.data.root == default_root {
            self.data.root = PathBuf::from(format!("/var/lib/{}", self.name));
        }

        let default_spec = format!(".{DEFAULT_CONFIG_NAME}");
        if self.data.spec == default_spec {
            self.data.spec = format!(".{}", self.name);
        }
    }

    /// Compute the effective Elasticsearch index name for a lexicon pool.
    pub fn elastic_pool_for(&self, uid: &str) -> String {
        format!("{}[{uid}]", self.data.pool)
    }

    /// Convenience helper for retrieving a namespace URI if present.
    pub fn namespace(&self, prefix: &str) -> Option<&str> {
        self.namespaces.0.get(prefix).map(|value| value.as_str())
    }

    /// Return the list of enabled query types respecting configuration flags.
    pub fn enabled_query_types(&self) -> Vec<String> {
        [
            ("term", self.query_types.term),
            ("fuzzy", self.query_types.fuzzy),
            ("match", self.query_types.r#match),
            ("match_phrase", self.query_types.match_phrase),
            ("prefix", self.query_types.prefix),
            ("wildcard", self.query_types.wildcard),
            ("regexp", self.query_types.regexp),
        ]
        .into_iter()
        .filter_map(|(name, enabled)| enabled.then(|| name.to_string()))
        .collect()
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config at {path:?}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse INI at {path:?}: {details}")]
    ParseConfig { path: PathBuf, details: String },
    #[error("failed to parse inline INI: {details}")]
    ParseInline { details: String },
    #[error("invalid integer for {key}: {value}")]
    InvalidInt {
        key: String,
        value: String,
        #[source]
        source: ParseIntError,
    },
    #[error("invalid boolean for {key}: {value}")]
    InvalidBool { key: String, value: String },
}

fn parse_u16(key: &str, value: &str) -> Result<u16, ConfigError> {
    value
        .parse::<u16>()
        .map_err(|source| ConfigError::InvalidInt {
            key: key.into(),
            value: value.into(),
            source,
        })
}

fn parse_u32(key: &str, value: &str) -> Result<u32, ConfigError> {
    value
        .parse::<u32>()
        .map_err(|source| ConfigError::InvalidInt {
            key: key.into(),
            value: value.into(),
            source,
        })
}

fn parse_bool(key: &str, value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ConfigError::InvalidBool {
            key: key.into(),
            value: value.into(),
        }),
    }
}

// TODO: replicate interpolation semantics (e.g. `%(name)s`) from the Python configuration loader if real config files rely on them.

fn ini_section<'a>(
    map: &'a HashMap<String, HashMap<String, Option<String>>>,
    name: &str,
) -> Option<&'a HashMap<String, Option<String>>> {
    let key = name.to_lowercase();
    map.get(key.as_str())
}

fn ini_value<'a>(
    map: &'a HashMap<String, HashMap<String, Option<String>>>,
    section: &str,
    key: &str,
) -> Option<&'a str> {
    let section_map = ini_section(map, section)?;
    let key_name = key.to_lowercase();
    section_map
        .get(key_name.as_str())
        .and_then(|value| value.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn default_config_uses_builtin_values() {
        let config = KoshConfig::from_optional_file::<&str>(None).expect("load default");

        assert_eq!(config.name, "kosh");
        assert_eq!(config.api.port, 5000);
        assert_eq!(config.data.root, PathBuf::from("/var/lib/kosh"));
        assert_eq!(config.data.retry_backoff_secs, 5);
    }

    #[test]
    fn config_file_overrides_are_applied() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(
            file,
            "[DEFAULT]\nname = custom\n\n[api]\nport = 8088\n\n[data]\nroot = /srv/kosh\n"
        )
        .expect("write config");

        let config = KoshConfig::from_optional_file(Some(file.path())).expect("load config");

        assert_eq!(config.name, "custom");
        assert_eq!(config.api.port, 8088);
        assert_eq!(config.data.root, PathBuf::from("/srv/kosh"));
        assert_eq!(config.data.pool, "custom");
        assert_eq!(config.data.retry_backoff_secs, 5);
    }

    #[test]
    fn name_overrides_update_dependent_defaults() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(file, "[DEFAULT]\nname = alt\n").expect("write config");

        let config = KoshConfig::from_optional_file(Some(file.path())).expect("load config");

        assert_eq!(config.name, "alt");
        assert_eq!(config.data.pool, "alt");
        assert_eq!(config.data.root, PathBuf::from("/var/lib/alt"));
        assert_eq!(config.data.spec, ".alt");
    }

    #[test]
    fn retry_backoff_overrides_are_applied() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(file, "[data]\nretry_backoff = 4\n").expect("write config");

        let config = KoshConfig::from_optional_file(Some(file.path())).expect("load config");

        assert_eq!(config.data.retry_backoff_secs, 4);
    }

    #[test]
    fn enabled_query_types_collects_flags() {
        let config = KoshConfig::default();
        assert_eq!(
            config.enabled_query_types(),
            vec!["term", "fuzzy", "match", "match_phrase"]
        );

        let mut config = config;
        config.query_types.prefix = true;
        config.query_types.regexp = true;

        assert_eq!(
            config.enabled_query_types(),
            vec!["term", "fuzzy", "match", "match_phrase", "prefix", "regexp"]
        );
    }

    #[test]
    fn inline_merge_updates_config() {
        let mut config = KoshConfig::default();
        config
            .merge_inline("[logger]\nlevel = debug\n")
            .expect("inline merge");

        assert_eq!(config.logger.level, "debug");
    }
}
