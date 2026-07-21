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
use crate::views;

// ── Embedded static assets ────────────────────────────────────────────────────

#[derive(RustEmbed)]
#[folder = "static/replay"]
struct ReplayAssets;

/// Site static assets (the shared stylesheet, etc.), served at `/assets/*`.
#[derive(RustEmbed)]
#[folder = "static/assets"]
struct SiteAssets;

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
        .route("/crawl/{id}", get(crawl_page))
        .route("/thumb/{id}", get(thumb_handler))
        .route("/files/{id}", get(serve_file))
        .route("/replay/viewer", get(replay_viewer))
        .route("/api/search", get(search_api))
        .route("/assets/{*path}", get(asset_handler))
        .route("/replay/", get(replay_index))
        .route("/replay/{*path}", get(replay_handler))
        .layer(CompressionLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<Body>| {
                    let ip = req
                        .extensions()
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
                .on_response(
                    |res: &Response, latency: std::time::Duration, _span: &tracing::Span| {
                        let ct = res
                            .headers()
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
                    },
                ),
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

    let cards: Vec<views::CollectionCard> = manifest
        .collections
        .iter()
        .map(|c| {
            let members: Vec<&Wacz> = manifest.members_of(&c.id).collect();
            views::CollectionCard {
                id: c.id.clone(),
                name: c.name.clone(),
                count: members.len(),
                description: c.description.clone(),
                // Capture date range (temporal span is meaningful at the
                // collection level; per-tool software lives on the WACZ page).
                date_range: members_capture_range(&members),
                // Representative image: the first member crawl that has one.
                thumb: members
                    .iter()
                    .find_map(|w| thumb_href(&state.index_dir, &w.id)),
            }
        })
        .collect();

    // Browse entry points: years (most recent first) and the busiest sites,
    // each a search link. Derived from an archive-wide facet overview.
    let overview = state.search.facet_overview().unwrap_or_default();
    let browse = views::HomeBrowse {
        years: browse_links(&overview, "year", "year", 12, true),
        sites: browse_links(&overview, "site", "site", 8, false),
    };

    views::home(&cards, &browse).into_response()
}

/// Build homepage browse links from one facet dimension: `field` is the facet
/// group to read, `query_field` the `field:value` used in the search link.
/// `by_value_desc` sorts by the value (e.g. year, newest first) instead of by
/// count; `max` caps how many are shown.
fn browse_links(
    overview: &[crate::search::FacetGroup],
    field: &str,
    query_field: &str,
    max: usize,
    by_value_desc: bool,
) -> Vec<views::BrowseLink> {
    let Some(group) = overview.iter().find(|g| g.field == field) else {
        return Vec::new();
    };
    let mut buckets: Vec<&crate::search::FacetBucket> = group.buckets.iter().collect();
    if by_value_desc {
        buckets.sort_by(|a, b| b.value.cmp(&a.value));
    }
    buckets
        .into_iter()
        .take(max)
        .map(|b| views::BrowseLink {
            label: b.value.clone(),
            count: b.count,
            href: format!(
                "/search?q={}",
                url_encode(&format!("{query_field}:{}", b.value))
            ),
        })
        .collect()
}

// ── Search results page ───────────────────────────────────────────────────────

/// Search results per page.
const PAGE_SIZE: usize = 20;

/// Format a `YYYYMM` month as `YYYY-MM` for display.
fn format_ym(ym: u64) -> String {
    format!("{:04}-{:02}", ym / 100, ym % 100)
}

/// The active `field:value` facet filters present in a query, in order. Only
/// single-token filters are recognized: a range like `month:[202101 TO 202106]`
/// is a valid query but splits into several whitespace tokens, so it does not
/// appear as a removable chip. Filter fields come from `search::is_filter_field`
/// so this stays in sync with the facet dimensions.
fn active_filters(q: &str) -> Vec<(String, String)> {
    q.split_whitespace()
        .filter_map(|tok| {
            let (f, v) = tok.split_once(':')?;
            (crate::search::is_filter_field(f) && !v.is_empty())
                .then(|| (f.to_string(), v.to_string()))
        })
        .collect()
}

/// Add a `field:value` filter to a query, leaving the rest (including quoted
/// phrases) untouched. A no-op if that exact filter is already present.
fn query_with_filter(q: &str, field: &str, value: &str) -> String {
    let token = format!("{field}:{value}");
    let base = q.trim();
    if base.split_whitespace().any(|t| t == token) {
        return base.to_string();
    }
    if base.is_empty() {
        token
    } else {
        format!("{base} {token}")
    }
}

/// Remove a `field:value` filter from a query (all occurrences of that token).
fn query_without_filter(q: &str, field: &str, value: &str) -> String {
    let token = format!("{field}:{value}");
    q.split_whitespace()
        .filter(|t| *t != token)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Deserialize)]
