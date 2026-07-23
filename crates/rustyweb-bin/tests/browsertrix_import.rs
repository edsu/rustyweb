//! End-to-end test of `rustyweb import browsertrix` against a local mock
//! Browsertrix API. Spawns the built binary (Cargo exposes its path as
//! `CARGO_BIN_EXE_rustyweb`) pointed at an in-process axum server that serves
//! canned JSON plus a fixture WACZ, then checks the whole path: auth → org →
//! collection resolution → item listing → QA filter → resources → download →
//! index → provenance, and that a re-run skips what's already imported.

use std::path::{Path, PathBuf};
use std::process::Command;

use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../rustyweb-lib/tests/fixtures"
    ))
    .join(name)
}

/// Start the mock Browsertrix API on an ephemeral port; returns its base URL
/// (e.g. `http://127.0.0.1:54321`). The server runs on its own runtime thread
/// for the lifetime of the test process.
fn start_mock() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tx.send(base.clone()).unwrap();
            axum::serve(listener, router(base)).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

fn router(base: String) -> Router {
    let wacz = std::fs::read(fixture("simple.wacz")).unwrap();
    let size = wacz.len();

    // replay.json points the WACZ resource back at this mock's /files endpoint.
    let replay = move || {
        let base = base.clone();
        async move {
            Json(json!({
                "resources": [{
                    "name": "simple.wacz",
                    "path": format!("{base}/files/simple.wacz"),
                    "hash": "sha256:deadbeef",
                    "size": size,
                }]
            }))
        }
    };

    Router::new()
        .route(
            "/api/auth/jwt/login",
            post(|| async { Json(json!({"access_token": "tok", "token_type": "bearer"})) }),
        )
        .route(
            "/api/orgs",
            get(|| async {
                Json(json!({"items": [{"id": "o1", "slug": "demo", "name": "Demo"}], "total": 1}))
            }),
        )
        .route(
            "/api/orgs/{oid}/collections",
            get(|| async {
                Json(json!({"items": [{"id": "col-uuid", "slug": "news", "name": "News"}], "total": 1}))
            }),
        )
        .route(
            "/api/orgs/{oid}/all-crawls",
            get(move || async move {
                // One reviewed crawl, so the default reviewed-only filter keeps it.
                Json(json!({
                    "items": [{
                        "id": "item1",
                        "name": "News Crawl",
                        "type": "crawl",
                        "fileSize": size,
                        "reviewStatus": 5,
                    }],
                    "total": 1,
                }))
            }),
        )
        .route("/api/orgs/{oid}/crawls/{id}/replay.json", get(replay))
        .route(
            "/files/simple.wacz",
            get(move || {
                let wacz = wacz.clone();
                async move { wacz }
            }),
        )
}

/// Read the derived `waczs.json` array under `<home>/index` (collection
/// descriptive metadata lives in `<home>/collections/*.md` finding aids).
fn manifest_array(home: &Path, file: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(home.join("index").join(file)).unwrap();
    serde_json::from_str::<Value>(&text)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn import_a_collection_then_skip_on_rerun() {
    let base = start_mock();
    let home = TempDir::new().unwrap();

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_rustyweb"))
            .args(["import", "browsertrix"])
            .args(["--host", &base])
            .args(["--org", "demo"])
            .args(["--collection", "news"]) // resolved to col-uuid via /collections
            .arg("--home")
            .arg(home.path())
            .env("BROWSERTRIX_USER", "u")
            .env("BROWSERTRIX_PASSWORD", "p")
            .env_remove("BROWSERTRIX_TOKEN")
            .output()
            .unwrap()
    };

    // First import: downloads + indexes the one reviewed crawl.
    let out = run();
    assert!(
        out.status.success(),
        "import failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // WACZ landed under archive/, in the item's subdirectory.
    assert!(
        home.path().join("archive/news/item1/simple.wacz").exists(),
        "downloaded WACZ should be under <home>/archive/<collection-slug>/<item-id>/"
    );

    // Manifest records the crawl, with Browsertrix provenance.
    let waczs = manifest_array(home.path(), "waczs.json");
    assert_eq!(waczs.len(), 1);
    assert_eq!(waczs[0]["browsertrix"]["item_id"], "item1");
    assert_eq!(waczs[0]["browsertrix"]["resource_hash"], "sha256:deadbeef");
    // No --into was passed, so importing the Browsertrix "News" collection
    // should auto-create a matching rustyweb finding aid (not scatter singletons).
    let news_md = home.path().join("collections/news/README.md");
    assert!(
        news_md.exists(),
        "importing a collection should create a collections/news/README.md finding aid"
    );
    assert!(
        std::fs::read_to_string(&news_md)
            .unwrap()
            .contains("name: News"),
        "the finding aid front-matter should carry the collection name"
    );

    // Re-run: the crawl is already imported, so it's skipped (no duplicate).
    let out2 = run();
    assert!(out2.status.success());
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr2.contains("skipped 1") || stderr2.contains("imported 0"),
        "re-run should report a skip; stderr: {stderr2}"
    );
    assert_eq!(
        manifest_array(home.path(), "waczs.json").len(),
        1,
        "re-run must not add a duplicate crawl"
    );
}
