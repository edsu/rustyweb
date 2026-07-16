use std::path::Path;

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, Value, INDEXED, STORED, STRING, TEXT};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

const FIELD_DOC_TYPE: &str = "doc_type";
const FIELD_COLLECTION_ID: &str = "collection_id";
const FIELD_COLLECTION_NAME: &str = "collection_name";
const FIELD_URL: &str = "url";
const FIELD_TS: &str = "timestamp";
const FIELD_TITLE: &str = "title";
const FIELD_BODY: &str = "body";
/// Exact host of a page URL (e.g. `example.com`), for `domain:` filtering.
const FIELD_DOMAIN: &str = "domain";
/// Tokenized words from a page URL (host + path), so URL words are searchable.
const FIELD_URL_TOKENS: &str = "url_tokens";
/// Page description from `<meta name=description>` / `og:description`.
const FIELD_DESCRIPTION: &str = "description";
/// Concatenated `<h1>`/`<h2>` heading text.
const FIELD_HEADINGS: &str = "headings";
/// Four-digit crawl year (from the page timestamp), for `year:` filtering.
const FIELD_YEAR: &str = "year";
/// Coarse media type of the page: `html` or `pdf`, for `type:` filtering.
const FIELD_MEDIA_TYPE: &str = "type";
/// Primary language subtag from `<html lang>` (e.g. `en`), for `lang:` filtering.
const FIELD_LANG: &str = "lang";
/// Curated collection id (slug) this document belongs to, for `collection:` filtering.
const FIELD_COLLECTION: &str = "collection";

