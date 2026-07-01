use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::debug;

use crate::cdx::{CdxRecord, CdxStore, encode_post_url};
use crate::collections::{Collection, CollectionKind, CollectionManifest, collection_id, file_sha256};
use crate::search::{SearchIndex, extract_html_text};
use crate::warc::{WarcRecord, iter_records};

/// Index a WARC or WACZ file (or a directory of them) into the given index directory.
/// Idempotent: re-indexing the same file overwrites existing keys.
/// `name` sets the collection display name; defaults to the file's stem.
pub fn index_path(path: &Path, index_dir: &Path, name: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;

    let store = CdxStore::open(index_dir.join("cdx").as_path())
        .with_context(|| format!("opening CDX store at {}", index_dir.display()))?;
    let search = Mutex::new(
        SearchIndex::open(index_dir.join("full_text").as_path())
            .with_context(|| format!("opening search index at {}", index_dir.display()))?,
    );

    let paths: Vec<_> = if path.is_dir() {
        std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };

    let counts: Vec<u64> = paths
        .par_iter()
        .map(|p| index_single(p, &store, &search))
        .collect::<Result<Vec<_>>>()?;

    search.into_inner().unwrap().commit()?;

    // Update the collections manifest with one entry per indexed file.
    let mut manifest = CollectionManifest::open(index_dir)?;
    for (p, &count) in paths.iter().zip(counts.iter()) {
        let abs = p.canonicalize().unwrap_or_else(|_| p.clone());
        let collection_name = name
            .map(|n| n.to_string())
            .unwrap_or_else(|| file_display_name(&abs));
        let id = collection_id(&abs);
        let sha = file_sha256(&abs)
            .with_context(|| format!("computing sha256 of {}", abs.display()))?;
        let file_size = std::fs::metadata(&abs)
            .map(|m| m.len())
            .unwrap_or(0);
        let kind = CollectionKind::from_path(&abs);
        let date_indexed = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        manifest.upsert(Collection {
            id,
            path: abs,
            name: collection_name,
            kind,
            date_indexed,
            record_count: count,
            file_size,
            sha256: sha,
        });
    }
    manifest.save()?;

    Ok(())
}

/// Dispatch based on extension, return number of CDX records written.
fn index_single(path: &Path, store: &CdxStore, search: &Mutex<SearchIndex>) -> Result<u64> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "warc" | "gz" => index_warc(path, path, store, search),
        "wacz" => index_wacz(path, store, search),
        _ => {
            debug!("skipping unsupported file type: {}", path.display());
            Ok(0)
        }
    }
}

/// Parse every response/request record in a WARC file and insert a CDX entry.
///
/// `read_from` is the physical file to parse (may be a tempfile for WACZ-extracted WARCs).
/// `record_path` is the path stored in CDX records (the original WARC or parent WACZ path).
///
/// Returns the number of CDX records written.
fn index_warc(
    read_from: &Path,
    record_path: &Path,
    store: &CdxStore,
    search: &Mutex<SearchIndex>,
) -> Result<u64> {
    // Always store an absolute path so the server can match it against the manifest.
    let record_path_str = record_path
        .canonicalize()
        .unwrap_or_else(|_| record_path.to_path_buf())
        .to_string_lossy()
        .into_owned();

    let all: Vec<WarcRecord> = iter_records(read_from)
        .with_context(|| format!("reading {}", read_from.display()))?
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("parsing records from {}", read_from.display()))?;

    // Build a map from record_id → request record (for POST body lookup).
    let requests: std::collections::HashMap<String, &WarcRecord> = all
        .iter()
        .filter(|r| r.warc_type.eq_ignore_ascii_case("request"))
        .map(|r| (r.record_id.clone(), r))
        .collect();

    let mut count = 0u64;
    for record in &all {
        if !record.warc_type.eq_ignore_ascii_case("response") {
            continue;
        }
        if record.target_uri.is_empty() || record.target_uri.starts_with("dns:") {
            continue;
        }

        let paired_request = record
            .concurrent_to
            .as_deref()
            .and_then(|id| requests.get(id));

        let cdx = warc_record_to_cdx(record, paired_request, &record_path_str);
        store.insert(&cdx)?;
        count += 1;

        let mime = cdx.mimetype.to_ascii_lowercase();
        if mime.contains("html") && !record.payload.is_empty() {
            let (title, body) = extract_html_text(&record.payload);
            if !title.is_empty() || !body.is_empty() {
                search
                    .lock()
                    .unwrap()
                    .add_document(&record.target_uri, &record.timestamp, &title, &body)?;
            }
        }
    }

    Ok(count)
}

