use std::path::Path;

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, Value, STORED, STRING, TEXT};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

const FIELD_DOC_TYPE: &str = "doc_type";
const FIELD_COLLECTION_ID: &str = "collection_id";
const FIELD_COLLECTION_NAME: &str = "collection_name";
const FIELD_URL: &str = "url";
const FIELD_TS: &str = "timestamp";
const FIELD_TITLE: &str = "title";
const FIELD_BODY: &str = "body";

pub struct SearchIndex {
    index: Index,
    /// Present only when opened for writing. The server opens read-only so it
    /// does not hold Tantivy's exclusive write lock, letting `index` run while
    /// the server is serving.
    writer: Option<IndexWriter>,
}

impl SearchIndex {
    /// Open the index for writing (indexing). Creates it if needed and acquires
    /// Tantivy's exclusive write lock for the lifetime of this value.
    pub fn open(index_dir: &Path) -> Result<Self> {
        let index = Self::open_index(index_dir)?;
        let writer = index.writer(50_000_000)?;
        Ok(Self { index, writer: Some(writer) })
    }

    /// Open the index read-only (searching). Does not create a writer, so it
    /// does not take the write lock; indexing can proceed concurrently.
    pub fn open_read_only(index_dir: &Path) -> Result<Self> {
        let index = Self::open_index(index_dir)?;
        Ok(Self { index, writer: None })
    }

    fn open_index(index_dir: &Path) -> Result<Index> {
        std::fs::create_dir_all(index_dir)?;
        let schema = build_schema();
        if index_dir.join("meta.json").exists() {
            Index::open_in_dir(index_dir)
                .with_context(|| format!("opening Tantivy index at {}", index_dir.display()))
        } else {
            Index::create_in_dir(index_dir, schema)
                .with_context(|| format!("creating Tantivy index at {}", index_dir.display()))
        }
    }

    fn writer_mut(&mut self) -> &mut IndexWriter {
        self.writer
            .as_mut()
            .expect("SearchIndex opened read-only; no writer available")
    }

    /// Remove all documents (pages and the collection doc) belonging to a
    /// collection.  Call this before re-indexing a collection so that
    /// re-indexing is an upsert rather than an append.
    ///
    /// Tantivy applies a delete only to documents committed before it, so the
    /// caller should `delete_collection()` first, then `index_page()` /
    /// `index_collection()`, then `commit()` - the fresh documents survive.
    pub fn delete_collection(&mut self, collection_id: &str) {
        let field = self.index.schema().get_field(FIELD_COLLECTION_ID).unwrap();
        self.writer_mut().delete_term(Term::from_field_text(field, collection_id));
    }

    /// Index a single HTML page from an archive.
    pub fn index_page(
        &mut self,
        url: &str,
        timestamp: &str,
        title: &str,
        body: &str,
        collection_id: &str,
        collection_name: &str,
    ) -> Result<()> {
        let schema = self.index.schema();
        let mut doc = TantivyDocument::default();
        doc.add_text(schema.get_field(FIELD_DOC_TYPE).unwrap(), "page");
        doc.add_text(schema.get_field(FIELD_COLLECTION_ID).unwrap(), collection_id);
        doc.add_text(schema.get_field(FIELD_COLLECTION_NAME).unwrap(), collection_name);
        doc.add_text(schema.get_field(FIELD_URL).unwrap(), url);
        doc.add_text(schema.get_field(FIELD_TS).unwrap(), timestamp);
        doc.add_text(schema.get_field(FIELD_TITLE).unwrap(), title);
        doc.add_text(schema.get_field(FIELD_BODY).unwrap(), body);
        self.writer_mut().add_document(doc)?;
        Ok(())
    }