/// How much more a title match counts than a body/url match when ranking.
const TITLE_BOOST: tantivy::Score = 3.0;
/// Headings rank above body text but below the title.
const HEADINGS_BOOST: tantivy::Score = 2.0;

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
            let index = Index::open_in_dir(index_dir)
                .with_context(|| format!("opening Tantivy index at {}", index_dir.display()))?;
            // A stored schema that differs from the current one (e.g. after new
            // fields were added) can't be written or queried correctly. Fail
            // with a clear message instead of panicking on a missing field.
            if index.schema() != schema {
                anyhow::bail!(
                    "the search index at {} was built with an older schema; \
                     run `rustyweb reindex` to rebuild it",
                    index_dir.display()
                );
            }
            Ok(index)
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

    /// Index a single page from an archive. Fields not set on the [`Page`]
    /// default to empty, so callers only populate what they have.
    pub fn index_page(&mut self, page: &Page) -> Result<()> {
        let schema = self.index.schema();
        let mut doc = TantivyDocument::default();
        doc.add_text(schema.get_field(FIELD_DOC_TYPE).unwrap(), "page");
        doc.add_text(schema.get_field(FIELD_COLLECTION_ID).unwrap(), page.collection_id);
        doc.add_text(schema.get_field(FIELD_COLLECTION_NAME).unwrap(), page.collection_name);
        doc.add_text(schema.get_field(FIELD_COLLECTION).unwrap(), page.collection);
        doc.add_text(schema.get_field(FIELD_URL).unwrap(), page.url);
        doc.add_text(schema.get_field(FIELD_TS).unwrap(), page.timestamp);
        doc.add_text(schema.get_field(FIELD_TITLE).unwrap(), page.title);
        doc.add_text(schema.get_field(FIELD_BODY).unwrap(), page.body);
        doc.add_text(schema.get_field(FIELD_DESCRIPTION).unwrap(), page.description);
        doc.add_text(schema.get_field(FIELD_HEADINGS).unwrap(), page.headings);
        // Derived URL fields: an exact host for `domain:` filtering, and the
        // URL's words tokenized so they're searchable as ordinary terms.
        doc.add_text(schema.get_field(FIELD_DOMAIN).unwrap(), domain_of(page.url));
        doc.add_text(schema.get_field(FIELD_URL_TOKENS).unwrap(), url_search_text(page.url));
        // Numeric year for range filtering; omitted when there's no usable date.
        if let Some(year) = year_of(page.timestamp) {
            doc.add_u64(schema.get_field(FIELD_YEAR).unwrap(), year);
        }
        doc.add_text(schema.get_field(FIELD_MEDIA_TYPE).unwrap(), page.media_type);
        doc.add_text(schema.get_field(FIELD_LANG).unwrap(), primary_lang(page.lang));
        self.writer_mut().add_document(doc)?;
        Ok(())
    }

    /// Index a collection-level document so the collection itself is searchable.
    /// `body` should be the concatenation of the description and seed page titles/URLs.
    pub fn index_collection(
        &mut self,
        collection_id: &str,
        collection_name: &str,
        collection: &str,
        body: &str,
    ) -> Result<()> {
        let schema = self.index.schema();
        let mut doc = TantivyDocument::default();
        doc.add_text(schema.get_field(FIELD_DOC_TYPE).unwrap(), "collection");
        doc.add_text(schema.get_field(FIELD_COLLECTION_ID).unwrap(), collection_id);
        doc.add_text(schema.get_field(FIELD_COLLECTION_NAME).unwrap(), collection_name);
        doc.add_text(schema.get_field(FIELD_COLLECTION).unwrap(), collection);
        doc.add_text(schema.get_field(FIELD_URL).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_TS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_TITLE).unwrap(), collection_name);
        doc.add_text(schema.get_field(FIELD_BODY).unwrap(), body);
        // Collection docs have no page URL or HTML metadata; keep those empty.
        doc.add_text(schema.get_field(FIELD_DESCRIPTION).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_HEADINGS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_DOMAIN).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_URL_TOKENS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_MEDIA_TYPE).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_LANG).unwrap(), "");
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
        let domain_f = schema.get_field(FIELD_DOMAIN).unwrap();
        let url_tokens_f = schema.get_field(FIELD_URL_TOKENS).unwrap();
        let description_f = schema.get_field(FIELD_DESCRIPTION).unwrap();
        let headings_f = schema.get_field(FIELD_HEADINGS).unwrap();
        let collection_f = schema.get_field(FIELD_COLLECTION).unwrap();

        // Bare words search the title, headings, body, description, and URL
        // words. Other fields (domain:, url:, title:) are reachable by explicit
        // `field:` syntax.
        let mut query_parser = QueryParser::for_index(
            &self.index,
            vec![title_f, headings_f, body_f, description_f, url_tokens_f],
        );
        // Require all terms by default (`climate change` means both), which
        // matches what people expect from a search box more than OR does.
        query_parser.set_conjunction_by_default();
        // A title match is the strongest relevance signal; headings next.
        query_parser.set_field_boost(title_f, TITLE_BOOST);
        query_parser.set_field_boost(headings_f, HEADINGS_BOOST);
        // Parse leniently: a malformed query (stray quote, bad `field:`) yields
        // a best-effort query instead of a hard error, so the search box never
        // 500s while someone is experimenting with syntax.
        let (query, _errors) = query_parser.parse_query_lenient(query_str);
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
                collection: get_text(&doc, collection_f),
                url: get_text(&doc, url_f),
                domain: get_text(&doc, domain_f),
                timestamp: get_text(&doc, ts_f),
                title: get_text(&doc, title_f),
                description: get_text(&doc, description_f),
                snippet: snippet.to_html(),
            });
        }

        Ok(results)
    }
}

