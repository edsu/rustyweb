use std::path::Path;

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, IndexWriter, TantivyDocument};

const FIELD_URL: &str = "url";
const FIELD_TS: &str = "timestamp";
const FIELD_TITLE: &str = "title";
const FIELD_BODY: &str = "body";

pub struct SearchIndex {
    index: Index,
    writer: IndexWriter,
}

impl SearchIndex {
    pub fn open(index_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(index_dir)?;
        let schema = build_schema();
        let index = if index_dir.join("meta.json").exists() {
            Index::open_in_dir(index_dir)
                .with_context(|| format!("opening Tantivy index at {}", index_dir.display()))?
        } else {
            Index::create_in_dir(index_dir, schema.clone())
                .with_context(|| format!("creating Tantivy index at {}", index_dir.display()))?
        };
        let writer = index.writer(50_000_000)?;
        Ok(Self { index, writer })
    }

    pub fn add_document(&mut self, url: &str, timestamp: &str, title: &str, body: &str) -> Result<()> {
        let schema = self.index.schema();
        let url_f = schema.get_field(FIELD_URL).unwrap();
        let ts_f = schema.get_field(FIELD_TS).unwrap();
        let title_f = schema.get_field(FIELD_TITLE).unwrap();
        let body_f = schema.get_field(FIELD_BODY).unwrap();

        let mut doc = TantivyDocument::default();
        doc.add_text(url_f, url);
        doc.add_text(ts_f, timestamp);
        doc.add_text(title_f, title);
        doc.add_text(body_f, body);

        self.writer.add_document(doc)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        Ok(())
    }

    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let schema = self.index.schema();

        let title_f = schema.get_field(FIELD_TITLE).unwrap();
        let body_f = schema.get_field(FIELD_BODY).unwrap();
        let url_f = schema.get_field(FIELD_URL).unwrap();
        let ts_f = schema.get_field(FIELD_TS).unwrap();

        let query_parser = QueryParser::for_index(&self.index, vec![title_f, body_f]);
        let query = query_parser.parse_query(query_str)?;
        let collector = TopDocs::with_limit(limit).order_by_score();
        let top_docs = searcher.search(&query, &collector)?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (_score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let url = get_text(&doc, url_f);
            let timestamp = get_text(&doc, ts_f);
            let title = get_text(&doc, title_f);
            results.push(SearchResult { url, timestamp, title });
        }

        Ok(results)
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub url: String,
    pub timestamp: String,
    pub title: String,
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field(FIELD_URL, STRING | STORED);
    builder.add_text_field(FIELD_TS, STRING | STORED);
    builder.add_text_field(FIELD_TITLE, TEXT | STORED);
    builder.add_text_field(FIELD_BODY, TEXT);
    builder.build()
}

fn get_text(doc: &TantivyDocument, field: tantivy::schema::Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Extract `(title, body_text)` from raw HTML bytes, skipping script/style content.
pub fn extract_html_text(html: &[u8]) -> (String, String) {
    use scraper::{Html, Selector};

    let html_str = String::from_utf8_lossy(html);
    let doc = Html::parse_document(&html_str);

    let title_sel = Selector::parse("title").unwrap();
    let title = doc
        .select(&title_sel)
        .next()
        .map(|e| e.text().collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string();

    let body_sel = Selector::parse("body").unwrap();
    let skip_sel = Selector::parse("script, style, noscript").unwrap();
    let mut body_parts: Vec<String> = Vec::new();

    if let Some(body) = doc.select(&body_sel).next() {
        // Collect IDs of all nodes inside skip elements (script/style/noscript).
        let mut skip_ids = std::collections::HashSet::new();
        for skip_el in body.select(&skip_sel) {
            skip_ids.insert(skip_el.id());
            for desc in skip_el.descendants() {
                skip_ids.insert(desc.id());
            }
        }

        // Walk descendants; yield text nodes that are not in skip set.
        for node in body.descendants() {
            if skip_ids.contains(&node.id()) {
                continue;
            }
            if let scraper::node::Node::Text(t) = node.value() {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    body_parts.push(trimmed.to_string());
                }
            }
        }
    }

    (title, body_parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extract_text_from_html() {
        let html = b"<html><head><title>Hello World</title></head><body><p>Some text</p><script>var x=1;</script></body></html>";
        let (title, body) = extract_html_text(html);
        assert_eq!(title, "Hello World");
        assert!(body.contains("Some text"), "body: {body}");
        assert!(!body.contains("var x"), "should exclude script content: {body}");
    }

    #[test]
    fn roundtrip_index_and_search() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.add_document(
            "http://example.com/",
            "20240115120000",
            "Example Page",
            "This is some interesting content about Rust programming",
        ).unwrap();
        idx.commit().unwrap();

        let results = idx.search("Rust programming", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "http://example.com/");
    }

    #[test]
    fn search_no_results() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.commit().unwrap();

        let results = idx.search("nonexistent", 10).unwrap();
        assert!(results.is_empty());
    }
}
