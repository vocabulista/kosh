use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kosh_core::lexicon::LexiconDescriptor;
use kosh_core::ParserMode;
use kosh_core::XmlEntryParser;
use serde_json::json;
use tempfile::TempDir;

fn main() -> anyhow::Result<()> {
    let entries = 5000usize;
    let tmp = TempDir::new()?;
    let xml_path = tmp.path().join("bench.xml");
    let schema_path = tmp.path().join("schema.json");

    write_schema(&schema_path)?;
    write_xml(&xml_path, entries)?;

    let lexicon = make_lexicon(&xml_path, &schema_path)?;

    let namespaces = HashMap::new();

    let sequential = parse_with_concurrency(&lexicon, &namespaces, ParserMode::Streaming, 1)?;
    let parallel = parse_with_concurrency(&lexicon, &namespaces, ParserMode::Streaming, 8)?;

    println!(
        "Parsed {entries} entries\n  concurrency=1 -> {:.3}s\n  concurrency=8 -> {:.3}s",
        sequential, parallel
    );

    Ok(())
}

fn write_schema(path: &Path) -> anyhow::Result<()> {
    let schema = json!({
        "mappings": {
            "_meta": {
                "_xpaths": {
                    "root": "/entries/entry",
                    "id": "id/text()",
                    "fields": {
                        "lemma": "lemma/text()",
                        "definition": "definition/text()"
                    }
                }
            }
        }
    });
    let mut file = File::create(path)?;
    serde_json::to_writer(&mut file, &schema)?;
    Ok(())
}

fn write_xml(path: &Path, entries: usize) -> anyhow::Result<()> {
    let mut file = File::create(path)?;
    writeln!(file, "<entries>")?;
    for idx in 0..entries {
        writeln!(
            file,
            "<entry><id>{idx}</id><lemma>lemma{idx}</lemma><definition>Definition {idx}</definition></entry>"
        )?;
    }
    writeln!(file, "</entries>")?;
    Ok(())
}

fn make_lexicon(xml_path: &Path, schema_path: &Path) -> anyhow::Result<LexiconDescriptor> {
    let schema_raw = fs::read_to_string(schema_path)?;
    let schema = serde_json::from_str(&schema_raw)?;

    Ok(LexiconDescriptor {
        uid: "bench".into(),
        pool: "bench[bench]".into(),
        files: vec![xml_path.to_path_buf()],
        schema,
        attributes: HashMap::new(),
        spec_path: PathBuf::from("bench"),
        schema_path: schema_path.to_path_buf(),
    })
}

fn parse_with_concurrency(
    lexicon: &LexiconDescriptor,
    namespaces: &HashMap<String, String>,
    mode: ParserMode,
    concurrency: usize,
) -> anyhow::Result<f64> {
    let parser = XmlEntryParser::new(
        std::sync::Arc::new(lexicon.clone()),
        namespaces.clone(),
        mode,
        concurrency,
    )?;

    let start = Instant::now();
    let docs = parser.parse_file(&lexicon.files[0])?;
    let elapsed = start.elapsed().as_secs_f64();

    if docs.len() != 5000 {
        anyhow::bail!("expected 5000 documents, got {}", docs.len());
    }

    Ok(elapsed)
}
