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
        index_into(tmp.path(), path);
    }
    tmp
}

/// Copy a fixture WACZ into `<home>/archive` and index it from there. Local
/// WACZs must live under the archive folder, so tests stage them there first.
fn index_into(home: &Path, name: &str) {
    let archive = home.join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    let dest = archive.join(name);
    std::fs::copy(fixture(name), &dest).unwrap();
    rustyweb_lib::index::index_path(&dest, home, None).unwrap();
}

// ── Indexing ──────────────────────────────────────────────────────────────────

#[test]
fn index_wacz_html_response_indexed() {
    let tmp = make_index(&["simple.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
    let results = idx.search("example", 10).unwrap();
    assert!(!results.is_empty(), "HTML content from WACZ should be in fulltext index");
}

#[test]
fn index_wacz_collection_document_indexed() {
    let tmp = make_index(&["simple.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
    // The seed page URL ends with "example.com" so searching example.com finds the collection doc.
    let results = idx.search("example.com", 10).unwrap();
    assert!(
        results.iter().any(|r| r.doc_type == "collection"),
        "collection document should be searchable"
    );
}

#[test]
fn index_wacz_result_has_collection_fields() {
    let tmp = make_index(&["simple.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
    let results = idx.search("example", 10).unwrap();
    let page = results.iter().find(|r| r.doc_type == "page").unwrap();
    assert!(!page.collection_id.is_empty(), "page should have collection_id");
    assert_eq!(page.collection_name, "simple");
}

#[test]
fn index_wacz_writes_manifest_with_metadata() {
    let tmp = make_index(&["simple.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    assert_eq!(manifest.waczs.len(), 1);
    let col = &manifest.waczs[0];
    assert_eq!(col.name, "simple");
    assert!(!col.id.is_empty());
    assert!(!col.sha256.is_empty());
    // simple.wacz has a pages/pages.jsonl with one page
    assert!(!col.seed_pages.is_empty(), "should have seed pages from pages.jsonl");
}

// ── Search API ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_api_returns_results() {
    let tmp = make_index(&["simple.wacz"]);
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
async fn search_api_result_includes_collection_fields() {
    let tmp = make_index(&["simple.wacz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/api/search?q=example")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let results = json["results"].as_array().unwrap();
    let first = &results[0];
    assert!(first.get("collection_id").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false));
    assert!(first.get("collection_name").is_some());
    assert!(first.get("doc_type").is_some());
}

#[tokio::test]
async fn search_api_no_results() {
    let tmp = make_index(&["simple.wacz"]);
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

// ── File serving ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn files_route_serves_registered_wacz() {
    let tmp = make_index(&["simple.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    let id = &manifest.waczs[0].id;
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    let req = Request::get(format!("/files/{id}"))
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn files_route_range_request() {
    let tmp = make_index(&["simple.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    let id = &manifest.waczs[0].id;
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    let req = Request::get(format!("/files/{id}"))
        .header("range", "bytes=0-99")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.len(), 100, "byte range should return exactly 100 bytes");
}

#[tokio::test]
async fn files_route_unknown_id_404() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/files/deadbeef").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Replay contract ─────────────────────────────────────────────────────────
//
// wabac.js replays a WACZ by reading it over HTTP from /files/{id} with range
// requests. Actual rendering needs a browser, but these tests assert the
// server-side contract wabac depends on: the bytes we serve are exactly the
// WACZ on disk, ranges return the correct slice, the served archive is
// replayable content (its internal CDX resolves a page to a 200), and the
// viewer wires up <replay-web-page> so the service worker loads.

#[tokio::test]
async fn served_wacz_is_byte_identical_to_disk() {
    let tmp = make_index(&["a.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    let id = manifest.waczs[0].id.clone();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    let req = Request::get(format!("/files/{id}")).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let served = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

    let on_disk = std::fs::read(fixture("a.wacz")).unwrap();
    assert_eq!(served.len(), on_disk.len(), "served length should match file");
    assert_eq!(served.as_ref(), on_disk.as_slice(), "served bytes must equal the WACZ on disk");
}

#[tokio::test]
async fn served_range_matches_the_file_slice() {
    let tmp = make_index(&["a.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    let id = manifest.waczs[0].id.clone();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    // Request an interior slice and verify the exact bytes, not just the length.
    let req = Request::get(format!("/files/{id}"))
        .header("range", "bytes=100-199")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        &format!("bytes 100-199/{}", std::fs::metadata(fixture("a.wacz")).unwrap().len()),
    );
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

    let on_disk = std::fs::read(fixture("a.wacz")).unwrap();
    assert_eq!(body.as_ref(), &on_disk[100..=199], "range must return the exact file slice");
}

#[tokio::test]
async fn served_wacz_cdx_resolves_a_replayable_page() {
    let tmp = make_index(&["a.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    let id = manifest.waczs[0].id.clone();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    // Pull the whole WACZ through the HTTP endpoint the browser would use...
    let req = Request::get(format!("/files/{id}")).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let served = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

    // ...write it out and confirm its embedded CDX (what wabac reads) resolves
    // a real page. a.wacz's seed is a 301; the storymaps URL is the 200 target.
    let served_path = tmp.path().join("served.wacz");
    std::fs::write(&served_path, &served).unwrap();

    let records = rustyweb_lib::wacz::search_cdx(&served_path, REAL_URL).unwrap();
    let page = records.iter().find(|r| r.status == 200 && r.mime.contains("html"));
    assert!(page.is_some(), "served WACZ should contain a replayable 200 HTML page for {REAL_URL}");
}

#[tokio::test]
async fn viewer_wires_up_replay_web_page() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/replay/viewer").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(html.contains("replay-web-page"), "viewer must mount the component");
    // Absolute replaybase is what makes the service worker resolve to
    // /replay/sw.js rather than /replay/replay/sw.js - the bug we hit.
    assert!(html.contains("replaybase"), "viewer must set replaybase");
    assert!(html.contains("/replay/"), "replaybase should be the absolute /replay/ path");
    assert!(html.contains("rwp-url-change"), "viewer should track navigation for the banner");
}

// ── Static assets ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn replay_viewer_served() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/replay/viewer").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn replay_asset_has_etag_and_no_cache() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/replay/viewer").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("etag").is_some(), "asset should carry an ETag");
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "no-cache",
        "asset should be revalidated so new versions propagate"
    );
}

#[tokio::test]
async fn replay_asset_returns_304_when_etag_matches() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    // First request to learn the ETag.
    let req = Request::get("/replay/viewer").body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let etag = resp.headers().get("etag").unwrap().to_str().unwrap().to_string();

    // Second request with matching If-None-Match should be 304.
    let req = Request::get("/replay/viewer")
        .header("if-none-match", &etag)
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(body.is_empty(), "304 should have no body");
}

#[tokio::test]
async fn replay_root_redirects() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/replay/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // /replay/ now redirects to homepage
    assert!(
        resp.status().is_redirection(),
        "expected redirect, got {}",
        resp.status()
    );
}

// ── Homepage ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn homepage_shows_collection_name() {
    let tmp = make_index(&["simple.wacz"]);
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("simple"), "homepage should show collection name: {text}");
}

#[tokio::test]
async fn homepage_card_links_to_collection_page() {
    let tmp = make_index(&["simple.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    let id = manifest.waczs[0].id.clone();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let resp = app.oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains(&format!("href=\"/collection/{id}\"")),
        "homepage card title should link to the collection page"
    );
}

#[tokio::test]
async fn wacz_page_shows_metadata_and_pages() {
    let tmp = make_index(&["a.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    let id = manifest.waczs[0].id.clone();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    let resp = app
        .oneshot(Request::get(format!("/wacz/{id}")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(html.contains("SHA-256"), "should show fixity metadata");
    assert!(html.contains("Replay"), "should have a replay button");
    assert!(html.contains("Pages"), "should have a pages section");
    // a.wacz's seed page (title "2Tone: The Sound of Britain").
    assert!(html.contains("2Tone"), "should list the WACZ's pages");
}

#[tokio::test]
async fn collection_page_lists_members() {
    let tmp = make_index(&["a.wacz"]);
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    // Singleton collection: its id equals the WACZ's id.
    let coll_id = manifest.collections[0].id.clone();
    let wacz_id = manifest.waczs[0].id.clone();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();

    let resp = app
        .oneshot(Request::get(format!("/collection/{coll_id}")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(html.contains("WACZs"), "collection page should have a members section");
    assert!(
        html.contains(&format!("/wacz/{wacz_id}")),
        "collection page should link to its member WACZ"
    );
}

#[tokio::test]
async fn collection_page_unknown_id_404() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let resp = app
        .oneshot(Request::get("/collection/deadbeef").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn home_directory_is_portable() {
    use rustyweb_lib::collections::{Manifest, Source};

    // Build a home dir with the WACZ under <home>/archive, then index it.
    let base = TempDir::new().unwrap();
    let home_a = base.path().join("home-a");
    let archive = home_a.join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::copy(fixture("simple.wacz"), archive.join("simple.wacz")).unwrap();
    rustyweb_lib::index::index_path(&archive.join("simple.wacz"), &home_a, None).unwrap();

    // The source is stored relative to home (portable), not absolute.
    let manifest = Manifest::open(&home_a.join("index")).unwrap();
    let id = manifest.waczs[0].id.clone();
    assert_eq!(
        manifest.waczs[0].source,
        Source::File(Path::new("archive/simple.wacz").to_path_buf()),
        "local WACZ should be stored relative to home"
    );

    // Move the whole home dir to a new path, then serve from there.
    let home_b = base.path().join("home-b");
    std::fs::rename(&home_a, &home_b).unwrap();

    let app = rustyweb_lib::server::router(&home_b).unwrap();
    let resp = app
        .oneshot(Request::get(format!("/files/{id}")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "moved home should still resolve the WACZ");
}

#[tokio::test]
async fn can_index_while_server_holds_the_index() {
    // A running server opens the index read-only (no write lock), so indexing
    // must be able to proceed concurrently.
    let tmp = make_index(&["simple.wacz"]);
    let _app = rustyweb_lib::server::router(tmp.path()).unwrap(); // held, like a live server

    // This previously failed with a Tantivy LockBusy error.
    index_into(tmp.path(), "pdf-doc.wacz");

    // The newly indexed content is searchable.
    let idx = rustyweb_lib::search::SearchIndex::open_read_only(
        tmp.path().join("index").join("full_text").as_path(),
    )
    .unwrap();
    assert!(!idx.search("\"flux capacitor\"", 10).unwrap().is_empty());
}

#[tokio::test]
async fn homepage_empty_collections() {
    let tmp = TempDir::new().unwrap();
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let req = Request::get("/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("No collections"),
        "empty index should show placeholder: {text}"
    );
}

// ── Remote (HTTP) source ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_from_http_url_and_link_directly() {
    use axum::routing::get;

    // Serve the simple.wacz fixture bytes over a local HTTP server.
    let wacz = std::fs::read(fixture("simple.wacz")).unwrap();
    let app = axum::Router::new().route(
        "/simple.wacz",
        get(move || {
            let bytes = wacz.clone();
            async move { bytes }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service()).await.unwrap();
    });

    let url = format!("http://{addr}/simple.wacz");
    let tmp = TempDir::new().unwrap();

    // index_location uses a blocking HTTP client; run it off the async runtime.
    let (url_c, dir_c) = (url.clone(), tmp.path().to_path_buf());
    tokio::task::spawn_blocking(move || {
        rustyweb_lib::index::index_location(&url_c, &dir_c, None, None).unwrap();
    })
    .await
    .unwrap();
    server.abort();

    // The manifest records the URL as the source (not a local path).
    let manifest = rustyweb_lib::collections::Manifest::open(&tmp.path().join("index")).unwrap();
    assert_eq!(manifest.waczs.len(), 1);
    let col = &manifest.waczs[0];
    assert_eq!(col.source, rustyweb_lib::collections::Source::Url(url.clone()));

    // The downloaded WACZ was indexed and is searchable. Scope the index so its
    // writer lock is released before the router opens its own SearchIndex.
    {
        let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
        assert!(!idx.search("example", 10).unwrap().is_empty());
    }

    // The WACZ page links wabac directly at the remote URL, not through /files/{id}.
    let app2 = rustyweb_lib::server::router(tmp.path()).unwrap();
    let resp = app2
        .oneshot(Request::get(format!("/wacz/{}", col.id)).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("source=http%3A%2F%2F127.0.0.1"),
        "remote source should be used directly in viewer links"
    );
    assert!(
        !html.contains(&format!("/files/{}", col.id)),
        "remote source should not be routed through /files/{{id}}"
    );
}

// ── Real-fixture smoke tests ───────────────────────────────────────────────────

const REAL_URL: &str = "https://storymaps.arcgis.com/stories/278e1b5c18a3474082e583e889705179";

#[test]
fn index_real_wacz_searchable() {
    let tmp = make_index(&["a.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
    let results = idx.search("Britain", 10).unwrap();
    assert!(!results.is_empty(), "real wacz should be searchable for a term in its title/text");
    // The storymaps page (title "2Tone: The Sound of Britain") should be among the hits.
    assert!(
        results.iter().any(|r| r.url == REAL_URL),
        "the storymaps page should be a result"
    );
}

#[test]
fn index_real_wacz_has_correct_url() {
    let tmp = make_index(&["a.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
    let results = idx.search("Britain", 10).unwrap();
    assert!(
        results.iter().any(|r| r.doc_type == "page" && r.url == REAL_URL),
        "a page document for the storymaps URL should exist"
    );
}

#[test]
fn index_pdf_text_is_searchable() {
    // pdf-doc.wacz wraps a real PDF (generated from text) as an
    // application/pdf response. Its body text ("flux capacitor ...") exists
    // only inside the PDF, so a hit proves PDF extraction ran during indexing.
    let tmp = make_index(&["pdf-doc.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
    let results = idx.search("\"flux capacitor\"", 10).unwrap();
    assert!(!results.is_empty(), "PDF text should be searchable");
    let hit = &results[0];
    assert_eq!(hit.doc_type, "page");
    assert_eq!(hit.url, "http://example.com/report.pdf");
    assert!(
        hit.snippet.to_lowercase().contains("flux"),
        "snippet should highlight matched PDF text: {}",
        hit.snippet
    );
}

#[test]
fn index_real_wacz_indexes_rendered_text() {
    // The storymaps page is a Next.js SPA: its raw HTML body is nearly empty, so
    // before urn:text indexing only the title was searchable. Browsertrix's
    // urn:text record carries the fully rendered text (author name, body prose),
    // which we now index. "Scout Butler" (the author) appears only there.
    let tmp = make_index(&["a.wacz"]);
    let idx = rustyweb_lib::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
    let results = idx.search("\"Scout Butler\"", 10).unwrap();
    assert!(
        !results.is_empty(),
        "rendered-text-only phrase should be searchable via the urn:text record"
    );
    let hit = &results[0];
    assert_eq!(hit.doc_type, "page");
    assert!(
        hit.snippet.contains("Scout") || hit.snippet.contains("Butler"),
        "snippet should highlight the matched rendered text: {}",
        hit.snippet
    );
}
