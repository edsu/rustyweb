use std::path::Path;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(FIXTURES).join(name)
}

fn make_index(paths: &[&str]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    for path in paths {
        rustyweb_lib::index::index_path(&fixture(path), tmp.path(), None).unwrap();
    }
    tmp
}

// ── Indexing ──────────────────────────────────────────────────────────────────

#[test]
fn index_warc_produces_cdx_records() {
    use rustyweb_lib::cdx::{CdxStore, MatchType};
    let tmp = make_index(&["simple.warc.gz"]);
    let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
    let records = store.query("http://example.com/", MatchType::Exact, None, None, 10).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, 200);
    assert_eq!(records[0].timestamp.len(), 14);
}

#[test]
fn index_warc_html_response_indexed() {
    let tmp = make_index(&["simple.warc.gz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("full_text").as_path()).unwrap();
    let results = idx.search("example", 10).unwrap();
    assert!(!results.is_empty(), "HTML content should be in fulltext index");
}

#[test]
fn index_warc_binary_not_indexed() {
    // The post.warc.gz captures a response with no HTML content-type, so
    // the fulltext index should either be empty or only contain pages with HTML.
    let tmp = make_index(&["post.warc.gz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("full_text").as_path()).unwrap();
    // The response in post.warc.gz has no HTML body, so fulltext should be empty.
    let results = idx.search("hello", 10).unwrap();
    assert!(results.is_empty(), "non-HTML responses should not be in fulltext index");
}

#[test]
fn index_wacz_extracts_inner_warcs() {
    use rustyweb_lib::cdx::{CdxStore, MatchType};
    let tmp = make_index(&["simple.wacz"]);
    let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
    let records = store.query("http://example.com/", MatchType::Exact, None, None, 10).unwrap();
    assert!(!records.is_empty(), "WACZ inner WARC should be indexed");
}

#[test]
fn index_post_request_encoded_key() {
    use rustyweb_lib::cdx::{CdxStore, MatchType};
    let tmp = make_index(&["post.warc.gz"]);
    let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
    // The POST form data "q=hello&page=1" should be encoded into the URL for keying.
    // Query with prefix to find any record for example.com/api.
    let records = store.query("http://example.com/api", MatchType::Prefix, None, None, 10).unwrap();
    assert!(!records.is_empty(), "POST record should be indexed under the API URL");
}

#[test]
fn index_incremental_is_idempotent() {
    use rustyweb_lib::cdx::{CdxStore, MatchType};
    let tmp = TempDir::new().unwrap();
    rustyweb_lib::index::index_path(&fixture("simple.warc.gz"), tmp.path(), None).unwrap();
    rustyweb_lib::index::index_path(&fixture("simple.warc.gz"), tmp.path(), None).unwrap();
    let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
    let records = store.query("http://example.com/", MatchType::Exact, None, None, 100).unwrap();
    // LSM-tree overwrites on same key, so we should still get exactly 1 record.
    assert_eq!(records.len(), 1, "re-indexing should not produce duplicates");
}

// ── CDX API ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cdx_api_exact_match() {
    let tmp = make_index(&["simple.warc.gz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/cdx/search/cdx?url=http://example.com/&output=json")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("example.com"), "CDX response should contain the URL: {text}");
}

#[tokio::test]
async fn cdx_api_prefix_match() {
    let tmp = make_index(&["simple.warc.gz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/cdx/search/cdx?url=http://example.com/&matchType=prefix&output=json")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(!text.is_empty(), "prefix match should return results: {text}");
}

#[tokio::test]
async fn cdx_api_time_range() {
    let tmp = make_index(&["simple.warc.gz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    // Use a future range that excludes everything.
    let req = Request::get("/cdx/search/cdx?url=http://example.com/&from=30000101000000&to=30000102000000")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.is_empty(), "future time range should return no results: {text}");
}

#[tokio::test]
async fn cdx_api_no_match_returns_empty() {
    let tmp = make_index(&["simple.warc.gz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/cdx/search/cdx?url=http://does-not-exist.example/")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(body.is_empty(), "unknown URL should return empty body");
}

#[tokio::test]
async fn cdx_api_fuzzy_fallback() {
    let tmp = make_index(&["simple.warc.gz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    // Query with a UTM param — fuzzy normalization should strip it and still find the record.
    let req = Request::get("/cdx/search/cdx?url=http://example.com/?utm_source=test&output=json")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("example.com"), "fuzzy fallback should find record: {text}");
}

// ── Search API ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_api_returns_results() {
    let tmp = make_index(&["simple.warc.gz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/api/search?q=example")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["results"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "search should return results: {json}"
    );
}

#[tokio::test]
async fn search_api_no_results() {
    let tmp = make_index(&["simple.warc.gz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/api/search?q=zzz_nonexistent_zzz")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let results = json["results"].as_array().unwrap();
    assert!(results.is_empty(), "nonexistent query should return empty results: {json}");
}

// ── Static assets ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn replay_assets_served() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/replay/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("ReplayWebPage") || text.contains("replay"), "should serve replay index: {text}");
}

// ── Real-fixture smoke tests ───────────────────────────────────────────────────
//
// Both a.warc.gz and a.wacz capture https://storymaps.arcgis.com/stories/278e1b5c18a3474082e583e889705179
// (a Browsertrix crawl of an ArcGIS StoryMaps page, status 200, text/html).

const REAL_URL: &str = "https://storymaps.arcgis.com/stories/278e1b5c18a3474082e583e889705179";

#[test]
fn index_real_warc_gz_produces_records() {
    use rustyweb_lib::cdx::{CdxStore, MatchType};
    let tmp = make_index(&["a.warc.gz"]);
    let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
    let records = store.query(REAL_URL, MatchType::Exact, None, None, 10).unwrap();
    assert!(!records.is_empty(), "real warc.gz should produce CDX records for {REAL_URL}");
    assert_eq!(records[0].status, 200);
    assert_eq!(records[0].timestamp.len(), 14);
}

#[test]
fn index_real_wacz_produces_records() {
    use rustyweb_lib::cdx::{CdxStore, MatchType};
    let tmp = make_index(&["a.wacz"]);
    let store = CdxStore::open(tmp.path().join("cdx").as_path()).unwrap();
    let records = store.query(REAL_URL, MatchType::Exact, None, None, 10).unwrap();
    assert!(!records.is_empty(), "real wacz should produce CDX records for {REAL_URL}");
    assert_eq!(records[0].status, 200);
}

#[test]
fn index_real_warc_gz_searchable() {
    // The captured page title is "2Tone: The Sound of Britain" (a StoryMaps story).
    // Body text is empty (Next.js SPA), but the title alone is enough to be indexed.
    let tmp = make_index(&["a.warc.gz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("full_text").as_path()).unwrap();
    let results = idx.search("Britain", 10).unwrap();
    assert!(!results.is_empty(), "real warc.gz HTML title should be searchable");
    assert!(results[0].url.contains("storymaps.arcgis.com"));
}

#[test]
fn index_real_wacz_searchable() {
    let tmp = make_index(&["a.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("full_text").as_path()).unwrap();
    let results = idx.search("Britain", 10).unwrap();
    assert!(!results.is_empty(), "real wacz HTML title should be searchable");
    assert!(results[0].url.contains("storymaps.arcgis.com"));
}