struct SearchPageParams {
    q: String,
    /// 1-based page number; absent/`<1` means the first page.
    page: Option<usize>,
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

    let page = params.page.unwrap_or(1).max(1);
    let offset = (page - 1) * PAGE_SIZE;
    let response = match state.search.search_faceted(&q, PAGE_SIZE, offset) {
        Ok(r) => r,
        Err(e) => return error_response(e).into_response(),
    };
    let results = &response.results;

    // Map each WACZ id to the wabac `source` to use: /files/{id} for a local
    // WACZ, or the remote URL directly for an http source.
    let waczs = load_waczs(&state);
    let source_for = |wacz_id: &str| -> String {
        waczs
            .iter()
            .find(|w| w.id == wacz_id)
            .map(viewer_source)
            .unwrap_or_else(|| format!("/files/{wacz_id}"))
    };
    // Curated collection id -> display name, for the "in <collection>" link.
    let collection_names: std::collections::HashMap<String, String> =
        Manifest::open(&state.index_dir)
            .map(|m| {
                m.collections
                    .iter()
                    .map(|c| (c.id.clone(), c.name.clone()))
                    .collect()
            })
            .unwrap_or_default();

    let rows: Vec<views::SearchResultRow> = results
        .iter()
        .map(|r| {
            let is_collection = r.doc_type == "collection";
            let title = if r.title.is_empty() {
                if is_collection {
                    r.crawl_name.clone()
                } else {
                    r.url.clone()
                }
            } else {
                r.title.clone()
            };

            // The curated collection this result belongs to (falls back to the
            // slug/id if the name isn't found).
            let coll_display = collection_names
                .get(&r.collection)
                .map(String::as_str)
                .unwrap_or(&r.collection)
                .to_string();
            let coll_href = url_encode(&r.collection);
            let name_enc = url_encode(&r.crawl_name);
            let source_enc = url_encode(&source_for(&r.crawl_id));
            // Carry the collection breadcrumb (name + id) into the replay viewer.
            let coll_q = format!(
                "&collection={}&collection_id={coll_href}",
                url_encode(&coll_display)
            );

            let href = if is_collection {
                // Link to the collection's root in the viewer.
                format!("/replay/viewer?source={source_enc}&name={name_enc}{coll_q}")
            } else {
                format!(
                    "/replay/viewer?source={source_enc}&url={}&ts={}&name={name_enc}{coll_q}",
                    url_encode(&r.url),
                    r.timestamp
                )
            };

            // Prefer the hit-highlighted body snippet; if the query didn't match
            // the body (e.g. a title-only or URL-only hit), fall back to the
            // page's description so the result still has context. The snippet is
            // already-safe HTML (Tantivy emits `<b>` tags); the description is
            // plain text, so escape it before splicing as pre-escaped HTML.
            let snippet_html = if !r.snippet.is_empty() {
                Some(r.snippet.clone())
            } else if !r.description.is_empty() {
                Some(html_escape(&r.description))
            } else {
                None
            };

            let timestamp_display = if !is_collection && !r.timestamp.is_empty() {
                format_timestamp(&r.timestamp)
            } else {
                String::new()
            };

            views::SearchResultRow {
                href,
                title,
                is_collection,
                url: r.url.clone(),
                timestamp_display,
                snippet_html,
                coll_href,
                coll_display,
                capture_count: r.capture_count,
            }
        })
        .collect();

