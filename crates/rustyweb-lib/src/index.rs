use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::{debug, info};

use crate::collections::{Collection, CollectionManifest, collection_id, file_sha256};
use crate::search::{SearchIndex, extract_html_text};
use crate::warc::{WarcRecord, iter_records};
use crate::wacz::{extract_warc_from_wacz, iter_warc_paths, read_datapackage};

/// Index a WACZ file (or a directory of WACZ files) into the given index directory.
/// Idempotent: re-indexing the same file overwrites the existing manifest entry and
/// re-adds documents to Tantivy.
/// `name` sets the collection display name; defaults to the file's stem.
pub fn index_path(path: &Path, index_dir: &Path, name: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;

    let search = Mutex::new(
        SearchIndex::open(index_dir.join("full_text").as_path())
            .with_context(|| format!("opening search index at {}", index_dir.display()))?,
    );

    let paths: Vec<_> = if path.is_dir() {
        std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("wacz"))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };

    let mut manifest = CollectionManifest::open(index_dir)?;

    // Process each WACZ file (potentially in parallel across files if multiple).
    // Within each WACZ, WARC entries are processed in parallel.
    for p in &paths {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext != "wacz" {
            debug!("skipping non-WACZ file: {}", p.display());
            continue;
        }

        let abs = p.canonicalize().unwrap_or_else(|_| p.clone());
        let collection_name = name
            .map(|n| n.to_string())
            .unwrap_or_else(|| file_display_name(&abs));
        let id = collection_id(&abs);

        // Drop any prior documents for this collection so re-indexing upserts
        // instead of appending duplicates.
        search.lock().unwrap().delete_collection(&id);

        index_wacz(&abs, &id, &collection_name, &search)?;

        // Read metadata from WACZ datapackage.json.
        let meta = read_datapackage(&abs).unwrap_or_default();
        let display_name = meta.title.as_deref().unwrap_or(&collection_name).to_string();

        // Index the collection itself as a searchable document.
        let coll_body = build_collection_body(&meta);
        search
            .lock()
            .unwrap()
            .index_collection(&id, &display_name, &coll_body)?;

        let sha = file_sha256(&abs)
            .with_context(|| format!("computing sha256 of {}", abs.display()))?;
        let file_size = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
        let date_indexed = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        manifest.upsert(Collection {
            id,
            path: abs,
            name: display_name,
            date_indexed,
            file_size,
            sha256: sha,
            description: meta.description,
            crawl_date: meta.created,
            seed_pages: meta.seed_pages,
        });
    }

    search.into_inner().unwrap().commit()?;
    manifest.save()?;

    Ok(())
}

/// Build the body text for a collection-level Tantivy document from its metadata.
fn build_collection_body(meta: &crate::wacz::WaczMetadata) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(desc) = &meta.description {
        parts.push(desc.clone());
    }
    for page in &meta.seed_pages {
        if let Some(title) = &page.title {
            parts.push(title.clone());
        }
        parts.push(page.url.clone());
    }
    parts.join(" ")
}

/// A raw contribution to a page's search document, parsed from one WARC record.
enum RawRecord {
    /// An HTML response: source of the page title and a scraped-text fallback body.
    Html {
        url: String,
        timestamp: String,
        title: String,
        body: String,
    },
    /// A `urn:text:` resource record: Browsertrix's fully rendered (post-JS)
    /// page text. Richer than scraped HTML, especially for SPAs.
    Text {
        url: String,
        timestamp: String,
        text: String,
    },
}

/// Accumulated per-URL data merged from all WARC records for that page.
#[derive(Default)]
struct MergedPage {
    timestamp: String,
    title: Option<String>,
    html_body: Option<String>,
    rendered_text: Option<String>,
}

/// Index all WARC entries inside a WACZ file into the Tantivy full-text index.
///
/// Records are collected across every inner WARC (rendered `urn:text:` records
/// often live in a separate WARC from the HTML response), merged into one
/// document per URL, and indexed once. The body prefers Browsertrix's rendered
/// text and falls back to scraped HTML; the title comes from the HTML.
fn index_wacz(
    wacz_path: &Path,
    collection_id: &str,
    collection_name: &str,
    search: &Mutex<SearchIndex>,
) -> Result<()> {
    let warc_paths: Vec<_> = iter_warc_paths(wacz_path)?
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("listing WARC entries in {}", wacz_path.display()))?;

    let per_warc: Vec<Vec<RawRecord>> = warc_paths
        .par_iter()
        .map(|entry_name| {
            let tmp = extract_warc_from_wacz(wacz_path, entry_name)
                .with_context(|| format!("extracting {} from {}", entry_name, wacz_path.display()))?;
            collect_page_records(tmp.path())
        })
        .collect::<Result<Vec<_>>>()?;

    // Merge all records into one entry per URL.
    let mut pages: HashMap<String, MergedPage> = HashMap::new();
    for raw in per_warc.into_iter().flatten() {
        match raw {
            RawRecord::Html { url, timestamp, title, body } => {
                let e = pages.entry(url).or_default();
                // The HTML capture is the authoritative timestamp for replay.
                e.timestamp = timestamp;
                if !title.is_empty() {
                    e.title = Some(title);
                }
                if !body.is_empty() {
                    e.html_body = Some(body);
                }
            }
            RawRecord::Text { url, timestamp, text } => {
                let e = pages.entry(url).or_default();
                if e.timestamp.is_empty() {
                    e.timestamp = timestamp;
                }
                e.rendered_text = Some(text);
            }
        }
    }

    let mut count = 0u64;
    {
        let mut s = search.lock().unwrap();
        for (url, m) in pages {
            // Prefer the fully rendered text; fall back to scraped HTML.
            let body = m.rendered_text.or(m.html_body).unwrap_or_default();
            let title = m.title.unwrap_or_default();
            if title.is_empty() && body.is_empty() {
                continue;
            }
            s.index_page(&url, &m.timestamp, &title, &body, collection_id, collection_name)?;
            count += 1;
        }
    }

    info!(pages = count, wacz = %wacz_path.display(), "indexed pages from WACZ");
    Ok(())
}

