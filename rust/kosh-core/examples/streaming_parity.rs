use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kosh_core::{discover_lexica, IndexDocument, KoshConfig, ParserMode, XmlEntryParser};
use serde_json::Map as JsonMap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = dataset_root()?;
    if !root.exists() {
        eprintln!("dataset path '{}' not found; skipping", root.display());
        return Ok(());
    }

    let default_config = KoshConfig::default();
    let namespaces: HashMap<_, _> = default_config.namespaces.0.into_iter().collect();

    let lexica = discover_lexica(&root, &default_config.data.spec, &default_config.data.pool)?;
    let mut compared = 0usize;
    let mut total_docs = 0usize;

    for lex in lexica {
        let lex = Arc::new(lex);
        let dom_parser =
            XmlEntryParser::new(Arc::clone(&lex), namespaces.clone(), ParserMode::Dom, 1)?;
        let streaming_parser = XmlEntryParser::new(
            Arc::clone(&lex),
            namespaces.clone(),
            ParserMode::Streaming,
            1,
        )?;

        if !streaming_parser.supports_streaming() {
            continue;
        }

        for file in &lex.files {
            if !file.exists() {
                continue;
            }
            let dom_docs = dom_parser.parse_file(file)?;
            let streaming_docs = streaming_parser.parse_file(file)?;

            assert_parity(file, &dom_docs, &streaming_docs)?;
            total_docs += dom_docs.len();
        }

        compared += 1;
    }

    println!(
        "Streaming parity verified for {} lexica ({} documents) under {}",
        compared,
        total_docs,
        root.display()
    );

    Ok(())
}

fn dataset_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let arg_path = env::args().nth(1).map(PathBuf::from);
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = match arg_path {
        Some(p) => {
            if p.is_absolute() {
                p
            } else {
                manifest_dir.join(p)
            }
        }
        None => manifest_dir.join("../../kosh_data"),
    };
    Ok(path)
}

fn assert_parity(
    file: &Path,
    dom_docs: &[IndexDocument],
    streaming_docs: &[IndexDocument],
) -> Result<(), Box<dyn std::error::Error>> {
    if dom_docs.len() != streaming_docs.len() {
        return Err(format!(
            "doc count mismatch for {} (dom={}, streaming={})",
            file.display(),
            dom_docs.len(),
            streaming_docs.len()
        )
        .into());
    }

    let mut dom = normalize_docs(dom_docs);
    let mut streaming = normalize_docs(streaming_docs);
    dom.sort_by(|a, b| a.0.cmp(&b.0));
    streaming.sort_by(|a, b| a.0.cmp(&b.0));

    if dom != streaming {
        return Err(format!("field mismatch for {}", file.display()).into());
    }

    Ok(())
}

fn normalize_docs(docs: &[IndexDocument]) -> Vec<(String, JsonMap<String, serde_json::Value>)> {
    docs.iter()
        .map(|doc| {
            let mut fields = doc.fields.clone();
            fields.remove("created");
            (doc.id.clone(), fields)
        })
        .collect()
}