    let total_pages = response.total_hits.div_ceil(PAGE_SIZE).max(1);
    let page_nav = views::PageNav {
        page,
        total_pages,
        total_hits: response.total_hits,
        capped: response.capped,
        query_encoded: url_encode(&q),
    };

    // Facet sidebar: clickable buckets that add/remove a `field:value` filter,
    // plus chips for the filters already active in the query. Refining resets
    // to page 1.
    let filters = active_filters(&q);
    let search_href = |new_q: &str| format!("/search?q={}", url_encode(new_q));
    // The `crawl:` filter's value is an opaque WACZ id (from a crawl-page facet
    // link); show the crawl's name in the chip instead. Other filters show their
    // value as-is. The removal token still uses the raw id.
    let manifest = Manifest::open(&state.index_dir).ok();
    let active: Vec<views::ActiveFilter> = filters
        .iter()
        .map(|(f, v)| {
            let display = if f == "crawl" {
                manifest
                    .as_ref()
                    .and_then(|m| m.wacz_by_id(v))
                    .map(|w| w.name.clone())
                    .unwrap_or_else(|| v.clone())
            } else {
                v.clone()
            };
            views::ActiveFilter {
                label: crate::search::filter_label(f).to_string(),
                value: display,
                remove_href: search_href(&query_without_filter(&q, f, v)),
            }
        })
        .collect();
    let groups: Vec<views::FacetGroupView> = response
        .facets
        .iter()
        .map(|g| views::FacetGroupView {
            label: g.label.clone(),
            items: g
                .buckets
                .iter()
                .map(|b| {
                    let is_active = filters.iter().any(|(f, v)| f == &g.field && v == &b.value);
                    let new_q = if is_active {
                        query_without_filter(&q, &g.field, &b.value)
                    } else {
                        query_with_filter(&q, &g.field, &b.value)
                    };
                    views::FacetItem {
                        value: b.value.clone(),
                        count: b.count,
                        href: search_href(&new_q),
                        active: is_active,
                    }
                })
                .collect(),
        })
        .collect();
    let sidebar = views::FacetSidebar { active, groups };

    // Timeline: one clickable bar per crawl month, oldest first, height scaled
    // to the busiest month. Clicking toggles a `month:YYYYMM` filter.
    let max_count = response
        .timeline
        .iter()
        .map(|t| t.count)
        .max()
        .unwrap_or(1)
        .max(1);
    let timeline: Vec<views::TimelineBar> = response
        .timeline
        .iter()
        .map(|t| {
            let month = t.ym.to_string();
            let is_active = filters.iter().any(|(f, v)| f == "month" && v == &month);
            let new_q = if is_active {
                query_without_filter(&q, "month", &month)
            } else {
                query_with_filter(&q, "month", &month)
            };
            views::TimelineBar {
                label: format_ym(t.ym),
                count: t.count,
                pct: (t.count as f64 / max_count as f64 * 100.0).round() as u32,
                href: search_href(&new_q),
                active: is_active,
            }
        })
        .collect();

    views::search_results(&q, &page_nav, &sidebar, &timeline, &rows).into_response()
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

    // Aggregates derived from members.
    let total_size: u64 = members.iter().map(|w| w.file_size).sum();
    let software = collection_software(&members);
    let range = members_capture_range(&members);

    let mut meta = Vec::new();
    if let Some(cur) = &c.curator {
        meta.push(views::MetaRow::new("Curator", cur.clone()));
    }
    meta.push(views::MetaRow::new("Crawls", members.len().to_string()));
    meta.push(views::MetaRow::new("Size", human_size(total_size)));
    if !software.is_empty() {
        meta.push(views::MetaRow::new("Software", software.join(", ")));
    }
    if let Some(r) = &range {
        meta.push(views::MetaRow::new("Capture dates", r.clone()));
    }
    let created = c.created.get(..10).unwrap_or(&c.created);
    meta.push(views::MetaRow::new("Created", created));

    let member_items: Vec<views::MemberItem> = members
        .iter()
        .map(|w| views::MemberItem {
            id: w.id.clone(),
            name: w.name.clone(),
            present: w.is_present(&state.home),
            provenance: provenance_summary(w),
            thumb: thumb_href(&state.index_dir, &w.id),
        })
        .collect();

