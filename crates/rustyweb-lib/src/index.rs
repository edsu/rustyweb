use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::{debug, info};

use crate::collections::{Collection, CollectionManifest, Source, collection_id, file_sha256};
use crate::search::{SearchIndex, extract_html_text};
use crate::warc::{WarcRecord, iter_records};
use crate::wacz::{extract_warc_from_wacz, iter_warc_paths, read_datapackage};

/// Index a local WACZ file or directory of WACZ files. Thin wrapper over
/// [`index_location`] for callers that already have a filesystem path.
pub fn index_path(path: &Path, index_dir: &Path, name: Option<&str>) -> Result<()> {
    index_location(&path.to_string_lossy(), index_dir, name)
}

/// Index WACZ(s) from a location into the given index directory. The location is
/// a local file, a local directory (scanned for `.wacz`), or a remote
/// `http(s)://` URL (downloaded to a temp file for indexing).
///
/// Idempotent: re-indexing the same source upserts its manifest entry and
/// replaces its documents in Tantivy.
/// `name` overrides the collection display name; otherwise it comes from the
/// WACZ metadata, falling back to the filename/URL stem.
pub fn index_location(location: &str, index_dir: &Path, name: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;

    let search = Mutex::new(
        SearchIndex::open(index_dir.join("full_text").as_path())
            .with_context(|| format!("opening search index at {}", index_dir.display()))?,
    );

    let sources = resolve_sources(location)?;
    let mut manifest = CollectionManifest::open(index_dir)?;

    for source in &sources {
        index_one(source, &mut manifest, &search, name)?;
    }

    search.into_inner().unwrap().commit()?;
    manifest.save()?;

    Ok(())
}

/// Expand a location into the concrete WACZ sources to index. A directory is
/// scanned (non-recursively) for `.wacz` files; a URL or single file yields one
/// source. Local file paths are canonicalized so the derived collection id is
/// stable regardless of how the path was written.
fn resolve_sources(location: &str) -> Result<Vec<Source>> {
    match Source::parse(location) {
        url @ Source::Url(_) => Ok(vec![url]),
        Source::File(p) => {
            if p.is_dir() {
                let mut sources = Vec::new();
                for entry in std::fs::read_dir(&p)? {
                    let path = entry?.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("wacz") {
                        let abs = path.canonicalize().unwrap_or(path);
                        sources.push(Source::File(abs));
                    }
                }
                Ok(sources)
            } else if p.extension().and_then(|e| e.to_str()) == Some("wacz") {
                let abs = p.canonicalize().unwrap_or(p);
                Ok(vec![Source::File(abs)])
            } else {
                debug!("skipping non-WACZ path: {}", p.display());
                Ok(Vec::new())
            }
        }
    }
}

/// Index a single WACZ source: obtain a local readable copy (downloading a URL
/// to a temp file), index its pages and metadata, and upsert its manifest entry.
fn index_one(
    source: &Source,
    manifest: &mut CollectionManifest,
    search: &Mutex<SearchIndex>,
    name: Option<&str>,
) -> Result<()> {
    // A URL is downloaded to a temp file for indexing; the temp file is kept
    // alive (`_tmp`) for the duration of this function.
    let (local, _tmp): (PathBuf, Option<tempfile::NamedTempFile>) = match source {
        Source::File(p) => (p.clone(), None),
        Source::Url(u) => {
            info!(url = %u, "downloading remote WACZ for indexing");
            let tmp = download_to_temp(u).with_context(|| format!("downloading {u}"))?;
            (tmp.path().to_path_buf(), Some(tmp))
        }
    };

    let id = collection_id(source);

    // Read metadata from the WACZ datapackage.json up front so its title can
    // name the collection. Precedence: explicit --name, then the WACZ title,
    // then the filename/URL stem.
    let meta = read_datapackage(&local).unwrap_or_default();
    let display_name = name
        .map(|n| n.to_string())
        .or_else(|| meta.title.clone().filter(|t| !t.trim().is_empty()))
        .unwrap_or_else(|| source_display_name(source));

    // Drop any prior documents for this collection so re-indexing upserts
    // instead of appending duplicates.
    search.lock().unwrap().delete_collection(&id);

    // Use the resolved name for page documents so page and collection results
    // agree on the collection's name.
    index_wacz(&local, &id, &display_name, search)?;

    // Index the collection itself as a searchable document.
    let coll_body = build_collection_body(&meta);
    search
        .lock()
        .unwrap()
        .index_collection(&id, &display_name, &coll_body)?;

    let sha = file_sha256(&local)
        .with_context(|| format!("computing sha256 of {}", local.display()))?;
    let file_size = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
    let date_indexed = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    manifest.upsert(Collection {
        id,
        source: source.clone(),
        name: display_name,
        date_indexed,
        file_size,
        sha256: sha,
        description: meta.description,
        crawl_date: meta.created,
        seed_pages: meta.seed_pages,
    });

    Ok(())
}

/// Download a remote WACZ to a temp file for indexing.
fn download_to_temp(url: &str) -> Result<tempfile::NamedTempFile> {
    use std::io::{copy, Write};

    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("HTTP GET {url}"))?;
    let mut tmp = tempfile::Builder::new().suffix(".wacz").tempfile()?;
    let mut reader = resp.into_reader();
    copy(&mut reader, &mut tmp).with_context(|| format!("writing {url} to temp file"))?;
    tmp.flush()?;
    Ok(tmp)
}

/// Display name for a source: the WACZ filename stem, for a file or URL.
fn source_display_name(source: &Source) -> String {
    match source {
        Source::File(p) => file_display_name(p),
        Source::Url(u) => {
            let path = u.split(['?', '#']).next().unwrap_or(u);
            let base = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(u);
            base.strip_suffix(".wacz").unwrap_or(base).to_string()
        }
    }
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

        // PDF responses: extract the text and index it as the page body, with a
        // title derived from the URL's filename.
        if mime.contains("pdf") {
            if !record.payload.is_empty() {
                if let Some(text) = crate::pdf::extract_pdf_text(&record.payload) {
                    out.push(RawRecord::Html {
                        url: uri.to_string(),
                        timestamp: record.timestamp.clone(),
                        title: pdf_title_from_url(uri),
                        body: text,
                    });
                } else {
                    debug!(url = uri, "PDF text extraction yielded nothing; skipping");
                }
            }
            continue;
        }

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

/// Derive a page title for a PDF from the last path segment of its URL
/// (e.g. `https://x.org/docs/report.pdf` -> `report.pdf`), falling back to the
/// full URL when there is no usable segment.
fn pdf_title_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/')
        .find(|seg| !seg.is_empty())
        .unwrap_or(url)
        .to_string()
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
        // simple.wacz has no title in its datapackage, so the name falls back
        // to the filename stem.
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.wacz"), tmp.path(), None).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(manifest.collections[0].name, "simple");
    }

    #[test]
    fn index_name_comes_from_datapackage_title() {
        // pdf-doc.wacz has "title": "PDF Test Collection" in its datapackage,
        // which should name the collection when --name is not given.
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("pdf-doc.wacz"), tmp.path(), None).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(manifest.collections[0].name, "PDF Test Collection");
    }

    #[test]
    fn explicit_name_overrides_datapackage_title() {
        // --name wins even when the WACZ has a title.
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("pdf-doc.wacz"), tmp.path(), Some("Custom Name")).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(manifest.collections[0].name, "Custom Name");
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