/// Convert a WARC response record (+ optional paired request) into a CDX entry.
fn warc_record_to_cdx(
    resp: &WarcRecord,
    req: Option<&&WarcRecord>,
    warc_path: &str,
) -> CdxRecord {
    let method = req
        .map(|r| http_method(&r.payload))
        .unwrap_or_else(|| "GET".to_string());

    let original_url = if method == "GET" {
        resp.target_uri.clone()
    } else {
        let post_body = req.map(|r| r.payload.as_slice()).unwrap_or(&[]);
        let ct = req
            .map(|r| r.content_type.as_str())
            .unwrap_or("application/x-www-form-urlencoded");
        encode_post_url(&resp.target_uri, ct, post_body)
    };

    CdxRecord {
        original_url,
        timestamp: resp.timestamp.clone(),
        mimetype: mime_without_params(&resp.content_type),
        status: resp.http_status.unwrap_or(0),
        digest: resp.digest.clone(),
        length: resp.record_length,
        warc_path: warc_path.to_string(),
        warc_offset: resp.offset,
        warc_record_length: resp.record_length,
    }
}

fn http_method(payload: &[u8]) -> String {
    let line_end = payload
        .iter()
        .position(|&b| b == b'\r' || b == b'\n')
        .unwrap_or(payload.len());
    let line = std::str::from_utf8(&payload[..line_end]).unwrap_or("");
    line.split_whitespace()
        .next()
        .unwrap_or("GET")
        .to_string()
}

/// Strip archive extensions to get a clean display name.
/// `simple.warc.gz` → `simple`, `my-archive.wacz` → `my-archive`.
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

fn mime_without_params(ct: &str) -> String {
    ct.split(';').next().unwrap_or(ct).trim().to_string()
}

/// Index a WACZ file by extracting its inner WARC archives and indexing each.
/// Stores the original WACZ path (not the tempfile path) in CDX records.
fn index_wacz(path: &Path, store: &CdxStore, search: &Mutex<SearchIndex>) -> Result<u64> {
    use crate::wacz::{extract_warc_from_wacz, iter_warc_paths};

    let mut total = 0u64;
    for entry_name_result in iter_warc_paths(path)? {
        let entry_name = entry_name_result?;
        let tmp = extract_warc_from_wacz(path, &entry_name)
            .with_context(|| format!("extracting {} from {}", entry_name, path.display()))?;
        // read_from = tempfile, record_path = original wacz (fixes warc_path bug)
        total += index_warc(tmp.path(), path, store, search)?;
    }
    Ok(total)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(FIXTURES).join(name)
    }

    #[test]
    fn index_warc_produces_cdx_entry() {
        let tmp = TempDir::new().unwrap();
        let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
        let search = Mutex::new(SearchIndex::open(tmp.path().join("ft").as_path()).unwrap());

        let fix = fixture("simple.warc.gz");
        index_warc(&fix, &fix, &store, &search).unwrap();

        let results = store
            .query("http://example.com/", crate::cdx::MatchType::Exact, None, None, 10)
            .unwrap();

        assert_eq!(results.len(), 1);
        let rec = &results[0];
        assert_eq!(rec.status, 200);
        assert_eq!(rec.timestamp.len(), 14);
        assert!(!rec.warc_path.is_empty());
    }

    #[test]
    fn index_warc_html_is_searchable() {
        let tmp = TempDir::new().unwrap();
        let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
        let search = Mutex::new(SearchIndex::open(tmp.path().join("ft").as_path()).unwrap());

        let fix = fixture("simple.warc.gz");
        index_warc(&fix, &fix, &store, &search).unwrap();
        search.into_inner().unwrap().commit().unwrap();

        let idx = crate::search::SearchIndex::open(tmp.path().join("ft").as_path()).unwrap();
        let results = idx.search("example", 10).unwrap();
        assert!(!results.is_empty(), "should find HTML content");
    }

    #[test]
    fn index_path_writes_manifest() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.warc.gz"), tmp.path(), Some("my-collection")).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(manifest.collections.len(), 1);
        let col = &manifest.collections[0];
        assert_eq!(col.name, "my-collection");
        assert_eq!(col.record_count, 1); // simple.warc.gz has one response record
        assert!(!col.sha256.is_empty());
        assert!(col.file_size > 0);
    }

    #[test]
    fn index_path_name_defaults_to_stem() {
        let tmp = TempDir::new().unwrap();
        index_path(&fixture("simple.warc.gz"), tmp.path(), None).unwrap();

        let manifest = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(manifest.collections[0].name, "simple");
    }

    #[test]
    fn wacz_records_use_wacz_path() {
        let tmp = TempDir::new().unwrap();
        let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
        let search = Mutex::new(SearchIndex::open(tmp.path().join("ft").as_path()).unwrap());

        let fix = fixture("simple.wacz");
        index_wacz(&fix, &store, &search).unwrap();

        // warc_path in CDX records should be the WACZ path, not a tempfile path
        let results = store
            .query("http://example.com/", crate::cdx::MatchType::Exact, None, None, 10)
            .unwrap();
        assert!(!results.is_empty());
        let warc_path = &results[0].warc_path;
        assert!(
            warc_path.ends_with("simple.wacz"),
            "warc_path should be the wacz path, got: {warc_path}"
        );
    }

    #[test]
    fn mime_stripping() {
        assert_eq!(mime_without_params("text/html; charset=utf-8"), "text/html");
        assert_eq!(mime_without_params("application/json"), "application/json");
    }
}