    // Scoped facet overview: what's *in* this collection, each value a search
    // scoped to it. Turns the page into a faceted entry point, not just a list.
    let overview = state
        .search
        .facet_overview_scoped(crate::search::FacetScope::Collection(&id))
        .unwrap_or_default();
    let facets = scoped_facet_sections(&overview, &format!("collection:{id}"));

    views::collection(
        &c.name,
        c.description.as_deref(),
        &meta,
        &facets,
        &member_items,
    )
    .into_response()
}

/// Build the scoped facet sections for a detail page. Each dimension becomes a
/// labeled group whose links run a search within `scope` (e.g. `collection:slug`)
/// further filtered by that value. The Collection dimension is skipped (moot on a
/// scoped page), and empty dimensions are dropped.
fn scoped_facet_sections(
    overview: &[crate::search::FacetGroup],
    scope: &str,
) -> Vec<views::FacetSection> {
    // (facet field == filter field, heading, sort by value desc, max shown)
    const DIMS: [(&str, &str, bool, usize); 4] = [
        ("site", "Top sites", false, 10),
        ("year", "By year", true, 12),
        ("type", "Types", false, 6),
        ("lang", "Languages", false, 8),
    ];
    DIMS.iter()
        .filter_map(|(field, label, by_value_desc, max)| {
            let group = overview.iter().find(|g| g.field == *field)?;
            let mut buckets: Vec<&crate::search::FacetBucket> = group.buckets.iter().collect();
            if buckets.is_empty() {
                return None;
            }
            if *by_value_desc {
                buckets.sort_by(|a, b| b.value.cmp(&a.value));
            }
            let links = buckets
                .into_iter()
                .take(*max)
                .map(|b| views::BrowseLink {
                    label: b.value.clone(),
                    count: b.count,
                    href: format!(
                        "/search?q={}",
                        url_encode(&format!("{scope} {field}:{}", b.value))
                    ),
                })
                .collect();
            Some(views::FacetSection {
                label: label.to_string(),
                links,
            })
        })
        .collect()
}

