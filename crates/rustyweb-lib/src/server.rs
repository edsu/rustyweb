use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;
use serde::Deserialize;
use tokio_util::io::ReaderStream;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::collections::{Collection, CollectionManifest};
use crate::search::SearchIndex;

// ── Embedded static assets ────────────────────────────────────────────────────

#[derive(RustEmbed)]
#[folder = "static/replay"]
struct ReplayAssets;

// ── AppState ──────────────────────────────────────────────────────────────────

struct AppState {
    search: SearchIndex,
    index_dir: PathBuf,
}

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router(index_dir: &Path) -> Result<Router> {
    let search = SearchIndex::open(index_dir.join("full_text").as_path())?;
    let state = Arc::new(AppState {
        search,
        index_dir: index_dir.to_path_buf(),
    });

    let app = Router::new()
        .route("/", get(homepage))
        .route("/search", get(search_page))
        .route("/files/{id}", get(serve_file))
        .route("/replay/viewer", get(replay_viewer))
        .route("/api/search", get(search_api))
        .route("/replay/", get(replay_index))
        .route("/replay/{*path}", get(replay_handler))
        .layer(CompressionLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<Body>| {
                    let ip = req.extensions()
                        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                        .map(|ci| ci.0.ip().to_string())
                        .unwrap_or_else(|| "-".to_string());
                    tracing::info_span!(
                        "request",
                        method = %req.method(),
                        uri = %req.uri(),
                        client_ip = %ip,
                    )
                })
                .on_response(|res: &Response, latency: std::time::Duration, _span: &tracing::Span| {
                    let ct = res.headers()
                        .get(axum::http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-");
                    let status = res.status().as_u16();
                    let ms = latency.as_millis();
                    if status >= 500 {
                        tracing::error!(status, content_type = ct, latency_ms = ms);
                    } else if status >= 400 {
                        tracing::warn!(status, content_type = ct, latency_ms = ms);
                    } else {
                        tracing::info!(status, content_type = ct, latency_ms = ms);
                    }
                }),
        )
        .with_state(state);

    Ok(app)
}

