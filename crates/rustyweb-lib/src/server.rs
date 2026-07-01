use std::io::{Read, Seek, SeekFrom};
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

use crate::cdx::{CdxStore, MatchType, normalize_url_fuzzy};
use crate::collections::{Collection, CollectionManifest};
use crate::search::SearchIndex;

// ── Embedded static assets ────────────────────────────────────────────────────

#[derive(RustEmbed)]
#[folder = "static/replay"]
struct ReplayAssets;

// ── AppState ──────────────────────────────────────────────────────────────────

struct AppState {
    cdx: CdxStore,
    search: SearchIndex,
    index_dir: PathBuf,
}

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router(index_dir: &Path) -> Result<Router> {
    let cdx = CdxStore::open(index_dir.join("cdx").as_path())?;
    let search = SearchIndex::open(index_dir.join("full_text").as_path())?;
    let state = Arc::new(AppState {
        cdx,
        search,
        index_dir: index_dir.to_path_buf(),
    });

    let app = Router::new()
        .route("/", get(homepage))
        .route("/search", get(search_page))
        .route("/files/{id}", get(serve_file))
        .route("/replay/viewer", get(replay_viewer))
        .route("/cdx/search/cdx", get(cdx_api))
        .route("/api/search", get(search_api))
        .route("/warcreplay/{*path}", get(warc_replay))
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
    let rows: String = collections
        .iter()
        .map(|c| {
            let status = if c.is_present() {
                r#"<span class="ok">✓</span>"#
            } else {
                r#"<span class="missing">✗ missing</span>"#
            };
            let path = html_escape(c.path.to_string_lossy().as_ref());
            let name = html_escape(&c.name);
            let date = &c.date_indexed[..10]; // just the date part
            format!(
                "<tr><td><a href=\"/search?q=*\">{name}</a></td>\
                 <td class=\"mono\">{path}</td>\
                 <td>{}</td>\
                 <td>{date}</td>\
                 <td>{status}</td></tr>",
                c.record_count,
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
    body {{ font-family: sans-serif; max-width: 900px; margin: 3rem auto; padding: 0 1rem; color: #222; }}
    h1 {{ font-size: 2.5rem; margin-bottom: 0.25rem; }}
    .tagline {{ color: #666; margin-bottom: 2rem; }}
    .search-form {{ display: flex; gap: 0.5rem; margin-bottom: 3rem; }}
    .search-form input {{ flex: 1; padding: 0.6rem 0.8rem; font-size: 1rem; border: 1px solid #ccc; border-radius: 4px; }}
    .search-form button {{ padding: 0.6rem 1.2rem; font-size: 1rem; cursor: pointer; background: #0066cc; color: #fff; border: none; border-radius: 4px; }}
    h2 {{ font-size: 1.2rem; border-bottom: 1px solid #eee; padding-bottom: 0.4rem; }}
    table {{ width: 100%; border-collapse: collapse; font-size: 0.9rem; }}
    th, td {{ text-align: left; padding: 0.5rem 0.4rem; border-bottom: 1px solid #eee; }}
    th {{ background: #f7f7f7; font-weight: 600; }}
    .mono {{ font-family: monospace; font-size: 0.8rem; color: #555; }}
    .ok {{ color: #2a7; }}
    .missing {{ color: #c33; }}
    .muted {{ color: #888; }}
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
  <h2>Indexed collections</h2>
  {empty_msg}
  {table}
</body>
</html>"#,
        empty_msg = empty_msg,
        table = if rows.is_empty() {
            String::new()
        } else {
            format!(
                "<table><thead><tr>\
                 <th>Name</th><th>Path</th><th>Records</th><th>Indexed</th><th>Status</th>\
                 </tr></thead><tbody>{rows}</tbody></table>"
            )
        }
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
            // Find which collection this result lives in by looking up its CDX record.
            // We derive the collection ID directly from the CDX warc_path so that
            // replay works even for files not yet registered in collections.json.
            let replay_link = state
                .cdx
                .query(&r.url, MatchType::Exact, None, None, 1)
                .ok()
                .and_then(|records| records.into_iter().next())
                .map(|rec| {
                    let (outer, _) = split_warc_path(&rec.warc_path);
                    let col_id = crate::collections::collection_id(
                        std::path::Path::new(outer),
                    );
                    (col_id, rec.timestamp.clone())
                })
                .map(|(col_id, ts)| {
                    let url_enc = url_encode(&r.url);
                    format!(
                        "<a class=\"replay-btn\" href=\"/replay/viewer?source=/files/{col_id}&url={url_enc}&ts={ts}\">Replay →</a>",
                    )
                })
                .unwrap_or_default();

            let title = html_escape(if r.title.is_empty() { &r.url } else { &r.title });
            let url = html_escape(&r.url);
            let ts = format_timestamp(&r.timestamp);
            let title_cell = if replay_link.is_empty() {
                format!("<div class=\"result-title\">{title}</div>")
            } else {
                // Extract the href from replay_link so the title is also a link.
                let href = replay_link
                    .split_once("href=\"")
                    .and_then(|(_, rest)| rest.split_once('"'))
                    .map(|(h, _)| h)
                    .unwrap_or("#");
                format!("<div class=\"result-title\"><a href=\"{href}\">{title}</a></div>")
            };
            format!(
                "<tr>\
                   <td>{title_cell}\
                       <div class=\"result-url\">{url}</div>\
                       <div class=\"result-ts\">{ts}</div></td>\
                   <td class=\"replay-col\">{replay_link}</td>\
                 </tr>"
            )
        })
        .collect();

    let count_msg = match results.len() {
        0 => format!("No results for <em>{}</em>", html_escape(&q)),
        n => format!("{n} result{} for <em>{}</em>", if n == 1 { "" } else { "s" }, html_escape(&q)),
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
  <title>{q_escaped} — rustyweb</title>
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
    .result-title {{ font-size: 1.05rem; font-weight: 500; color: #1a0dab; }}
    .result-url {{ font-size: 0.8rem; color: #006621; margin: 0.15rem 0; }}
    .result-ts {{ font-size: 0.8rem; color: #888; }}
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
    let content_type = col.kind.content_type();
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_byte_range(s, file_size));

    match tokio::fs::File::open(&col.path).await {
        Ok(mut file) => {
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
                    .header("content-type", content_type)
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
                    .header("content-type", content_type)
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

// ── ir_ replay (pywb-style proxy) ────────────────────────────────────────────
//
// The wabac.js service worker, configured with `archiveMod: "ir_"`, fetches
// raw archived HTTP responses from `{archivePrefix}{ts}ir_/{url}`.  This
// handler performs the CDX lookup, reads the WARC record, and returns the
// original HTTP response so the SW can inject wombat.js and serve the page.

async fn replay_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(path): axum::extract::Path<String>,
    axum::extract::RawQuery(outer_query): axum::extract::RawQuery,
) -> impl IntoResponse {
    // If the path looks like a wabac.js archive request, dispatch to the archive handler.
    // Archive requests have the form: {id}/{ts}{modifier_}/{url}
    // Modifiers: ir_, oe_, cs_, js_, mp_, id_, if_, bn_, wkr_, …  (all end with _/)
    if let Some(slash) = path.find('/') {
        let id = &path[..slash];
        let rest = &path[slash + 1..];
        if has_archive_modifier(rest) {
            return ir_resource_inner(&state, id, rest, outer_query.as_deref());
        }
    }
    serve_embedded_asset(&path)
}

/// Returns true if `rest` contains a wabac.js/pywb archive modifier like `ir_/`, `oe_/`, `cs_/`.
fn has_archive_modifier(rest: &str) -> bool {
    // Find `_/` and check that the characters immediately before it are all lowercase ASCII.
    rest.find("_/").map_or(false, |pos| {
        pos > 0 && rest[..pos].ends_with(|c: char| c.is_ascii_lowercase())
    })
}

fn ir_resource_inner(state: &AppState, id: &str, rest: &str, outer_query: Option<&str>) -> Response {
    // rest = "{ts}{modifier_}/{url}"  e.g. "20230327164112oe_///www.w3.org/…"
    // Find the modifier: scan for _/ and walk back over lowercase letters.
    let Some(underscore_pos) = rest.find("_/") else {
        return (StatusCode::NOT_FOUND, "no archive modifier in path").into_response();
    };
    let mod_name_start = rest[..underscore_pos]
        .trim_end_matches(|c: char| c.is_ascii_lowercase())
        .len();
    let ts = &rest[..mod_name_start];
    let raw_url = &rest[underscore_pos + 2..]; // skip _/

    // CDX always stores absolute URLs with a scheme.  wabac.js can produce:
    //   - "https://host/path"  → use as-is
    //   - "//host/path"        → Chrome may normalise "modifier///host" → "modifier/host"
    //                            so raw_url may arrive as "//host/path" OR "host/path"
    // Normalise to an absolute URL and try both schemes if needed.
    let mut as_https = if raw_url.starts_with("https://") || raw_url.starts_with("http://") {
        raw_url.to_string()
    } else if raw_url.starts_with("//") {
        format!("https:{raw_url}")
    } else {
        // Chrome stripped the leading // — prepend the scheme.
        format!("https://{raw_url}")
    };

    // The inner URL's query string (e.g. ?w=50) arrives as the OUTER HTTP query
    // string, because HTTP treats the first `?` as the query delimiter.  Axum's
    // {*path} wildcard captures only the path component, so we must re-attach it.
    if let Some(q) = outer_query {
        if !q.is_empty() && !as_https.contains('?') {
            as_https.push('?');
            as_https.push_str(q);
        }
    }

    // Query all timestamps — sub-resources are often archived at a different
    // time than the page that requested them, so we find the closest match.
    let mut from_prefix = false;
    let records = {
        let mut r = state.cdx.query(&as_https, MatchType::Exact, None, None, 100)
            .unwrap_or_default();
        // Scheme fallback: try http:// if the https:// lookup returned nothing.
        if r.is_empty() {
            let as_http = format!("http://{}", &as_https["https://".len()..]);
            r = state.cdx.query(&as_http, MatchType::Exact, None, None, 100)
                .unwrap_or_default();
        }
        // Fuzzy fallback: strip tracking/noise query params and retry.
        if r.is_empty() {
            let fuzzy = normalize_url_fuzzy(&as_https);
            if fuzzy != as_https {
                r = state.cdx.query(&fuzzy, MatchType::Exact, None, None, 100)
                    .unwrap_or_default();
            }
        }
        // Last resort: prefix match on the bare URL path without any query string.
        // Handles CDN image URLs where ?w=N varies per request but the archive
        // only has certain sizes (e.g. ?w=20, ?w=800 but not ?w=50).
        if r.is_empty() {
            if let Ok(mut stripped) = url::Url::parse(&as_https) {
                if stripped.query().is_some() {
                    stripped.set_query(None);
                    r = state.cdx.query(stripped.as_str(), MatchType::Prefix, None, None, 100)
                        .unwrap_or_default();
                    if !r.is_empty() {
                        from_prefix = true;
                    }
                }
            }
        }
        r
    };

    // Find the CDX record belonging to this collection.
    // warc_path may be composite "wacz_path\x1einner_warc" — use only the outer
    // part for the collection-id hash.
    //
    // For exact/fuzzy matches, pick the record with the closest timestamp.
    // For prefix matches (quality variants like ?w=20 vs ?w=800), pick the
    // largest content size — that's the best-quality proxy available.
    let col_filter = |r: &&crate::cdx::CdxRecord| {
        let (outer, _) = split_warc_path(&r.warc_path);
        crate::collections::collection_id(std::path::Path::new(outer)) == id
    };
    let Some(record) = (if from_prefix {
        records.iter().filter(col_filter)
            .max_by_key(|r| (r.length, u64::MAX - ts_distance(&r.timestamp, ts)))
            .cloned()
    } else {
        records.iter().filter(col_filter)
            .min_by_key(|r| ts_distance(&r.timestamp, ts))
            .cloned()
    }) else {
        return (StatusCode::NOT_FOUND, "URL not in this collection").into_response();
    };

    // Build a minimal Collection from the CDX record — no manifest entry required.
    let (outer_path_str, inner_warc) = split_warc_path(&record.warc_path);
    let warc_path = std::path::PathBuf::from(outer_path_str);
    let kind = crate::collections::CollectionKind::from_path(&warc_path);
    let col = crate::collections::Collection {
        id: id.to_string(),
        path: warc_path,
        name: String::new(),
        kind,
        date_indexed: String::new(),
        record_count: 0,
        file_size: 0,
        sha256: String::new(),
    };
    if !col.is_present() {
        return (StatusCode::NOT_FOUND, "archive not on disk").into_response();
    }

    let raw = match read_record_from_collection(&col, inner_warc, record.warc_offset, record.warc_record_length)
    {
        Ok(b) => b,
        Err(e) => return error_response(e),
    };

    match crate::warc::parse_warc_bytes(&raw) {
        Ok(Some(wr)) => {
            let status = wr.http_status.unwrap_or(200);
            let body_len = wr.payload.len();
            let mut builder = Response::builder()
                .status(status)
                .header("x-archive-ts", &record.timestamp)
                .header("access-control-allow-origin", "*");

            // Forward original HTTP response headers.
            // Strip hop-by-hop and replay-hostile headers; we set content-length ourselves.
            const SKIP: &[&str] = &[
                "content-length",
                "transfer-encoding",
                "connection",
                "keep-alive",
                "proxy-authenticate",
                "proxy-authorization",
                "te",
                "trailers",
                "upgrade",
                // Strip headers that break iframe embedding and script injection.
                "x-frame-options",
                "content-security-policy",
                "content-security-policy-report-only",
            ];
            for (name, value) in &wr.http_headers {
                if SKIP.contains(&name.to_ascii_lowercase().as_str()) {
                    continue;
                }
                // Rewrite redirect Location headers so the browser stays in the archive.
                // The original Location is an absolute live URL; point it back to our
                // replay endpoint so the follow goes through ir_resource_inner again.
                if name.eq_ignore_ascii_case("location") && (300..400).contains(&status) {
                    let loc = value.trim();
                    if loc.starts_with("http://") || loc.starts_with("https://") {
                        builder = builder.header(
                            "location",
                            format!("/replay/{id}/{ts}ir_/{loc}"),
                        );
                        continue;
                    }
                }
                builder = builder.header(name.as_str(), value.as_str());
            }

            // Ensure content-type has a fallback and content-length is authoritative.
            if wr.content_type.is_empty() {
                builder = builder.header("content-type", "application/octet-stream");
            }
            builder
                .header("content-length", body_len)
                .body(Body::from(wr.payload))
                .unwrap()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "empty WARC record").into_response(),
        Err(e) => error_response(e),
    }
}

/// Split a CDX `warc_path` into the outer path and optional inner WARC entry name.
///
/// For plain WARC files: `("/path/file.warc.gz", None)`
/// For WACZ inner WARCs: `("/path/file.wacz", Some("archive/rec-XXXX.warc.gz"))`
fn split_warc_path(warc_path: &str) -> (&str, Option<&str>) {
    if let Some(pos) = warc_path.find('\x1e') {
        (&warc_path[..pos], Some(&warc_path[pos + 1..]))
    } else {
        (warc_path, None)
    }
}

/// Read raw WARC record bytes from either a plain WARC or a named inner WARC inside a WACZ.
fn read_record_from_collection(
    col: &Collection,
    inner_warc: Option<&str>,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    use crate::collections::CollectionKind;
    match col.kind {
        CollectionKind::Warc => read_warc_slice(&col.path, offset, length),
        CollectionKind::Wacz => {
            use crate::wacz::extract_warc_from_wacz;
            let entry = inner_warc.ok_or_else(|| {
                anyhow::anyhow!(
                    "CDX record for {} is missing inner WARC name (reindex required)",
                    col.path.display()
                )
            })?;
            let tmp = extract_warc_from_wacz(&col.path, entry)?;
            read_warc_slice(tmp.path(), offset, length)
        }
    }
}

// ── CDX API ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CdxParams {
    url: String,
    #[serde(rename = "matchType")]
    match_type: Option<String>,
    from: Option<String>,
    to: Option<String>,
    limit: Option<usize>,
    output: Option<String>,
}

async fn cdx_api(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CdxParams>,
) -> impl IntoResponse {
    let match_type = match params.match_type.as_deref().unwrap_or("exact") {
        "prefix" => MatchType::Prefix,
        "domain" => MatchType::Domain,
        _ => MatchType::Exact,
    };
    let limit = params.limit.unwrap_or(10_000).min(100_000);

    let mut records = match state.cdx.query(
        &params.url,
        match_type,
        params.from.as_deref(),
        params.to.as_deref(),
        limit,
    ) {
        Ok(r) => r,
        Err(e) => return error_response(e),
    };

    if records.is_empty() && params.match_type.as_deref().unwrap_or("exact") == "exact" {
        let fuzzy = normalize_url_fuzzy(&params.url);
        if fuzzy != params.url {
            if let Ok(r) = state.cdx.query(&fuzzy, MatchType::Exact, None, None, limit) {
                records = r;
            }
        }
    }

    let output = params.output.as_deref().unwrap_or("json");

    if output == "json" {
        let mut lines = String::new();
        for rec in &records {
            let surt = crate::cdx::to_surt(&rec.original_url);
            if !lines.is_empty() {
                lines.push('\n');
            }
            let line = serde_json::json!([
                surt,
                rec.timestamp,
                rec.original_url,
                rec.mimetype,
                rec.status.to_string(),
                rec.digest,
                rec.length.to_string(),
                rec.warc_offset.to_string(),
                rec.warc_path,
            ]);
            lines.push_str(&line.to_string());
        }
        (
            StatusCode::OK,
            [("content-type", "application/x-ndjson; charset=utf-8")],
            lines,
        )
            .into_response()
    } else {
        let mut lines = String::new();
        for rec in &records {
            let surt = crate::cdx::to_surt(&rec.original_url);
            lines.push_str(&format!(
                "{} {} {} {} {} {} {} {} {}\n",
                surt,
                rec.timestamp,
                rec.original_url,
                rec.mimetype,
                rec.status,
                rec.digest,
                rec.length,
                rec.warc_offset,
                rec.warc_path,
            ));
        }
        (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            lines,
        )
            .into_response()
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
                    "url": r.url,
                    "timestamp": r.timestamp,
                    "title": r.title,
                })).collect::<Vec<_>>()
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
        Err(e) => error_response(e),
    }
}

// ── WARC replay ───────────────────────────────────────────────────────────────

async fn warc_replay(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let Some((warc_path, range_str)) = path.split_once('@') else {
        return (StatusCode::BAD_REQUEST, "missing @ separator in path").into_response();
    };
    let Some((offset_str, length_str)) = range_str.split_once('+') else {
        return (StatusCode::BAD_REQUEST, "missing + in range").into_response();
    };
    let offset: u64 = match offset_str.parse() {
        Ok(n) => n,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad offset").into_response(),
    };
    let length: u64 = match length_str.parse() {
        Ok(n) => n,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad length").into_response(),
    };

    let abs_path = if std::path::Path::new(warc_path).is_absolute() {
        std::path::PathBuf::from(warc_path)
    } else {
        state.index_dir.join(warc_path)
    };

    match read_warc_slice(&abs_path, offset, length) {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "application/warc".parse().unwrap());
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(e) => error_response(e),
    }
}

fn read_warc_slice(path: &Path, offset: u64, length: u64) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Absolute difference between two 14-digit WARC timestamps (YYYYMMDDHHMMSS).
fn ts_distance(a: &str, b: &str) -> u64 {
    let av: i64 = a.parse().unwrap_or(0);
    let bv: i64 = b.parse().unwrap_or(0);
    (av - bv).unsigned_abs()
}

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

/// Format a 14-digit WARC timestamp as a human-readable date string.
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

// ── ReplayWebPage static assets ───────────────────────────────────────────────

async fn replay_viewer() -> impl IntoResponse {
    serve_embedded_asset("viewer.html")
}

async fn replay_index() -> impl IntoResponse {
    serve_embedded_asset("index.html")
}


fn serve_embedded_asset(path: &str) -> Response {
    match ReplayAssets::get(path) {
        Some(content) => {
            let mime = mime_guess_from_path(path);
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                mime.parse().unwrap_or("application/octet-stream".parse().unwrap()),
            );
            (StatusCode::OK, headers, content.data.to_vec()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
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