async fn crawl_page(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let manifest = match Manifest::open(&state.index_dir) {
        Ok(m) => m,
        Err(e) => return error_response(e).into_response(),
    };
    let Some(c) = manifest.wacz_by_id(&id) else {
        return (StatusCode::NOT_FOUND, "Crawl not found").into_response();
    };

    let source_enc = url_encode(&viewer_source(c));
    let name_enc = url_encode(&c.name);
    // Breadcrumb + replay params for the containing collection (name + id).
    let col = manifest.collection_by_id(&c.collection);
    let crumb = col.map(|col| (col.id.clone(), col.name.clone()));
    let coll_q = col
        .map(|col| {
            format!(
                "&collection={}&collection_id={}",
                url_encode(&col.name),
                url_encode(&col.id)
            )
        })
        .unwrap_or_default();

    // Replay button: first seed page, else the collection root.
    let replay_href = match c.seed_pages.first() {
        Some(p) => format!(
            "/replay/viewer?source={source_enc}&url={}&ts={}&name={name_enc}{coll_q}",
            url_encode(&p.url),
            ts_to_14digit(&p.ts),
        ),
        None => format!("/replay/viewer?source={source_enc}&name={name_enc}{coll_q}"),
    };

    let pages: Vec<views::PageItem> = c
        .seed_pages
        .iter()
        .map(|p| views::PageItem {
            href: format!(
                "/replay/viewer?source={source_enc}&url={}&ts={}&name={name_enc}{coll_q}",
                url_encode(&p.url),
                ts_to_14digit(&p.ts),
            ),
            title: p.title.clone().unwrap_or_else(|| p.url.clone()),
            url: p.url.clone(),
        })
        .collect();

    // Provenance panel: how this crawl was produced. Only rows with data show.
    let mut provenance = Vec::new();
    if let Some(bt) = &c.browsertrix {
        // Attribution for content pulled in via `rustyweb import browsertrix`.
        let host = bt
            .host
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        provenance.push(views::MetaRow::new(
            "Source",
            format!("Browsertrix ({host})"),
        ));
        if !bt.item_id.is_empty() {
            provenance.push(views::MetaRow::mono("Browsertrix item", bt.item_id.clone()));
        }
    }
    if !c.software.is_empty() {
        provenance.push(views::MetaRow::new("Software", c.software.join(", ")));
    }
    if let Some(op) = &c.operator {
        provenance.push(views::MetaRow::new("Operator", op.clone()));
    }
    if let Some(ua) = &c.user_agent {
        provenance.push(views::MetaRow::mono("User-Agent", ua.clone()));
    }
    if let Some(rb) = &c.robots {
        provenance.push(views::MetaRow::new("Robots", rb.clone()));
    }
    if let Some(n) = c.nested_waczs {
        provenance.push(views::MetaRow::new(
            "Multi-WACZ",
            format!(
                "{n} crawl{} bundled in one file",
                if n == 1 { "" } else { "s" }
            ),
        ));
    }
    if let Some(n) = c.page_count {
        provenance.push(views::MetaRow::new("Pages", n.to_string()));
    }
    if let Some(range) = capture_range(c) {
        provenance.push(views::MetaRow::new("Capture dates", range));
    }

    let page = views::CrawlPage {
        crumb,
        name: c.name.clone(),
        description: c.description.clone(),
        thumb: thumb_href(&state.index_dir, &id),
        replay_href,
        provenance,
        source: c.source.location(),
        size: human_size(c.file_size),
        sha_short: c.sha256.get(..16).unwrap_or(&c.sha256).to_string(),
        sha_full: c.sha256.clone(),
        crawled: c
            .crawl_date
            .as_deref()
            .map(|d| d.get(..10).unwrap_or(d).to_string()),
        indexed: c
            .date_indexed
            .get(..10)
            .unwrap_or(&c.date_indexed)
            .to_string(),
        present: c.is_present(&state.home),
        facets: scoped_facet_sections(
            &state
                .search
                .facet_overview_scoped(crate::search::FacetScope::Crawl(&id))
                .unwrap_or_default(),
            &format!("crawl:{id}"),
        ),
        pages,
    };

    views::crawl(&page).into_response()
}