pub async fn serve(bind: &str, index_dir: &Path) -> Result<()> {
    let app = router(index_dir)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

// ── Homepage ──────────────────────────────────────────────────────────────────

async fn homepage(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let collections = load_collections(&state);

    let cards: String = collections
        .iter()
        .map(|c| {
            let status_class = if c.is_present() { "ok" } else { "missing" };
            let status_text = if c.is_present() { "✓ present" } else { "✗ missing" };
            let name = html_escape(&c.name);
            let path = html_escape(c.path.to_string_lossy().as_ref());
            let date = c.date_indexed.get(..10).unwrap_or(&c.date_indexed);

            let description = c.description.as_deref()
                .map(|d| format!("<p class=\"desc\">{}</p>", html_escape(d)))
                .unwrap_or_default();

            let crawl_date = c.crawl_date.as_deref()
                .map(|d| {
                    let short = d.get(..10).unwrap_or(d);
                    format!("<div class=\"meta-row\">Crawled: {short}</div>")
                })
                .unwrap_or_default();

            let seed_links: String = c.seed_pages.iter().take(5).map(|p| {
                let title = p.title.as_deref().unwrap_or(&p.url);
                let url_enc = url_encode(&p.url);
                let col_id = &c.id;
                let ts = ts_to_14digit(&p.ts);
                let viewer_href = format!("/replay/viewer?source=/files/{col_id}&url={url_enc}&ts={ts}&name={}", url_encode(&c.name));
                format!("<li><a href=\"{viewer_href}\">{}</a></li>", html_escape(title))
            }).collect();

            let seed_section = if seed_links.is_empty() {
                String::new()
            } else {
                format!("<ul class=\"seeds\">{seed_links}</ul>")
            };

            // Clicking the collection name opens it in the replay viewer.
            // Land on the first seed page if we have one, otherwise let the
            // component pick the collection's default entry point.
            let name_enc = url_encode(&c.name);
            let col_id = &c.id;
            let title_link = match c.seed_pages.first() {
                Some(p) => format!(
                    "/replay/viewer?source=/files/{col_id}&url={}&ts={}&name={name_enc}",
                    url_encode(&p.url),
                    ts_to_14digit(&p.ts),
                ),
                None => format!("/replay/viewer?source=/files/{col_id}&name={name_enc}"),
            };
            format!(
                r#"<div class="card">
  <div class="card-header">
    <a class="card-title" href="{title_link}">{name}</a>
    <span class="status {status_class}">{status_text}</span>
  </div>
  {description}
  {seed_section}
  <div class="card-footer">
    <span class="meta-row mono">{path}</span>
    {crawl_date}
    <span class="meta-row muted">Indexed: {date}</span>
  </div>
</div>"#
            )
        })
        .collect();

    let empty_msg = if collections.is_empty() {
        "<p class=\"muted\">No collections indexed yet. Run <code>rustyweb index &lt;path&gt;</code> to get started.</p>"
    } else {
        ""
    };

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>rustyweb</title>
  <style>
    * {{ box-sizing: border-box; }}
    body {{ font-family: sans-serif; max-width: 960px; margin: 3rem auto; padding: 0 1rem; color: #222; }}
    h1 {{ font-size: 2.5rem; margin-bottom: 0.25rem; }}
    .tagline {{ color: #666; margin-bottom: 2rem; }}
    .search-form {{ display: flex; gap: 0.5rem; margin-bottom: 3rem; }}
    .search-form input {{ flex: 1; padding: 0.6rem 0.8rem; font-size: 1rem; border: 1px solid #ccc; border-radius: 4px; }}
    .search-form button {{ padding: 0.6rem 1.2rem; font-size: 1rem; cursor: pointer; background: #0066cc; color: #fff; border: none; border-radius: 4px; }}
    h2 {{ font-size: 1.2rem; border-bottom: 1px solid #eee; padding-bottom: 0.4rem; }}
    .cards {{ display: grid; gap: 1.5rem; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); }}
    .card {{ border: 1px solid #ddd; border-radius: 8px; padding: 1rem 1.2rem; background: #fafafa; }}
    .card-header {{ display: flex; justify-content: space-between; align-items: baseline; gap: 0.5rem; margin-bottom: 0.5rem; }}
    .card-title {{ font-size: 1.1rem; font-weight: 600; color: #0066cc; text-decoration: none; }}
    .card-title:hover {{ text-decoration: underline; }}
    .desc {{ color: #444; font-size: 0.9rem; margin: 0.4rem 0; }}
    .seeds {{ margin: 0.5rem 0; padding-left: 1.2rem; font-size: 0.88rem; }}
    .seeds li {{ margin: 0.2rem 0; }}
    .seeds a {{ color: #0066cc; text-decoration: none; }}
    .seeds a:hover {{ text-decoration: underline; }}
    .card-footer {{ margin-top: 0.6rem; border-top: 1px solid #eee; padding-top: 0.5rem; font-size: 0.8rem; }}
    .meta-row {{ display: block; color: #666; }}
    .mono {{ font-family: monospace; font-size: 0.78rem; word-break: break-all; }}
    .muted {{ color: #999; }}
    .status {{ font-size: 0.8rem; white-space: nowrap; }}
    .ok {{ color: #2a7; }}
    .missing {{ color: #c33; }}
    a {{ color: #0066cc; text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
  </style>
</head>
<body>
  <h1>rustyweb</h1>
  <p class="tagline">Web archive search and replay</p>
  <form class="search-form" action="/search" method="get">
    <input type="search" name="q" placeholder="Search archived pages…" autofocus>
    <button type="submit">Search</button>
  </form>
  <h2>Collections</h2>
  {empty_msg}
  <div class="cards">{cards}</div>
</body>
</html>"#,
    );

    (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], html)
}

// ── Search results page ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchPageParams {
    q: String,
}

async fn search_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchPageParams>,
) -> impl IntoResponse {
    let q = params.q.trim().to_string();
    if q.is_empty() {
        return (
            StatusCode::SEE_OTHER,
            [("location", "/"), ("content-type", "text/html")],
            String::new(),
        )
            .into_response();
    }

    let results = match state.search.search(&q, 20) {
        Ok(r) => r,
        Err(e) => return error_response(e).into_response(),
    };

    let rows: String = results
        .iter()
        .map(|r| {
            let is_collection = r.doc_type == "collection";
            let title = html_escape(if r.title.is_empty() {
                if is_collection { &r.collection_name } else { &r.url }
            } else {
                &r.title
            });

            let col_name = html_escape(&r.collection_name);
            let url_enc = url_encode(&r.url);
            let name_enc = url_encode(&r.collection_name);

            let href = if is_collection {
                // Link to the collection's root in the viewer.
                format!("/replay/viewer?source=/files/{}&name={name_enc}", r.collection_id)
            } else {
                format!(
                    "/replay/viewer?source=/files/{}&url={url_enc}&ts={}&name={name_enc}",
                    r.collection_id, r.timestamp
                )
            };

            let snippet_html = if r.snippet.is_empty() {
                String::new()
            } else {
                format!("<div class=\"snippet\">{}</div>", r.snippet)
            };

            let url_display = if is_collection {
                format!("<span class=\"result-coll-badge\">Collection</span>")
            } else {
                format!("<div class=\"result-url\">{}</div>", html_escape(&r.url))
            };

            let ts_display = if !is_collection && !r.timestamp.is_empty() {
                format!("<div class=\"result-ts\">{}</div>", format_timestamp(&r.timestamp))
            } else {
                String::new()
            };

            format!(
                "<tr>\
                   <td>\
                     <div class=\"result-title\"><a href=\"{href}\">{title}</a></div>\
                     <div class=\"result-meta\">{url_display}{ts_display}</div>\
                     {snippet_html}\
                     <div class=\"result-coll\">in <em>{col_name}</em></div>\
                   </td>\
                   <td class=\"replay-col\">\
                     <a class=\"replay-btn\" href=\"{href}\">Replay →</a>\
                   </td>\
                 </tr>"
            )
        })
        .collect();

    let count_msg = match results.len() {
        0 => format!("No results for <em>{}</em>", html_escape(&q)),
        n => format!(
            "{n} result{} for <em>{}</em>",
            if n == 1 { "" } else { "s" },
            html_escape(&q)
        ),
    };

    let table = if rows.is_empty() {
        String::new()
    } else {
        format!("<table><tbody>{rows}</tbody></table>")
    };

    let q_escaped = html_escape(&q);
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>{q_escaped} - rustyweb</title>
  <style>
    * {{ box-sizing: border-box; }}
    body {{ font-family: sans-serif; max-width: 900px; margin: 2rem auto; padding: 0 1rem; color: #222; }}
    .top {{ display: flex; align-items: center; gap: 1rem; margin-bottom: 1.5rem; }}
    .top a {{ font-size: 1.4rem; font-weight: bold; text-decoration: none; color: #222; }}
    .search-form {{ display: flex; gap: 0.5rem; flex: 1; }}
    .search-form input {{ flex: 1; padding: 0.5rem 0.8rem; font-size: 1rem; border: 1px solid #ccc; border-radius: 4px; }}
    .search-form button {{ padding: 0.5rem 1rem; font-size: 1rem; cursor: pointer; background: #0066cc; color: #fff; border: none; border-radius: 4px; }}
    .count {{ color: #666; font-size: 0.9rem; margin-bottom: 1rem; }}
    table {{ width: 100%; border-collapse: collapse; }}
    tr {{ border-bottom: 1px solid #eee; }}
    td {{ padding: 0.8rem 0.4rem; vertical-align: top; }}
    .result-title {{ font-size: 1.05rem; font-weight: 500; }}
    .result-title a {{ color: #1a0dab; }}
    .result-url {{ font-size: 0.8rem; color: #006621; margin: 0.15rem 0; }}
    .result-ts {{ font-size: 0.8rem; color: #888; }}
    .result-coll {{ font-size: 0.8rem; color: #666; margin-top: 0.3rem; }}
    .result-coll-badge {{ display: inline-block; font-size: 0.75rem; background: #e8f0fe; color: #1967d2; padding: 0.1rem 0.4rem; border-radius: 3px; margin-bottom: 0.15rem; }}
    .snippet {{ font-size: 0.88rem; color: #444; margin: 0.4rem 0; line-height: 1.4; }}
    .snippet b {{ background: #fff3cd; font-weight: 600; }}
    .replay-col {{ width: 100px; text-align: right; white-space: nowrap; }}
    .replay-btn {{ display: inline-block; padding: 0.3rem 0.7rem; background: #0066cc; color: #fff; border-radius: 4px; font-size: 0.85rem; text-decoration: none; }}
    .replay-btn:hover {{ background: #0052a3; }}
    a {{ color: #0066cc; text-decoration: none; }}
  </style>
</head>
<body>
  <div class="top">
    <a href="/">rustyweb</a>
    <form class="search-form" action="/search" method="get">
      <input type="search" name="q" value="{q_escaped}">
      <button type="submit">Search</button>
    </form>
  </div>
  <div class="count">{count_msg}</div>
  {table}
</body>
</html>"#
    );

    (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], html).into_response()
}

// ── File serving ──────────────────────────────────────────────────────────────

async fn serve_file(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let collections = load_collections(&state);
    let Some(col) = collections.iter().find(|c| c.id == id) else {
        return (StatusCode::NOT_FOUND, "collection not found").into_response();
    };
    if !col.is_present() {
        return (StatusCode::NOT_FOUND, "archive file not found on disk").into_response();
    }

    let file_size = col.file_size;
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_byte_range(s, file_size));

    match tokio::fs::File::open(&col.path).await {
        Ok(mut file) => {
            const CONTENT_TYPE: &str = "application/octet-stream";
            const CORS_EXPOSE: &str = "Content-Length, Content-Range, Accept-Ranges";
            if let Some((start, end)) = range {
                use tokio::io::AsyncSeekExt;
                if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                    return error_response(anyhow::anyhow!(e)).into_response();
                }
                let length = end - start + 1;
                let limited = tokio::io::AsyncReadExt::take(file, length);
                let body = Body::from_stream(ReaderStream::new(limited));
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header("content-type", CONTENT_TYPE)
                    .header("content-length", length)
                    .header("content-range", format!("bytes {start}-{end}/{file_size}"))
                    .header("accept-ranges", "bytes")
                    .header("access-control-allow-origin", "*")
                    .header("access-control-expose-headers", CORS_EXPOSE)
                    .body(body)
                    .unwrap()
            } else {
                let body = Body::from_stream(ReaderStream::new(file));
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", CONTENT_TYPE)
                    .header("content-length", file_size)
                    .header("accept-ranges", "bytes")
                    .header("access-control-allow-origin", "*")
                    .header("access-control-expose-headers", CORS_EXPOSE)
                    .body(body)
                    .unwrap()
            }
        }
        Err(e) => error_response(anyhow::anyhow!(e)).into_response(),
    }
}

fn parse_byte_range(range: &str, file_size: u64) -> Option<(u64, u64)> {
    let s = range.strip_prefix("bytes=")?;
    if let Some(suffix_len) = s.strip_prefix('-') {
        let n: u64 = suffix_len.parse().ok()?;
        let start = file_size.saturating_sub(n);
        Some((start, file_size - 1))
    } else {
        let (start_str, end_str) = s.split_once('-')?;
        let start: u64 = start_str.parse().ok()?;
        let end = if end_str.is_empty() {
            file_size - 1
        } else {
            end_str.parse::<u64>().ok()?.min(file_size - 1)
        };
        Some((start, end))
    }
}

// ── Search API (JSON) ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    limit: Option<usize>,
}

async fn search_api(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(20).min(200);
    match state.search.search(&params.q, limit) {
        Ok(results) => {
            let body = serde_json::json!({
                "results": results.iter().map(|r| serde_json::json!({
                    "doc_type": r.doc_type,
                    "url": r.url,
                    "timestamp": r.timestamp,
                    "title": r.title,
                    "collection_id": r.collection_id,
                    "collection_name": r.collection_name,
                    "snippet": r.snippet,
                })).collect::<Vec<_>>()
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
        Err(e) => error_response(e),
    }
}

// ── ReplayWebPage static assets ───────────────────────────────────────────────

async fn replay_viewer(headers: HeaderMap) -> impl IntoResponse {
    serve_embedded_asset("viewer.html", &headers)
}

async fn replay_index() -> impl IntoResponse {
    (StatusCode::SEE_OTHER, [("location", "/")]).into_response()
}

async fn replay_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    serve_embedded_asset(&path, &headers)
}

/// Serve an embedded ReplayWebPage asset with an ETag derived from its content
/// hash and `Cache-Control: no-cache`. Browsers must revalidate on every load,
/// so a rebuild that changes an asset (e.g. `viewer.html`, `sw.js`) propagates
/// to clients on their next request instead of being masked by the HTTP cache.
/// When the client's `If-None-Match` matches, we return `304` with no body so
/// unchanged assets aren't re-downloaded.
fn serve_embedded_asset(path: &str, req_headers: &HeaderMap) -> Response {
    match ReplayAssets::get(path) {
        Some(content) => {
            let etag = etag_for(&content.metadata.sha256_hash());

            let matches = req_headers
                .get("if-none-match")
                .and_then(|v| v.to_str().ok())
                .map(|inm| inm == etag)
                .unwrap_or(false);

            if matches {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header("etag", &etag)
                    .header("cache-control", "no-cache")
                    .body(Body::empty())
                    .unwrap();
            }

            let mime = mime_guess_from_path(path);
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", mime)
                .header("etag", etag)
                .header("cache-control", "no-cache")
                .body(Body::from(content.data.to_vec()))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Build a quoted ETag from the first 8 bytes of a content hash.
fn etag_for(hash: &[u8]) -> String {
    let hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

fn mime_guess_from_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn load_collections(state: &AppState) -> Vec<Collection> {
    CollectionManifest::open(&state.index_dir)
        .map(|m| m.collections)
        .unwrap_or_default()
}

fn error_response(e: anyhow::Error) -> Response {
    tracing::error!("{e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Normalize a timestamp to the 14-digit form wabac.js expects. Seed pages in
/// `pages.jsonl` carry ISO 8601 timestamps (`2026-06-09T21:34:06.891Z`); wabac
/// wants `20260609213406`. Extract the digits and take the first 14.
fn ts_to_14digit(ts: &str) -> String {
    ts.chars().filter(|c| c.is_ascii_digit()).take(14).collect()
}

fn format_timestamp(ts: &str) -> String {
    if ts.len() >= 14 {
        format!(
            "{}-{}-{} {}:{}",
            &ts[0..4],
            &ts[4..6],
            &ts[6..8],
            &ts[8..10],
            &ts[10..12]
        )
    } else {
        ts.to_string()
    }
}
