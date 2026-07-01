//! Headless-browser replay smoke test.
//!
//! This is the only test that exercises the part of rustyweb that truly matters
//! but can't be checked without a browser: whether ReplayWeb.page / wabac.js
//! actually renders an archived page from a WACZ we serve. It drives real
//! Chrome via WebDriver, so it's `#[ignore]`d by default.
//!
//! To run it:
//!
//! ```sh
//! chromedriver --port=9515 &          # or any WebDriver server
//! cargo test -p rustyweb-lib --test browser -- --ignored
//! ```
//!
//! Override the WebDriver endpoint with `WEBDRIVER_URL` (default
//! `http://localhost:9515`).

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use thirtyfour::prelude::*;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(FIXTURES).join(name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a running WebDriver (chromedriver) and browser; run with --ignored"]
async fn browser_renders_archived_page() {
    // 1. Index a.wacz - a real Browsertrix capture of an ArcGIS StoryMaps page
    //    ("2Tone: The Sound of Britain"). We use a real WACZ, not a hand-rolled
    //    fixture, because wabac.js requires a standard CDXJ index; the minimal
    //    simple.wacz fixture uses a non-standard CDX that wabac can't read.
    let tmp = tempfile::TempDir::new().unwrap();
    rustyweb_lib::index::index_path(&fixture("a.wacz"), tmp.path(), None).unwrap();
    let manifest = rustyweb_lib::collections::CollectionManifest::open(tmp.path()).unwrap();
    let id = manifest.collections[0].id.clone();

    // 2. Serve it on an ephemeral port (localhost is a secure context, so the
    //    service worker is allowed to register).
    let app = rustyweb_lib::server::router(tmp.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // 3. Connect to WebDriver (headless Chrome).
    let wd = std::env::var("WEBDRIVER_URL").unwrap_or_else(|_| "http://localhost:9515".into());
    let mut caps = DesiredCapabilities::chrome();
    caps.add_arg("--headless=new").unwrap();
    caps.add_arg("--no-sandbox").unwrap();
    caps.add_arg("--disable-gpu").unwrap();
    caps.add_arg("--disable-dev-shm-usage").unwrap();
    let driver = WebDriver::new(&wd, caps)
        .await
        .expect("connect to WebDriver - is `chromedriver --port=9515` running?");

    // 4. Drive the viewer and assert the archived page rendered. Always tear
    //    down the browser session and server, even on failure.
    let result = drive_and_check(&driver, addr, &id).await;
    let _ = driver.quit().await;
    server.abort();
    result.unwrap();
}

async fn drive_and_check(driver: &WebDriver, addr: SocketAddr, id: &str) -> Result<(), String> {
    // The 200 HTML capture of the story page (the arcg.is seed is a 301 to it).
    let page_url = "https://storymaps.arcgis.com/stories/278e1b5c18a3474082e583e889705179";
    let url = format!(
        "http://{addr}/replay/viewer?source=/files/{id}&url={page_url}&ts=20260609213407&name=2Tone"
    );
    driver.goto(&url).await.map_err(|e| e.to_string())?;

    // The banner is rendered by our own viewer JS immediately - a quick sanity
    // check that the viewer page itself loaded (it shows the current URL).
    if !deep_contains(driver, "storymaps.arcgis.com").await? {
        return Err("viewer banner (current URL) did not appear".into());
    }

    // wabac must install its service worker, read the WACZ over byte-range,
    // load its CDX, and render the page into a (same-origin) replay iframe.
    // Poll for the archived page's title text ("2Tone", present in the served
    // HTML <title>) to appear anywhere in the frame tree.
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut rendered = false;
    while Instant::now() <= deadline {
        if deep_contains(driver, "2Tone").await? {
            rendered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !rendered {
        let diag = diagnostics(driver).await.unwrap_or_else(|e| e);
        return Err(format!("archived page title '2Tone' never rendered.\nDIAG: {diag}"));
    }

    check_banner_tracks_navigation(driver).await
}

/// Verify the banner updates when ReplayWeb.page fires `rwp-url-change`. wabac
/// dispatches this (non-bubbling) event on the <replay-web-page> element as the
/// user navigates; our viewer listens on the element and updates the banner. We
/// dispatch a synthetic event so the check is deterministic (no reliance on
/// clicking links inside the SPA).
async fn check_banner_tracks_navigation(driver: &WebDriver) -> Result<(), String> {
    let new_url = "https://storymaps.arcgis.com/stories/navigation-check";
    driver
        .execute(
            "document.querySelector('replay-web-page')\
               .dispatchEvent(new CustomEvent('rwp-url-change', {detail: {url: arguments[0]}}));",
            vec![serde_json::json!(new_url)],
        )
        .await
        .map_err(|e| e.to_string())?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let banner = driver
        .execute(
            "return document.getElementById('current-url').textContent;",
            vec![],
        )
        .await
        .map_err(|e| e.to_string())?;
    let text = banner.json().as_str().unwrap_or("").to_string();
    if text == new_url {
        Ok(())
    } else {
        Err(format!("banner did not track rwp-url-change: expected {new_url}, got {text:?}"))
    }
}

async fn diagnostics(driver: &WebDriver) -> Result<String, String> {
    let script = r#"
        const out = {};
        out.loc = location.href;
        out.swController = !!(navigator.serviceWorker && navigator.serviceWorker.controller);
        const rwp = document.querySelector('replay-web-page');
        out.rwp = !!rwp;
        out.rwpShadow = !!(rwp && rwp.shadowRoot);
        const frames = [];
        function walk(node, depth) {
            if (!node) return;
            if (node.shadowRoot) walk(node.shadowRoot, depth);
            if (node.tagName === 'IFRAME') {
                let info = { src: node.getAttribute('src'), same: false, sample: null };
                try { if (node.contentDocument) { info.same = true;
                    info.sample = (node.contentDocument.body ? node.contentDocument.body.innerText : '').slice(0,120); } }
                catch (e) { info.err = String(e); }
                frames.push(info);
                try { if (node.contentDocument) walk(node.contentDocument, depth+1); } catch(e){}
            }
            const kids = node.childNodes || [];
            for (let i=0;i<kids.length;i++) walk(kids[i], depth);
        }
        walk(document, 0);
        out.frames = frames;
        return JSON.stringify(out);
    "#;
    let ret = driver
        .execute(script, vec![])
        .await
        .map_err(|e| e.to_string())?;
    Ok(ret.json().as_str().unwrap_or("<no diag>").to_string())
}

/// Return true if `needle` appears in any text node reachable from the document,
/// piercing shadow roots and same-origin iframes. ReplayWeb.page renders into a
/// shadow DOM and a replay iframe served from our own origin, so both are
/// reachable from page JS.
async fn deep_contains(driver: &WebDriver, needle: &str) -> Result<bool, String> {
    let script = r#"
        const needle = arguments[0];
        const acc = [];
        function collect(node) {
            if (!node) return;
            if (node.nodeType === 3) { acc.push(node.textContent); return; }
            if (node.shadowRoot) collect(node.shadowRoot);
            if (node.tagName === 'IFRAME') {
                try { collect(node.contentDocument); } catch (e) { /* cross-origin */ }
            }
            const kids = node.childNodes || [];
            for (let i = 0; i < kids.length; i++) collect(kids[i]);
        }
        collect(document);
        return acc.join(' ').indexOf(needle) !== -1;
    "#;
    let ret = driver
        .execute(script, vec![serde_json::json!(needle)])
        .await
        .map_err(|e| e.to_string())?;
    Ok(ret.json().as_bool().unwrap_or(false))
}