/// Format a byte count as a short human-readable size.
/// Format a byte count for display (e.g. `48.2 MB`). Shared by the web UI and
/// the CLI so both show sizes the same way.
pub fn human_size(bytes: u64) -> String {
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
    match state.search.search_faceted(&params.q, limit, 0) {
        Ok(response) => {
            let body = serde_json::json!({
                "total": response.total_hits,
                "capped": response.capped,
                "results": response.results.iter().map(|r| serde_json::json!({
                    "doc_type": r.doc_type,
                    "url": r.url,
                    "domain": r.domain,
                    "timestamp": r.timestamp,
                    "title": r.title,
                    "crawl_id": r.crawl_id,
                    "crawl_name": r.crawl_name,
                    "collection": r.collection,
                    "snippet": r.snippet,
                    "capture_count": r.capture_count,
                })).collect::<Vec<_>>(),
                "facets": response.facets.iter().map(|g| serde_json::json!({
                    "field": g.field,
                    "label": g.label,
                    "buckets": g.buckets.iter().map(|b| serde_json::json!({
                        "value": b.value,
                        "count": b.count,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
        Err(e) => error_response(e),
    }
}

// ── ReplayWebPage static assets ───────────────────────────────────────────────

async fn replay_viewer(headers: HeaderMap) -> impl IntoResponse {
    serve_embedded_asset(ReplayAssets::get("viewer.html"), "viewer.html", &headers)
}

async fn replay_index() -> impl IntoResponse {
    (StatusCode::SEE_OTHER, [("location", "/")]).into_response()
}

async fn replay_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    serve_embedded_asset(ReplayAssets::get(&path), &path, &headers)
}

/// Serve a site static asset (CSS, etc.) embedded from `static/assets`.
async fn asset_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    serve_embedded_asset(SiteAssets::get(&path), &path, &headers)
}

/// The path to a crawl's cached thumbnail, if one was generated at index time.
fn thumb_path(index_dir: &Path, crawl_id: &str) -> PathBuf {
    index_dir.join("thumbs").join(format!("{crawl_id}.jpg"))
}

/// The `/thumb/{id}` href for a crawl, or `None` if it has no cached thumbnail
/// (the UI then shows a CSS placeholder). `id` is a crawl id.
fn thumb_href(index_dir: &Path, crawl_id: &str) -> Option<String> {
    thumb_path(index_dir, crawl_id)
        .exists()
        .then(|| format!("/thumb/{crawl_id}"))
}

/// Serve a crawl's cached representative thumbnail (a small JPEG under
/// `<home>/index/thumbs`). 404 when the crawl has none.
async fn thumb_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    // Crawl ids are hex hashes; reject anything else so the id can't escape the
    // thumbs directory.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match std::fs::read(thumb_path(&state.index_dir, &id)) {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serve an embedded ReplayWebPage asset with an ETag derived from its content
/// hash and `Cache-Control: no-cache`. Browsers must revalidate on every load,
/// so a rebuild that changes an asset (e.g. `viewer.html`, `sw.js`) propagates
/// to clients on their next request instead of being masked by the HTTP cache.
/// When the client's `If-None-Match` matches, we return `304` with no body so
/// unchanged assets aren't re-downloaded.
fn serve_embedded_asset(
    content: Option<rust_embed::EmbeddedFile>,
    path: &str,
    req_headers: &HeaderMap,
) -> Response {
    match content {
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
            Some(if sd == ed {
                sd
            } else {
                format!("{sd} → {ed}")
            })
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
            Some(if sd == ed {
                sd
            } else {
                format!("{sd} → {ed}")
            })
        }
        (Some(s), None) => Some(ymd(&s)),
        (None, Some(e)) => Some(ymd(&e)),
        (None, None) => None,
    }
}

/// A compact one-line provenance summary (`Software: X · N pages · dates`) as
/// plain text for collection member listings. `None` when nothing is known.
/// The view wraps it in a `.prov` element and handles escaping.
fn provenance_summary(c: &Wacz) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !c.software.is_empty() {
        parts.push(format!("Software: {}", c.software.join(", ")));
    }
    if let Some(n) = c.page_count {
        parts.push(format!("{n} page{}", if n == 1 { "" } else { "s" }));
    }
    if let Some(range) = capture_range(c) {
        parts.push(range);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_filters_extracts_facet_tokens_only() {
        // Free text and non-facet `field:` tokens are ignored.
        let f = active_filters("climate type:pdf domain:example.com title:foo");
        assert_eq!(
            f,
            vec![
                ("type".to_string(), "pdf".to_string()),
                ("domain".to_string(), "example.com".to_string()),
            ]
        );
        assert!(active_filters("just some words").is_empty());
    }

    #[test]
    fn query_with_filter_appends_once() {
        assert_eq!(
            query_with_filter("climate", "type", "pdf"),
            "climate type:pdf"
        );
        // Idempotent: already-present filter is not duplicated.
        assert_eq!(
            query_with_filter("climate type:pdf", "type", "pdf"),
            "climate type:pdf"
        );
        // Empty base query yields just the filter.
        assert_eq!(query_with_filter("  ", "year", "2021"), "year:2021");
    }

    #[test]
    fn query_without_filter_removes_that_token() {
        assert_eq!(
            query_without_filter("climate type:pdf", "type", "pdf"),
            "climate"
        );
        // Leaves other filters and free text intact.
        assert_eq!(
            query_without_filter("climate type:pdf domain:ex.com", "type", "pdf"),
            "climate domain:ex.com"
        );
        // Removing an absent filter is a no-op (modulo whitespace normalization).
        assert_eq!(query_without_filter("climate", "type", "pdf"), "climate");
    }

    #[test]
    fn toggling_a_filter_round_trips() {
        let q = "coral reef";
        let added = query_with_filter(q, "collection", "coralreef-gov");
        assert_eq!(added, "coral reef collection:coralreef-gov");
        assert_eq!(
            query_without_filter(&added, "collection", "coralreef-gov"),
            "coral reef"
        );
    }
}