/// The indexable fields of one page. Borrowed string slices so callers can pass
/// references without cloning; unset fields default to `""` via [`Default`], so
/// adding a field here does not force every call site to change.
#[derive(Debug, Default)]
pub struct Page<'a> {
    pub url: &'a str,
    pub timestamp: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub description: &'a str,
    pub headings: &'a str,
    /// Coarse media type: `"html"` or `"pdf"` (empty if unknown).
    pub media_type: &'a str,
    /// Page language tag, e.g. `"en-US"` (stored as its primary subtag).
    pub lang: &'a str,
    /// The WACZ this page came from (id and display name).
    pub collection_id: &'a str,
    pub collection_name: &'a str,
    /// The curated collection id (slug) this page's WACZ belongs to.
    pub collection: &'a str,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc_type: String,
    /// The WACZ this result came from (id and display name).
    pub collection_id: String,
    pub collection_name: String,
    /// The curated collection id (slug) the WACZ belongs to, for `collection:`
    /// filtering and linking to the collection page.
    pub collection: String,
    pub url: String,
    /// Exact host of the page URL (empty for collection results).
    pub domain: String,
    pub timestamp: String,
    pub title: String,
    /// Page description (`<meta description>` / og:description), if any.
    pub description: String,
    pub snippet: String,
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field(FIELD_DOC_TYPE, STRING | STORED);
    builder.add_text_field(FIELD_COLLECTION_ID, STRING | STORED);
    builder.add_text_field(FIELD_COLLECTION_NAME, STRING | STORED);
    // Curated collection id (slug), for `collection:` filtering.
    builder.add_text_field(FIELD_COLLECTION, STRING | STORED);
    builder.add_text_field(FIELD_URL, STRING | STORED);
    builder.add_text_field(FIELD_TS, STRING | STORED);
    builder.add_text_field(FIELD_TITLE, TEXT | STORED);
    builder.add_text_field(FIELD_BODY, TEXT | STORED);
    // Description is stored so it can be shown when a page has no body snippet.
    builder.add_text_field(FIELD_DESCRIPTION, TEXT | STORED);
    // Headings are indexed (and boosted at query time) but not stored.
    builder.add_text_field(FIELD_HEADINGS, TEXT);
    // Exact host, stored so results can show it: matched only by `domain:host`.
    builder.add_text_field(FIELD_DOMAIN, STRING | STORED);
    // Tokenized URL words; indexed for search but not stored (we keep the URL).
    builder.add_text_field(FIELD_URL_TOKENS, TEXT);
    // Numeric crawl year, indexed for `year:2021` and `year:[2020 TO 2023]`.
    builder.add_u64_field(FIELD_YEAR, INDEXED | STORED);
    // Coarse media type (`html`/`pdf`) and page language, for exact filtering.
    builder.add_text_field(FIELD_MEDIA_TYPE, STRING | STORED);
    builder.add_text_field(FIELD_LANG, STRING | STORED);
    builder.build()
}