/// Parse an extracted WARC file into raw per-record contributions (HTML
/// responses and `urn:text:` rendered-text resources). Other record types
/// (images, JS, CSS, redirects, other `urn:` pseudo-records) are ignored.
fn collect_page_records(warc_path: &Path) -> Result<Vec<RawRecord>> {
    let records: Vec<WarcRecord> = iter_records(warc_path)
        .with_context(|| format!("reading {}", warc_path.display()))?
        .collect::<Result<Vec<_>>>()?;

    let mut out = Vec::new();
    for record in &records {
        let uri = record.target_uri.as_str();
        if uri.is_empty() || uri.starts_with("dns:") {
            continue;
        }

        // Browsertrix stores fully rendered page text as a `urn:text:<url>`
        // resource record (WARC-Type: resource, not response). Map it back to
        // the real URL and use its plain-text payload as the body.
        if let Some(real_url) = uri.strip_prefix("urn:text:") {
            if record.payload.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(&record.payload).trim().to_string();
            if text.is_empty() {
                continue;
            }
            out.push(RawRecord::Text {
                url: real_url.to_string(),
                timestamp: record.timestamp.clone(),
                text,
            });
            continue;
        }

        // Skip other urn: pseudo-records (pageinfo, thumbnail, view, ...).
        if uri.starts_with("urn:") {
            continue;
        }

        // HTML responses give us the title and a scraped-text fallback body.
        if !record.warc_type.eq_ignore_ascii_case("response") {
            continue;
        }
        let mime = record.content_type.to_ascii_lowercase();
        if !mime.contains("html") || record.payload.is_empty() {
            continue;
        }
        let (title, body) = extract_html_text(&record.payload);
        if title.is_empty() && body.is_empty() {
            continue;
        }
        out.push(RawRecord::Html {
            url: uri.to_string(),
            timestamp: record.timestamp.clone(),
            title,
            body,
        });
    }

    Ok(out)
}

/// Strip archive extensions to get a clean display name.
fn file_display_name(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
    for suffix in &[".warc.gz", ".warc", ".wacz"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(FIXTURES).join(name)
    }

    #[test]
    fn index_path_wacz_writes_manifest() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), Some("my-collection")).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(manifest.collections.len(), 1);
        let col = &manifest.collections[0];
        assert_eq!(col.name, "my-collection");
        assert!(!col.sha256.is_empty());
        assert!(col.file_size > 0);
    }

    #[test]
    fn index_path_name_defaults_to_stem() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), None).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(manifest.collections[0].name, "simple");
    }

    #[test]
    fn index_wacz_html_is_searchable() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), None).unwrap();

        let idx = crate::search::SearchIndex::open(tmp.path().join("full_text").as_path()).unwrap();
        let results = idx.search("example", 10).unwrap();
        assert!(!results.is_empty(), "should find HTML content from WACZ");
        assert_eq!(results[0].collection_name, "simple");
    }

    #[test]
    fn index_wacz_stores_seed_pages_in_manifest() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), None).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        let col = &manifest.collections[0];
        assert!(
            !col.seed_pages.is_empty(),
            "simple.wacz has pages in pages.jsonl"
        );
        assert_eq!(col.seed_pages[0].url, "http://example.com/");
    }

    #[test]
    fn index_wacz_collection_is_searchable() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), None).unwrap();

        let idx = crate::search::SearchIndex::open(tmp.path().join("full_text").as_path()).unwrap();
        // The seed page URL "http://example.com/" is part of the collection body.
        let results = idx.search("example.com", 10).unwrap();
        assert!(
            results.iter().any(|r| r.doc_type == "collection"),
            "collection document should be searchable"
        );
    }

    #[test]
    fn reindexing_does_not_duplicate_documents() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), None).unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), None).unwrap();

        let idx = crate::search::SearchIndex::open(tmp.path().join("full_text").as_path()).unwrap();
        let results = idx.search("example", 50).unwrap();
        let pages = results.iter().filter(|r| r.doc_type == "page").count();
        assert_eq!(pages, 1, "re-indexing should upsert, not duplicate pages");
    }

    #[test]
    fn mime_display_name_strips_extension() {
        let p = Path::new("/data/my-archive.wacz");
        assert_eq!(file_display_name(p), "my-archive");
        let p2 = Path::new("/data/my.warc.gz");
        assert_eq!(file_display_name(p2), "my");
    }
}