    /// Index a collection-level document so the collection itself is searchable.
    /// `body` should be the concatenation of the description and seed page titles/URLs.
    pub fn index_collection(
        &mut self,
        collection_id: &str,
        collection_name: &str,
        body: &str,
    ) -> Result<()> {
        let schema = self.index.schema();
        let mut doc = TantivyDocument::default();
        doc.add_text(schema.get_field(FIELD_DOC_TYPE).unwrap(), "collection");
        doc.add_text(schema.get_field(FIELD_COLLECTION_ID).unwrap(), collection_id);
        doc.add_text(schema.get_field(FIELD_COLLECTION_NAME).unwrap(), collection_name);
        doc.add_text(schema.get_field(FIELD_URL).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_TS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_TITLE).unwrap(), collection_name);
        doc.add_text(schema.get_field(FIELD_BODY).unwrap(), body);
        self.writer_mut().add_document(doc)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer_mut().commit()?;
        Ok(())
    }

    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let schema = self.index.schema();

        let title_f = schema.get_field(FIELD_TITLE).unwrap();
        let body_f = schema.get_field(FIELD_BODY).unwrap();
        let doc_type_f = schema.get_field(FIELD_DOC_TYPE).unwrap();
        let coll_id_f = schema.get_field(FIELD_COLLECTION_ID).unwrap();
        let coll_name_f = schema.get_field(FIELD_COLLECTION_NAME).unwrap();
        let url_f = schema.get_field(FIELD_URL).unwrap();
        let ts_f = schema.get_field(FIELD_TS).unwrap();

        let query_parser = QueryParser::for_index(&self.index, vec![title_f, body_f]);
        let query = query_parser.parse_query(query_str)?;
        let collector = TopDocs::with_limit(limit).order_by_score();
        let top_docs = searcher.search(&query, &collector)?;

        let mut snippet_gen = SnippetGenerator::create(&searcher, &query, body_f)?;
        // Tantivy's default snippet window is 150 chars; widen it for more
        // context around the matched terms in search results.
        snippet_gen.set_max_num_chars(350);

        let mut results = Vec::with_capacity(top_docs.len());
        for (_score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let snippet = snippet_gen.snippet_from_doc(&doc);
            results.push(SearchResult {
                doc_type: get_text(&doc, doc_type_f),
                collection_id: get_text(&doc, coll_id_f),
                collection_name: get_text(&doc, coll_name_f),
                url: get_text(&doc, url_f),
                timestamp: get_text(&doc, ts_f),
                title: get_text(&doc, title_f),
                snippet: snippet.to_html(),
            });
        }

        Ok(results)
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc_type: String,
    pub collection_id: String,
    pub collection_name: String,
    pub url: String,
    pub timestamp: String,
    pub title: String,
    pub snippet: String,
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field(FIELD_DOC_TYPE, STRING | STORED);
    builder.add_text_field(FIELD_COLLECTION_ID, STRING | STORED);
    builder.add_text_field(FIELD_COLLECTION_NAME, STRING | STORED);
    builder.add_text_field(FIELD_URL, STRING | STORED);
    builder.add_text_field(FIELD_TS, STRING | STORED);
    builder.add_text_field(FIELD_TITLE, TEXT | STORED);
    builder.add_text_field(FIELD_BODY, TEXT | STORED);
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
        let mut skip_ids = std::collections::HashSet::new();
        for skip_el in body.select(&skip_sel) {
            skip_ids.insert(skip_el.id());
            for desc in skip_el.descendants() {
                skip_ids.insert(desc.id());
            }
        }
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

        idx.index_page(
            "http://example.com/",
            "20240115120000",
            "Example Page",
            "This is some interesting content about Rust programming",
            "abc12345",
            "My Collection",
        ).unwrap();
        idx.commit().unwrap();

        let results = idx.search("Rust programming", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "http://example.com/");
        assert_eq!(results[0].collection_id, "abc12345");
        assert_eq!(results[0].doc_type, "page");
    }

    #[test]
    fn collection_document_is_searchable() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.index_collection(
            "abc12345",
            "My Archive",
            "A collection about digital preservation and web archiving",
        ).unwrap();
        idx.commit().unwrap();

        let results = idx.search("digital preservation", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_type, "collection");
        assert_eq!(results[0].collection_id, "abc12345");
    }

    #[test]
    fn search_returns_snippet() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.index_page(
            "http://example.com/",
            "20240115120000",
            "Example",
            "The quick brown fox jumps over the lazy dog near the riverbank",
            "abc12345",
            "Test",
        ).unwrap();
        idx.commit().unwrap();

        let results = idx.search("fox", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.contains("fox"), "snippet should contain matched term: {}", results[0].snippet);
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