/// The primary language subtag of an HTML `lang` attribute, lowercased
/// (`en-US` -> `en`). Empty when there's no usable value.
fn primary_lang(lang: &str) -> String {
    lang.split(['-', '_'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The four-digit crawl year parsed from a 14-digit page timestamp
/// (`20210417...` -> `2021`). `None` when the timestamp is missing or does not
/// start with a plausible year.
fn year_of(timestamp: &str) -> Option<u64> {
    let year: u64 = timestamp.get(..4)?.parse().ok()?;
    (1000..=9999).contains(&year).then_some(year)
}

/// The exact host of a URL, lowercased (e.g. `https://Example.com/a` -> `example.com`).
/// Empty when the URL has no host (relative paths, `urn:`, unparseable input).
fn domain_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default()
}

/// A searchable text rendering of a URL: the host and path with separators
/// turned into spaces, so the default tokenizer indexes each word. For example
/// `https://github.com/DocNow/hydrator` yields `github.com DocNow hydrator`,
/// making a search for `hydrator` match the page.
fn url_search_text(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return String::new(),
    };
    let mut parts: Vec<&str> = Vec::new();
    if let Some(host) = parsed.host_str() {
        parts.push(host);
    }
    // Split the path on `/` and keep non-empty segments (drops the leading `/`).
    parts.extend(parsed.path().split('/').filter(|s| !s.is_empty()));
    parts.join(" ")
}

fn get_text(doc: &TantivyDocument, field: tantivy::schema::Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Text extracted from an HTML page for indexing.
#[derive(Debug, Default, PartialEq)]
pub struct HtmlText {
    pub title: String,
    pub body: String,
    /// `<meta name=description>` or `og:description`, if present.
    pub description: String,
    /// Concatenated `<h1>`/`<h2>` heading text.
    pub headings: String,
    /// The `<html lang>` attribute value, if present (e.g. `en`, `en-US`).
    pub lang: String,
}

/// Extract title, body text, description, and headings from raw HTML bytes,
/// skipping script/style content.
pub fn extract_html_text(html: &[u8]) -> HtmlText {
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

    // Description: prefer <meta name="description">, fall back to og:description.
    let description = meta_content(&doc, "meta[name=description]")
        .or_else(|| meta_content(&doc, "meta[property=\"og:description\"]"))
        .unwrap_or_default();

    // Headings: h1 and h2 text, in document order.
    let heading_sel = Selector::parse("h1, h2").unwrap();
    let headings = doc
        .select(&heading_sel)
        .map(|e| e.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    // Language from the <html lang="..."> attribute, if any.
    let html_sel = Selector::parse("html").unwrap();
    let lang = doc
        .select(&html_sel)
        .next()
        .and_then(|e| e.value().attr("lang"))
        .unwrap_or("")
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

    HtmlText {
        title,
        body: body_parts.join(" "),
        description,
        headings,
        lang,
    }
}

/// The trimmed `content` attribute of the first element matching `selector`,
/// dropped if empty. `selector` must be a valid CSS selector.
fn meta_content(doc: &scraper::Html, selector: &str) -> Option<String> {
    let sel = scraper::Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a page with the common fields; unset fields default to empty.
    fn page<'a>(url: &'a str, title: &'a str, body: &'a str, cid: &'a str, cname: &'a str) -> Page<'a> {
        Page { url, title, body, collection_id: cid, collection_name: cname, ..Default::default() }
    }

    /// A page with a given URL and timestamp; fixed title/body for date tests.
    fn page_ts<'a>(url: &'a str, ts: &'a str) -> Page<'a> {
        Page { url, timestamp: ts, title: "T", body: "shared content", collection_id: "c1", collection_name: "C1", ..Default::default() }
    }

    #[test]
    fn extract_text_from_html() {
        let html = b"<html><head><title>Hello World</title></head><body><p>Some text</p><script>var x=1;</script></body></html>";
        let t = extract_html_text(html);
        assert_eq!(t.title, "Hello World");
        assert!(t.body.contains("Some text"), "body: {}", t.body);
        assert!(!t.body.contains("var x"), "should exclude script content: {}", t.body);
    }

    #[test]
    fn extract_description_and_headings_from_html() {
        let html = br#"<html><head><title>T</title>
            <meta name="description" content="A concise summary">
            <meta property="og:description" content="OG fallback"></head>
            <body><h1>Main Heading</h1><h2>Sub Heading</h2><p>Body.</p></body></html>"#;
        let t = extract_html_text(html);
        assert_eq!(t.description, "A concise summary");
        assert!(t.headings.contains("Main Heading"), "headings: {}", t.headings);
        assert!(t.headings.contains("Sub Heading"), "headings: {}", t.headings);
    }

    #[test]
    fn description_falls_back_to_og_description() {
        let html = br#"<html><head><meta property="og:description" content="OG only"></head>
            <body>x</body></html>"#;
        let t = extract_html_text(html);
        assert_eq!(t.description, "OG only");
    }

    #[test]
    fn extract_lang_from_html_element() {
        let html = br#"<html lang="en-US"><head><title>T</title></head><body>x</body></html>"#;
        let t = extract_html_text(html);
        assert_eq!(t.lang, "en-US");
    }

    #[test]
    fn primary_lang_takes_the_first_subtag() {
        assert_eq!(primary_lang("en-US"), "en");
        assert_eq!(primary_lang("EN"), "en");
        assert_eq!(primary_lang("pt_BR"), "pt");
        assert_eq!(primary_lang(""), "");
    }

    #[test]
    fn type_and_lang_filters() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/page", title: "Doc", body: "shared", media_type: "html",
            lang: "en-US", collection_id: "c1", collection_name: "C1", ..Default::default()
        }).unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/file.pdf", title: "Report", body: "shared", media_type: "pdf",
            lang: "", collection_id: "c1", collection_name: "C1", ..Default::default()
        }).unwrap();
        idx.commit().unwrap();

        let r = idx.search("type:pdf", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/file.pdf");

        let r = idx.search("shared type:html", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/page");

        // lang is stored as its primary subtag, so `lang:en` matches `en-US`.
        let r = idx.search("lang:en", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/page");
    }

    #[test]
    fn roundtrip_index_and_search() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.index_page(&page(
            "http://example.com/",
            "Example Page",
            "This is some interesting content about Rust programming",
            "abc12345",
            "My Collection",
        )).unwrap();
        idx.commit().unwrap();

        let results = idx.search("Rust programming", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "http://example.com/");
        assert_eq!(results[0].collection_id, "abc12345");
        assert_eq!(results[0].doc_type, "page");
    }

    #[test]
    fn description_and_headings_are_searchable() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/a",
            title: "Plain Title",
            body: "ordinary body",
            description: "a treatise on marmots",
            headings: "Notable Rodents",
            collection_id: "c1",
            collection_name: "C1",
            ..Default::default()
        }).unwrap();
        idx.commit().unwrap();

        assert_eq!(idx.search("marmots", 10).unwrap().len(), 1, "description searchable");
        assert_eq!(idx.search("rodents", 10).unwrap().len(), 1, "headings searchable");
        assert_eq!(idx.search("a", 10).unwrap()[0].description, "a treatise on marmots");
    }

    #[test]
    fn collection_document_is_searchable() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.index_collection(
            "abc12345",
            "My Archive",
            "my-archive",
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

        idx.index_page(&page(
            "http://example.com/",
            "Example",
            "The quick brown fox jumps over the lazy dog near the riverbank",
            "abc12345",
            "Test",
        )).unwrap();
        idx.commit().unwrap();

        let results = idx.search("fox", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.contains("fox"), "snippet should contain matched term: {}", results[0].snippet);
    }

    #[test]
    fn open_errors_on_schema_mismatch() {
        use tantivy::schema::{Schema, TEXT};
        let tmp = TempDir::new().unwrap();
        // Create an index with a different (older-style) schema.
        let mut b = Schema::builder();
        b.add_text_field("body", TEXT);
        tantivy::Index::create_in_dir(tmp.path(), b.build()).unwrap();

        // Opening with the current schema must fail cleanly (not panic), and
        // the message should point the user at reindex.
        let result = SearchIndex::open(tmp.path());
        assert!(result.is_err(), "opening a mismatched schema should error");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("reindex"), "error should suggest reindex: {msg}");
    }

    #[test]
    fn search_no_results() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.commit().unwrap();

        let results = idx.search("nonexistent", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn domain_of_extracts_lowercased_host() {
        assert_eq!(domain_of("https://Example.COM/a/b?x=1"), "example.com");
        assert_eq!(domain_of("http://sub.example.org/"), "sub.example.org");
        // No host / unparseable input yields an empty domain.
        assert_eq!(domain_of("urn:text:foo"), "");
        assert_eq!(domain_of("not a url"), "");
    }

    #[test]
    fn url_search_text_yields_host_and_path_words() {
        let text = url_search_text("https://github.com/DocNow/hydrator");
        assert_eq!(text, "github.com DocNow hydrator");
    }

    #[test]
    fn search_matches_words_from_the_url() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page(
            "https://github.com/DocNow/hydrator",
            "Some Title",
            "unrelated body text",
            "abc12345",
            "Test",
        )).unwrap();
        idx.commit().unwrap();

        // "hydrator" appears only in the URL, but url words are searchable.
        let results = idx.search("hydrator", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://github.com/DocNow/hydrator");
        assert_eq!(results[0].domain, "github.com");
    }

    #[test]
    fn domain_filter_restricts_to_exact_host() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page("https://example.com/one", "One", "shared word", "c1", "C1")).unwrap();
        idx.index_page(&page("https://other.org/two", "Two", "shared word", "c1", "C1")).unwrap();
        idx.commit().unwrap();

        let results = idx.search("domain:example.com", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/one");

        // Combined with a term, still AND-scoped to that domain.
        let results = idx.search("domain:example.com shared", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/one");
    }

    #[test]
    fn collection_filter_restricts_results() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://a.com/1", title: "A", body: "shared",
            collection_id: "w1", collection_name: "W1", collection: "demo", ..Default::default()
        }).unwrap();
        idx.index_page(&Page {
            url: "https://b.com/1", title: "B", body: "shared",
            collection_id: "w2", collection_name: "W2", collection: "other", ..Default::default()
        }).unwrap();
        idx.commit().unwrap();

        let r = idx.search("collection:demo", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].collection, "demo");

        // Combined with a term, still scoped to the collection.
        let r = idx.search("collection:demo shared", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://a.com/1");
    }

    #[test]
    fn multi_word_queries_require_all_terms() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page("https://ex.com/a", "A", "alpha beta", "c1", "C1")).unwrap();
        idx.index_page(&page("https://ex.com/b", "B", "alpha gamma", "c1", "C1")).unwrap();
        idx.commit().unwrap();

        // AND-by-default: only the page containing BOTH words matches.
        let results = idx.search("alpha beta", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://ex.com/a");

        // A term present in neither-together combination returns nothing.
        let results = idx.search("beta gamma", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn title_matches_rank_above_body_matches() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        // The term is in page 1's title and page 2's body only.
        idx.index_page(&page("https://ex.com/title-hit", "kangaroo", "filler text", "c1", "C1")).unwrap();
        idx.index_page(&page("https://ex.com/body-hit", "filler", "kangaroo text", "c1", "C1")).unwrap();
        idx.commit().unwrap();

        let results = idx.search("kangaroo", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://ex.com/title-hit", "title match should rank first");
    }

    #[test]
    fn year_of_parses_leading_year() {
        assert_eq!(year_of("20210417120000"), Some(2021));
        assert_eq!(year_of("2021"), Some(2021));
        assert_eq!(year_of(""), None);
        assert_eq!(year_of("notadate"), None);
        assert_eq!(year_of("0099010100"), None); // implausible year
    }

    #[test]
    fn year_filter_exact_and_range() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page_ts("https://ex.com/2019", "20190101000000")).unwrap();
        idx.index_page(&page_ts("https://ex.com/2021", "20210101000000")).unwrap();
        idx.index_page(&page_ts("https://ex.com/2023", "20230101000000")).unwrap();
        idx.commit().unwrap();

        // Exact year.
        let r = idx.search("year:2021", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/2021");

        // Inclusive range.
        let r = idx.search("year:[2020 TO 2023]", 10).unwrap();
        assert_eq!(r.len(), 2, "2021 and 2023 fall in range");

        // Combined with a term (AND-scoped).
        let r = idx.search("shared year:2019", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/2019");
    }

    #[test]
    fn malformed_query_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page("https://ex.com/a", "A", "hello world", "c1", "C1")).unwrap();
        idx.commit().unwrap();

        // An unbalanced quote would be a parse error; lenient parsing must not
        // propagate it as an Err (the search box should never 500).
        assert!(idx.search("\"hello", 10).is_ok());
        assert!(idx.search("title:", 10).is_ok());
    }
}
