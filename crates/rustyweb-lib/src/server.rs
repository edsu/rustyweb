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

use crate::collections::{Manifest, Wacz};
use crate::search::SearchIndex;

// ── Embedded static assets ────────────────────────────────────────────────────

#[derive(RustEmbed)]
#[folder = "static/replay"]
struct ReplayAssets;

// ── AppState ──────────────────────────────────────────────────────────────────

struct AppState {
    search: SearchIndex,
    /// rustyweb home directory; local WACZ sources resolve against it.
    home: PathBuf,
    /// `<home>/index`, where the manifest and full-text index live.
    index_dir: PathBuf,
}

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router(home: &Path) -> Result<Router> {
    let index_dir = crate::index::index_dir(home);
    // Read-only: the server never writes, so it must not hold the write lock,
    // which would block `rustyweb index` from running while serving.
    let search = SearchIndex::open_read_only(index_dir.join("full_text").as_path())?;
    let state = Arc::new(AppState {
        search,
        home: home.to_path_buf(),
        index_dir,
    });

    let app = Router::new()
        .route("/", get(homepage))
        .route("/search", get(search_page))
        .route("/collection/{id}", get(collection_page))
        .route("/wacz/{id}", get(wacz_page))
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

pub async fn serve(bind: &str, home: &Path) -> Result<()> {
    let app = router(home)?;
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
    let manifest = match Manifest::open(&state.index_dir) {
        Ok(m) => m,
        Err(e) => return error_response(e).into_response(),
    };

    let cards: String = manifest
        .collections
        .iter()
        .map(|c| {
            let members: Vec<&Wacz> = manifest.members_of(&c.id).collect();
            let name = html_escape(&c.name);
            let title_link = format!("/collection/{}", c.id);
            let n = members.len();
            let count_label = format!("{n} WACZ{}", if n == 1 { "" } else { "s" });

            let description = c
                .description
                .as_deref()
                .map(|d| format!("<p class=\"desc\">{}</p>", html_escape(d)))
                .unwrap_or_default();

            // Show the collection's capture date range (temporal span is
            // meaningful at the collection level; per-tool software lives on the
            // WACZ detail page).
            let prov = match members_capture_range(&members) {
                Some(r) => format!("<div class=\"prov\">{}</div>", html_escape(&r)),
                None => String::new(),
            };

            format!(
                r#"<div class="card">
  <div class="card-header">
    <a class="card-title" href="{title_link}">{name}</a>
    <span class="status muted">{count_label}</span>
  </div>
  {prov}
  {description}
</div>"#
            )
        })
        .collect();

    let empty_msg = if manifest.collections.is_empty() {
        "<p class=\"muted\">No collections indexed yet. Run <code>rustyweb index archive/*.wacz</code> to get started.</p>"
    } else {
        ""
    };

    let tips = search_tips_html();

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
    .tips {{ margin: -2rem 0 2.5rem; font-size: 0.88rem; }}
    .tips summary {{ cursor: pointer; color: #0066cc; width: fit-content; }}
    .tips-body {{ margin-top: 0.6rem; padding: 0.8rem 1rem; background: #f6f8fa; border: 1px solid #e5e7eb; border-radius: 6px; color: #333; }}
    .tips-body p {{ margin: 0.4rem 0; }}
    .tips-body ul {{ margin: 0.5rem 0; padding-left: 1.2rem; }}
    .tips-body li {{ margin: 0.25rem 0; }}
    .tips-body code {{ background: #eef1f4; padding: 0.05rem 0.3rem; border-radius: 3px; font-family: monospace; font-size: 0.85em; }}
    .tips-note {{ color: #666; margin-bottom: 0 !important; }}
    h2 {{ font-size: 1.2rem; border-bottom: 1px solid #eee; padding-bottom: 0.4rem; }}
    .cards {{ display: grid; gap: 1.5rem; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); }}
    .card {{ border: 1px solid #ddd; border-radius: 8px; padding: 1rem 1.2rem; background: #fafafa; }}
    .card-header {{ display: flex; justify-content: space-between; align-items: baseline; gap: 0.5rem; margin-bottom: 0.5rem; }}
    .card-title {{ font-size: 1.1rem; font-weight: 600; color: #0066cc; text-decoration: none; }}
    .card-title:hover {{ text-decoration: underline; }}
    .desc {{ color: #444; font-size: 0.9rem; margin: 0.4rem 0; }}
    .prov {{ font-size: 0.82rem; color: #1a4d7a; background: #eef4fb; border: 1px solid #d6e4f2; border-radius: 4px; padding: 0.3rem 0.55rem; margin: 0.2rem 0 0.6rem; line-height: 1.4; }}
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
  {tips}
  <h2>Collections</h2>
  {empty_msg}
  <div class="cards">{cards}</div>
</body>
</html>"#,
    );

    (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], html).into_response()
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

    // Map each collection id to the wabac `source` to use: /files/{id} for a
    // local WACZ, or the remote URL directly for an http source.
    let collections = load_waczs(&state);
    let source_for = |cid: &str| -> String {
        collections
            .iter()
            .find(|c| c.id == cid)
            .map(viewer_source)
            .unwrap_or_else(|| format!("/files/{cid}"))
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
            let coll_id = html_escape(&r.collection_id);
            let url_enc = url_encode(&r.url);
            let name_enc = url_encode(&r.collection_name);
            let source_enc = url_encode(&source_for(&r.collection_id));

            let href = if is_collection {
                // Link to the collection's root in the viewer.
                format!("/replay/viewer?source={source_enc}&name={name_enc}")
            } else {
                format!(
                    "/replay/viewer?source={source_enc}&url={url_enc}&ts={}&name={name_enc}",
                    r.timestamp
                )
            };

            // Prefer the hit-highlighted body snippet; if the query didn't match
            // the body (e.g. a title-only or URL-only hit), fall back to the
            // page's description so the result still has context.
            let snippet_html = if !r.snippet.is_empty() {
                format!("<div class=\"snippet\">{}</div>", r.snippet)
            } else if !r.description.is_empty() {
                format!("<div class=\"snippet\">{}</div>", html_escape(&r.description))
            } else {
                String::new()
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
                     <div class=\"result-coll\">in <a href=\"/wacz/{coll_id}\"><em>{col_name}</em></a></div>\
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
    let tips = search_tips_html();
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
    .tips {{ margin: 0 0 1.5rem; font-size: 0.85rem; }}
    .tips summary {{ cursor: pointer; color: #0066cc; width: fit-content; }}
    .tips-body {{ margin-top: 0.6rem; padding: 0.8rem 1rem; background: #f6f8fa; border: 1px solid #e5e7eb; border-radius: 6px; color: #333; }}
    .tips-body p {{ margin: 0.4rem 0; }}
    .tips-body ul {{ margin: 0.5rem 0; padding-left: 1.2rem; }}
    .tips-body li {{ margin: 0.25rem 0; }}
    .tips-body code {{ background: #eef1f4; padding: 0.05rem 0.3rem; border-radius: 3px; font-family: monospace; font-size: 0.85em; }}
    .tips-note {{ color: #666; margin-bottom: 0 !important; }}
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
  {tips}
  <div class="count">{count_msg}</div>
  {table}
</body>
</html>"#
    );

    (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], html).into_response()
}

// ── Collection detail page ──────────────────────────────────────────────────

async fn collection_page(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let manifest = match Manifest::open(&state.index_dir) {
        Ok(m) => m,
        Err(e) => return error_response(e).into_response(),
    };
    let Some(c) = manifest.collection_by_id(&id) else {
        return (StatusCode::NOT_FOUND, "collection not found").into_response();
    };
    let members: Vec<&Wacz> = manifest.members_of(&id).collect();

    let name = html_escape(&c.name);
    let description = c
        .description
        .as_deref()
        .map(|d| format!("<p class=\"desc\">{}</p>", html_escape(d)))
        .unwrap_or_default();

    // Aggregates derived from members.
    let total_size: u64 = members.iter().map(|w| w.file_size).sum();
    let software = collection_software(&members);
    let range = members_capture_range(&members);

    let mut meta_rows = String::new();
    if let Some(cur) = &c.curator {
        meta_rows.push_str(&format!("<tr><th>Curator</th><td>{}</td></tr>", html_escape(cur)));
    }
    meta_rows.push_str(&format!("<tr><th>WACZs</th><td>{}</td></tr>", members.len()));
    meta_rows.push_str(&format!("<tr><th>Size</th><td>{}</td></tr>", human_size(total_size)));
    if !software.is_empty() {
        meta_rows.push_str(&format!("<tr><th>Software</th><td>{}</td></tr>", html_escape(&software.join(", "))));
    }
    if let Some(r) = &range {
        meta_rows.push_str(&format!("<tr><th>Capture dates</th><td>{}</td></tr>", html_escape(r)));
    }
    let created = c.created.get(..10).unwrap_or(&c.created);
    meta_rows.push_str(&format!("<tr><th>Created</th><td>{}</td></tr>", html_escape(created)));

    let items: String = members
        .iter()
        .map(|w| {
            let status = if w.is_present(&state.home) {
                "<span class=\"ok\">✓</span>"
            } else {
                "<span class=\"missing\">✗</span>"
            };
            format!(
                "<li><a href=\"/wacz/{}\">{}</a> {status}{}</li>",
                w.id,
                html_escape(&w.name),
                provenance_line(w),
            )
        })
        .collect();
    let members_section = if items.is_empty() {
        "<p class=\"muted\">No WACZs in this collection.</p>".to_string()
    } else {
        format!("<ul class=\"pages\">{items}</ul>")
    };

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>{name} - rustyweb</title>
  <style>
    * {{ box-sizing: border-box; }}
    body {{ font-family: sans-serif; max-width: 900px; margin: 2rem auto; padding: 0 1rem; color: #222; }}
    .top {{ display: flex; align-items: center; gap: 1rem; margin-bottom: 1.5rem; }}
    .top a.home {{ font-size: 1.4rem; font-weight: bold; text-decoration: none; color: #222; }}
    .search-form {{ display: flex; gap: 0.5rem; flex: 1; }}
    .search-form input {{ flex: 1; padding: 0.5rem 0.8rem; font-size: 1rem; border: 1px solid #ccc; border-radius: 4px; }}
    .search-form button {{ padding: 0.5rem 1rem; font-size: 1rem; cursor: pointer; background: #0066cc; color: #fff; border: none; border-radius: 4px; }}
    h1 {{ font-size: 1.6rem; margin: 0.3rem 0; }}
    .desc {{ color: #444; margin: 0.5rem 0 1rem; }}
    table.meta {{ border-collapse: collapse; font-size: 0.9rem; margin-bottom: 2rem; }}
    table.meta th {{ text-align: left; padding: 0.3rem 1rem 0.3rem 0; color: #666; font-weight: 600; vertical-align: top; white-space: nowrap; }}
    table.meta td {{ padding: 0.3rem 0; }}
    h2 {{ font-size: 1.1rem; border-bottom: 1px solid #eee; padding-bottom: 0.4rem; }}
    ul.pages {{ list-style: none; padding: 0; }}
    ul.pages li {{ padding: 0.5rem 0; border-bottom: 1px solid #f0f0f0; }}
    ul.pages a {{ color: #1a0dab; font-size: 1.02rem; text-decoration: none; }}
    ul.pages a:hover {{ text-decoration: underline; }}
    .prov {{ font-size: 0.82rem; color: #1a4d7a; background: #eef4fb; border: 1px solid #d6e4f2; border-radius: 4px; padding: 0.2rem 0.5rem; margin: 0.25rem 0 0; display: inline-block; }}
    .ok {{ color: #2a7; }} .missing {{ color: #c33; }} .muted {{ color: #888; }}
    a {{ color: #0066cc; }}
  </style>
</head>
<body>
  <div class="top">
    <a class="home" href="/">rustyweb</a>
    <form class="search-form" action="/search" method="get">
      <input type="search" name="q" placeholder="Search all collections…">
      <button type="submit">Search</button>
    </form>
  </div>

  <h1>{name}</h1>
  {description}
  <table class="meta">{meta_rows}</table>

  <h2>WACZs</h2>
  {members_section}
</body>
</html>"#
    );

    (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], html).into_response()
}

async fn wacz_page(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let manifest = match Manifest::open(&state.index_dir) {
        Ok(m) => m,
        Err(e) => return error_response(e).into_response(),
    };
    let Some(c) = manifest.wacz_by_id(&id) else {
        return (StatusCode::NOT_FOUND, "WACZ not found").into_response();
    };

    // Breadcrumb back to the containing collection.
    let crumb = manifest
        .collection_by_id(&c.collection)
        .map(|col| {
            format!(
                "<div class=\"crumb\">in <a href=\"/collection/{}\">{}</a></div>",
                col.id,
                html_escape(&col.name)
            )
        })
        .unwrap_or_default();

    let name = html_escape(&c.name);
    let source_enc = url_encode(&viewer_source(c));
    let name_enc = url_encode(&c.name);
    let source_disp = html_escape(&c.source.location());
    let status = if c.is_present(&state.home) {
        "<span class=\"ok\">✓ present</span>"
    } else {
        "<span class=\"missing\">✗ missing</span>"
    };
    let description = c
        .description
        .as_deref()
        .map(|d| format!("<p class=\"desc\">{}</p>", html_escape(d)))
        .unwrap_or_default();
    let crawl_row = c
        .crawl_date
        .as_deref()
        .map(|d| format!("<tr><th>Crawled</th><td>{}</td></tr>", html_escape(d.get(..10).unwrap_or(d))))
        .unwrap_or_default();
    let indexed = c.date_indexed.get(..10).unwrap_or(&c.date_indexed);
    let size = human_size(c.file_size);
    let sha_short = c.sha256.get(..16).unwrap_or(&c.sha256);

    // Replay button: first seed page, else the collection root.
    let replay_href = match c.seed_pages.first() {
        Some(p) => format!(
            "/replay/viewer?source={source_enc}&url={}&ts={}&name={name_enc}",
            url_encode(&p.url),
            ts_to_14digit(&p.ts),
        ),
        None => format!("/replay/viewer?source={source_enc}&name={name_enc}"),
    };

    let pages: String = c
        .seed_pages
        .iter()
        .map(|p| {
            let title = p.title.as_deref().unwrap_or(&p.url);
            let href = format!(
                "/replay/viewer?source={source_enc}&url={}&ts={}&name={name_enc}",
                url_encode(&p.url),
                ts_to_14digit(&p.ts),
            );
            format!(
                "<li><a href=\"{href}\">{}</a><div class=\"result-url\">{}</div></li>",
                html_escape(title),
                html_escape(&p.url),
            )
        })
        .collect();
    let pages_section = if pages.is_empty() {
        "<p class=\"muted\">No pages are listed in this WACZ.</p>".to_string()
    } else {
        format!("<ul class=\"pages\">{pages}</ul>")
    };

    // Provenance panel: how this WACZ was produced. Only rows with data show.
    let prov_rows = {
        let mut rows = String::new();
        let mut row = |label: &str, value: &str, mono: bool| {
            let class = if mono { " class=\"mono\"" } else { "" };
            rows.push_str(&format!("<tr><th>{label}</th><td{class}>{}</td></tr>", html_escape(value)));
        };
        if !c.software.is_empty() {
            row("Software", &c.software.join(", "), false);
        }
        if let Some(op) = &c.operator {
            row("Operator", op, false);
        }
        if let Some(ua) = &c.user_agent {
            row("User-Agent", ua, true);
        }
        if let Some(rb) = &c.robots {
            row("Robots", rb, false);
        }
        if let Some(n) = c.page_count {
            row("Pages", &n.to_string(), false);
        }
        if let Some(range) = capture_range(c) {
            row("Capture dates", &range, false);
        }
        rows
    };
    let provenance_section = if prov_rows.is_empty() {
        String::new()
    } else {
        format!("<h2>Provenance</h2>\n  <table class=\"meta\">{prov_rows}</table>")
    };

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>{name} - rustyweb</title>
  <style>
    * {{ box-sizing: border-box; }}
    body {{ font-family: sans-serif; max-width: 900px; margin: 2rem auto; padding: 0 1rem; color: #222; }}
    .top {{ display: flex; align-items: center; gap: 1rem; margin-bottom: 1.5rem; }}
    .top a.home {{ font-size: 1.4rem; font-weight: bold; text-decoration: none; color: #222; }}
    .search-form {{ display: flex; gap: 0.5rem; flex: 1; }}
    .search-form input {{ flex: 1; padding: 0.5rem 0.8rem; font-size: 1rem; border: 1px solid #ccc; border-radius: 4px; }}
    .search-form button {{ padding: 0.5rem 1rem; font-size: 1rem; cursor: pointer; background: #0066cc; color: #fff; border: none; border-radius: 4px; }}
    h1 {{ font-size: 1.6rem; margin: 0.3rem 0; }}
    .crumb {{ font-size: 0.85rem; color: #666; margin-bottom: 0.3rem; }}
    .desc {{ color: #444; margin: 0.5rem 0 1rem; }}
    .replay-btn {{ display: inline-block; padding: 0.5rem 1rem; background: #0066cc; color: #fff; border-radius: 4px; text-decoration: none; margin-bottom: 1.5rem; }}
    .replay-btn:hover {{ background: #0052a3; }}
    table.meta {{ border-collapse: collapse; font-size: 0.9rem; margin-bottom: 2rem; }}
    table.meta th {{ text-align: left; padding: 0.3rem 1rem 0.3rem 0; color: #666; font-weight: 600; vertical-align: top; white-space: nowrap; }}
    table.meta td {{ padding: 0.3rem 0; }}
    .mono {{ font-family: monospace; font-size: 0.85rem; word-break: break-all; }}
    h2 {{ font-size: 1.1rem; border-bottom: 1px solid #eee; padding-bottom: 0.4rem; }}
    ul.pages {{ list-style: none; padding: 0; }}
    ul.pages li {{ padding: 0.5rem 0; border-bottom: 1px solid #f0f0f0; }}
    ul.pages a {{ color: #1a0dab; font-size: 1.02rem; text-decoration: none; }}
    ul.pages a:hover {{ text-decoration: underline; }}
    .result-url {{ font-size: 0.8rem; color: #006621; margin-top: 0.1rem; }}
    .ok {{ color: #2a7; }} .missing {{ color: #c33; }} .muted {{ color: #888; }}
    a {{ color: #0066cc; }}
  </style>
</head>
<body>
  <div class="top">
    <a class="home" href="/">rustyweb</a>
    <form class="search-form" action="/search" method="get">
      <input type="search" name="q" placeholder="Search all collections…">
      <button type="submit">Search</button>
    </form>
  </div>

  {crumb}
  <h1>{name}</h1>
  {description}
  <a class="replay-btn" href="{replay_href}">Replay →</a>

  {provenance_section}
  <h2>File</h2>
  <table class="meta">
    <tr><th>Source</th><td class="mono">{source_disp}</td></tr>
    <tr><th>Size</th><td>{size}</td></tr>
    <tr><th>SHA-256</th><td class="mono" title="{sha256}">{sha_short}…</td></tr>
    {crawl_row}
    <tr><th>Indexed</th><td>{indexed}</td></tr>
    <tr><th>Status</th><td>{status}</td></tr>
  </table>

  <h2>Pages</h2>
  {pages_section}
</body>
</html>"#,
        sha256 = html_escape(&c.sha256),
    );

    (StatusCode::OK, [("content-type", "text/html; charset=utf-8")], html).into_response()
}

/// Format a byte count as a short human-readable size.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < UNITS.len() - 1 {
        b /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{b:.1} {}", UNITS[i])
    }
}

// ── File serving ──────────────────────────────────────────────────────────────

async fn serve_file(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let collections = load_waczs(&state);
    let Some(col) = collections.iter().find(|c| c.id == id) else {
        return (StatusCode::NOT_FOUND, "collection not found").into_response();
    };

    // Remote sources aren't proxied: wabac.js reads them directly. If /files/{id}
    // is hit for a remote source anyway, redirect to the URL as a convenience.
    if let crate::collections::Source::Url(u) = &col.source {
        return axum::response::Redirect::temporary(u).into_response();
    }
    // File source: resolve relative paths against home.
    let path = col.source.resolve(&state.home).unwrap();
    if !path.exists() {
        return (StatusCode::NOT_FOUND, "archive file not found on disk").into_response();
    }

    let file_size = col.file_size;
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_byte_range(s, file_size));

    match tokio::fs::File::open(path).await {
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
                    "domain": r.domain,
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

fn load_waczs(state: &AppState) -> Vec<Wacz> {
    Manifest::open(&state.index_dir)
        .map(|m| m.waczs)
        .unwrap_or_default()
}

/// The `source` value to hand wabac.js for a collection: our local byte-range
/// endpoint for a file, or the remote URL directly (read client-side) for a URL.
fn viewer_source(col: &Wacz) -> String {
    match &col.source {
        crate::collections::Source::File(_) => format!("/files/{}", col.id),
        crate::collections::Source::Url(u) => u.clone(),
    }
}

fn error_response(e: anyhow::Error) -> Response {
    tracing::error!("{e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

/// A collapsible "Search tips" panel documenting the query syntax the search
/// box actually supports. Rendered as a `<details>` element (no JavaScript) so
/// it can sit unobtrusively on the homepage and the results page. The examples
/// here must stay in sync with how `SearchIndex::search` configures the query
/// parser (AND-by-default, title/body/URL default fields, `domain:` filtering).
fn search_tips_html() -> &'static str {
    r#"<details class="tips">
  <summary>Search tips</summary>
  <div class="tips-body">
    <p>Type words to search page titles, headings, page text, descriptions, and
    URLs. <strong>All words must match</strong> - <code>climate policy</code>
    finds pages containing both.</p>
    <ul>
      <li><code>"climate policy"</code> - an exact phrase (use quotes)</li>
      <li><code>climate OR weather</code> - either word</li>
      <li><code>climate -policy</code> - has "climate", excludes "policy"</li>
      <li><code>(climate OR weather) risk</code> - group with parentheses</li>
      <li><code>title:climate</code> - match only in the page title</li>
      <li><code>domain:example.com</code> - only pages from that exact host</li>
      <li><code>year:2021</code> or <code>year:[2020 TO 2023]</code> - filter by crawl year</li>
      <li><code>type:pdf</code> - only PDFs (or <code>type:html</code>)</li>
      <li><code>lang:en</code> - only pages in that language</li>
      <li><code>climate^2 change</code> - rank "climate" matches higher</li>
    </ul>
    <p class="tips-note">Searches are case-insensitive. Title matches rank
    above body matches. <code>domain:</code> needs the exact host (e.g.
    <code>www.example.com</code>); to match host words loosely, just type them
    (e.g. <code>example</code>).</p>
  </div>
</details>"#
}

/// `YYYY-MM-DD` from the first 8 digits of a 14-digit timestamp; the input as-is
/// if it is too short.
fn ymd(ts: &str) -> String {
    if ts.len() >= 8 && ts[..8].bytes().all(|b| b.is_ascii_digit()) {
        format!("{}-{}-{}", &ts[0..4], &ts[4..6], &ts[6..8])
    } else {
        ts.to_string()
    }
}

/// The capture date range of a collection as a display string (`start → end`, or
/// a single date when they coincide), or `None` when no range was recorded.
fn capture_range(c: &Wacz) -> Option<String> {
    match (c.capture_start.as_deref(), c.capture_end.as_deref()) {
        (Some(s), Some(e)) => {
            let (sd, ed) = (ymd(s), ymd(e));
            Some(if sd == ed { sd } else { format!("{sd} → {ed}") })
        }
        (Some(s), None) => Some(ymd(s)),
        (None, Some(e)) => Some(ymd(e)),
        (None, None) => None,
    }
}

/// The deduped union of software across a collection's member WACZs.
fn collection_software(members: &[&Wacz]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in members {
        for s in &w.software {
            if !out.contains(s) {
                out.push(s.clone());
            }
        }
    }
    out
}

/// The capture date range spanning a collection's member WACZs.
fn members_capture_range(members: &[&Wacz]) -> Option<String> {
    let start = members.iter().filter_map(|w| w.capture_start.clone()).min();
    let end = members.iter().filter_map(|w| w.capture_end.clone()).max();
    match (start, end) {
        (Some(s), Some(e)) => {
            let (sd, ed) = (ymd(&s), ymd(&e));
            Some(if sd == ed { sd } else { format!("{sd} → {ed}") })
        }
        (Some(s), None) => Some(ymd(&s)),
        (None, Some(e)) => Some(ymd(&e)),
        (None, None) => None,
    }
}

/// A compact one-line provenance summary (`Captured with X · N pages · dates`)
/// for homepage cards and search results. Empty when nothing is known.
fn provenance_line(c: &Wacz) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !c.software.is_empty() {
        parts.push(format!("Software: {}", html_escape(&c.software.join(", "))));
    }
    if let Some(n) = c.page_count {
        parts.push(format!("{n} page{}", if n == 1 { "" } else { "s" }));
    }
    if let Some(range) = capture_range(c) {
        parts.push(html_escape(&range));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("<div class=\"prov\">{}</div>", parts.join(" · "))
    }
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
