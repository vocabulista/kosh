pub mod config;
pub mod index;
pub mod lexicon;
pub mod logging;
pub mod search;

pub use config::{
    ApiConfig, ConfigError, DataConfig, InfoConfig, KoshConfig, LogFormat, LoggerConfig,
    NamespacesConfig, QueryTypesConfig,
};
pub use index::{
    elasticsearch::{ElasticsearchBackend, ElasticsearchBackendError, ElasticsearchConfig},
    parser::{LexiconXpaths, ParserError, ParserMode, XmlEntryParser},
    IndexBackend, IndexDocument, IndexError, IndexService, IndexStats,
};
pub use lexicon::{discover_lexica, parse_spec, LexiconDescriptor, LexiconError};
pub use logging::{init_tracing, LoggingError};
pub use search::{SearchError, SearchService};
