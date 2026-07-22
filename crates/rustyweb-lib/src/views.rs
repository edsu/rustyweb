//! Server-rendered HTML views, built with [Maud]. Handlers in `server.rs` gather
//! data and hand it to these functions, which return a [`Markup`] response.
//! Shared page chrome lives in [`layout`]; styling lives in the served
//! `/assets/app.css` stylesheet (no inline `<style>`).
//!
//! [Maud]: https://maud.lambda.xyz/

use maud::{html, Markup, PreEscaped, DOCTYPE};

/// The shared page shell: doctype, head (with the stylesheet link), and body.
pub fn layout(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (title) }
                link rel="stylesheet" href="/assets/app.css";
            }
            body { (body) }
        }
    }
}

/// The "Search tips" disclosure shown on the homepage and results page. The
/// examples must stay in sync with how `SearchIndex::search` configures the
/// query parser (AND-by-default, default fields, and the `field:` filters).
pub fn search_tips() -> Markup {
    html! {
        details.tips {
            summary { "Search tips" }
            div.tips-body {
                p {
                    "Type words to search page titles, headings, body text, descriptions, "
                    "keywords, author, and URLs. "
                    strong { "All words must match" } " - " code { "climate policy" }
                    " finds pages containing both."
                }
                ul {
                    li { code { "\"climate policy\"" } " - an exact phrase (use quotes)" }
                    li { code { "climate OR weather" } " - either word" }
                    li { code { "climate -policy" } " - has \"climate\", excludes \"policy\"" }
                    li { code { "(climate OR weather) risk" } " - group with parentheses" }
                    li { code { "title:climate" } " - match only in the page title" }
                    li { code { "author:hopper" } " - match the page author" }
                    li { code { "site:example.com" } " - a whole site, across subdomains" }
                    li { code { "domain:www.example.com" } " - only that exact host" }
                    li { code { "collection:demo" } " - only pages in that collection" }
                    li { code { "year:2021" } " or " code { "year:[2020 TO 2023]" } " - filter by crawl year" }
                    li { code { "month:202103" } " or " code { "month:[202101 TO 202106]" } " - filter by crawl month" }
                    li { code { "modified:2015" } " - filter by Last-Modified year" }
                    li { code { "type:pdf" } " - only PDFs (or " code { "type:html" } ")" }
                    li { code { "lang:en" } " - only pages in that language" }
                    li { code { "status:200" } " - filter by HTTP status (or " code { "status:[200 TO 299]" } ")" }
                    li { code { "climate^2 change" } " - rank \"climate\" matches higher" }
                }
                p.tips-note {
                    "Searches are case-insensitive. Title matches rank above body matches. "
                    code { "domain:" } " needs the exact host (e.g. " code { "www.example.com" }
                    "); to match host words loosely, just type them (e.g. " code { "example" } ")."
                }
            }
        }
    }
}

/// The top bar on inner pages: the home link plus a search form. On the results
/// page the box is prefilled with the current query; elsewhere it shows a
/// placeholder.
fn top_bar(query: Option<&str>) -> Markup {
    html! {
        div.top {
            a.home href="/" { "rustyweb" }
            form.search-form action="/search" method="get" {
                @if let Some(q) = query {
                    input type="search" name="q" value=(q);
                } @else {
                    input type="search" name="q" placeholder="Search all collections…";
                }
                button type="submit" { "Search" }
            }
        }
    }
}

/// A collection as shown on a homepage card.
pub struct CollectionCard {
    pub id: String,
    pub name: String,
    pub count: usize,
    pub description: Option<String>,
    pub date_range: Option<String>,
    /// `/thumb/{id}` for a representative member crawl, if any has one.
    pub thumb: Option<String>,
    /// Whether the collection has any locally-stored / any remote member — both
    /// true means a mixed collection (show both pills).
    pub has_local: bool,
    pub has_remote: bool,
}

/// A card/detail representative image. Shows the cached thumbnail if present,
/// otherwise a CSS placeholder tinted by a hash of `seed` (so cards vary a bit).
fn thumb_area(thumb: Option<&str>, seed: &str) -> Markup {
    html! {
        @match thumb {
            Some(src) => div.thumb { img src=(src) alt="" loading="lazy"; },
            None => div.thumb.placeholder style=(placeholder_style(seed)) {},
        }
    }
}

/// A deterministic gradient for a placeholder, its hue derived from `seed` so
/// each collection/crawl gets a stable, distinct tint.
fn placeholder_style(seed: &str) -> String {
    let hue = seed
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
        % 360;
    format!("background:linear-gradient(135deg,hsl({hue},45%,72%),hsl({hue},38%,55%))")
}

/// A single browse entry point on the homepage: a label, its count, and the
/// search link it leads to (e.g. a year or a site).
pub struct BrowseLink {
    pub label: String,
    pub count: u64,
    pub href: String,
}

/// Archive-wide browse entry points shown on the homepage.
pub struct HomeBrowse {
    pub years: Vec<BrowseLink>,
    pub sites: Vec<BrowseLink>,
}

/// One labeled group of browse-links in a detail page's scoped facet overview
/// (e.g. "Top sites" on a collection page), each link a search within that scope.
pub struct FacetSection {
    pub label: String,
    pub links: Vec<BrowseLink>,
}

/// Render a `.browse` block from facet sections (reused on detail pages). Empty
/// sections render nothing.
fn facet_browse(facets: &[FacetSection]) -> Markup {
    html! {
        @if !facets.is_empty() {
            div.browse {
                @for f in facets {
                    div.browse-group {
                        h3 { (f.label) }
                        div.browse-links {
                            @for l in &f.links {
                                a.browse-link href=(l.href) {
                                    (l.label) " " span.browse-count { (l.count) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The homepage: search box, tips, browse-by entry points, and a card per
/// collection.
pub fn home(cards: &[CollectionCard], browse: &HomeBrowse) -> Markup {
    let body = html! {
        h1 { "rustyweb" }
        p.tagline { "Web archive search and replay" }
        form.search-form.home action="/search" method="get" {
            input type="search" name="q" placeholder="Search archived pages…" autofocus;
            button type="submit" { "Search" }
        }
        (search_tips())
        @if !browse.years.is_empty() || !browse.sites.is_empty() {
            div.browse {
                @if !browse.years.is_empty() {
                    div.browse-group {
                        h3 { "Browse by year" }
                        div.browse-links {
                            @for y in &browse.years {
                                a.browse-link href=(y.href) {
                                    (y.label) " " span.browse-count { (y.count) }
                                }
                            }
                        }
                    }
                }
                @if !browse.sites.is_empty() {
                    div.browse-group {
                        h3 { "Top sites" }
                        div.browse-links {
                            @for s in &browse.sites {
                                a.browse-link href=(s.href) {
                                    (s.label) " " span.browse-count { (s.count) }
                                }
                            }
                        }
                    }
                }
            }
        }
        h2 { "Collections" }
        @if cards.is_empty() {
            p.muted {
                "No collections indexed yet. Run "
                code { "rustyweb index archive/*.wacz" } " to get started."
            }
        }
        div.cards {
            @for c in cards {
                div.card {
                    a.card-thumb href=(format!("/collection/{}", c.id)) {
                        (thumb_area(c.thumb.as_deref(), &c.name))
                    }
                    div.card-header {
                        span.card-title-wrap {
                            @if c.has_local { (source_badge(false)) }
                            @if c.has_remote { (source_badge(true)) }
                            a.card-title href=(format!("/collection/{}", c.id)) { (c.name) }
                        }
                        span.status.muted {
                            (c.count) " crawl" @if c.count != 1 { "s" }
                        }
                    }
                    @if let Some(d) = &c.description {
                        p.desc { (d) }
                    }
                    @if let Some(r) = &c.date_range {
                        div.prov { (r) }
                    }
                }
            }
        }
    };
    layout("rustyweb", body)
}

// ── Search results ─────────────────────────────────────────────────────────

/// One row of the search results table. The handler computes the replay `href`,
/// display strings, and the (pre-escaped) snippet HTML; the view just lays it out.
pub struct SearchResultRow {
    pub href: String,
    pub title: String,
    pub is_collection: bool,
    /// Display URL (empty for a collection-level hit, which shows a badge).
    pub url: String,
    /// Pre-formatted timestamp, empty when there is none to show.
    pub timestamp_display: String,
    /// Pre-escaped snippet HTML (may contain Tantivy `<b>` highlight tags).
    pub snippet_html: Option<String>,
    /// URL-encoded curated-collection id, for the "in <collection>" link.
    pub coll_href: String,
    /// Display name of the curated collection.
    pub coll_display: String,
    /// How many captures of this URL matched (>1 shows a "captured N times" note).
    pub capture_count: usize,
}

/// Pagination state for the results page: the current 1-based page, the total
/// number of pages, and the total match count (across all pages).
pub struct PageNav {
    pub page: usize,
    pub total_pages: usize,
    pub total_hits: usize,
    /// True when more captures matched than were scanned for grouping, so the
    /// total is shown as a floor (e.g. "1000+").
    pub capped: bool,
    /// The URL-encoded query, so page links can preserve it.
    pub query_encoded: String,
}

/// The facet sidebar: the filters currently active in the query, plus a group
/// of clickable counts per facet dimension.
pub struct FacetSidebar {
    pub active: Vec<ActiveFilter>,
    pub groups: Vec<FacetGroupView>,
}

/// A `field:value` filter currently applied, with a link that removes it.
pub struct ActiveFilter {
    pub label: String,
    pub value: String,
    pub remove_href: String,
}

/// One facet dimension in the sidebar.
pub struct FacetGroupView {
    pub label: String,
    pub items: Vec<FacetItem>,
}

/// One clickable facet value: its count, the link that toggles it, and whether
/// it is currently applied.
pub struct FacetItem {
    pub value: String,
    pub count: u64,
    pub href: String,
    pub active: bool,
}

/// One bar of the results timeline: a crawl month, its count, a height
/// percentage (0–100), a toggle link, and whether that month is filtered.
pub struct TimelineBar {
    pub label: String,
    pub count: u64,
    pub pct: u32,
    pub href: String,
    pub active: bool,
}

/// The search results page: top bar, tips, a count line, an active-filter row,
/// a month timeline, then a facet sidebar beside the results table with
/// prev/next pagination.
pub fn search_results(
    query: &str,
    nav: &PageNav,
    sidebar: &FacetSidebar,
    timeline: &[TimelineBar],
    rows: &[SearchResultRow],
) -> Markup {
    // Preserve the query when linking to another page.
    let page_href = |p: usize| format!("/search?q={}&page={}", nav.query_encoded, p);
    let body = html! {
        (top_bar(Some(query)))
        (search_tips())
        div.count {
            @if nav.total_hits == 0 {
                "No results for " em { (query) }
            } @else {
                (nav.total_hits) @if nav.capped { "+" } " result" @if nav.total_hits != 1 { "s" } " for " em { (query) }
                @if nav.total_pages > 1 {
                    " · page " (nav.page) " of " (nav.total_pages)
                }
            }
        }
        @if !sidebar.active.is_empty() {
            div.active-filters {
                span.active-label { "Filters:" }
                @for f in &sidebar.active {
                    a.filter-chip href=(f.remove_href) {
                        span.chip-label { (f.label) ": " }
                        (f.value) " ✕"
                    }
                }
            }
        }
        @if timeline.len() >= 2 {
            div.timeline title="Results by crawl month — click a bar to filter" {
                @for b in timeline {
                    a.tl-bar.active[b.active] href=(b.href) title=(format!("{}: {} result{}", b.label, b.count, if b.count == 1 { "" } else { "s" })) {
                        span.tl-fill style=(format!("height:{}%", b.pct.max(3))) {}
                        span.tl-label { (b.label) }
                    }
                }
            }
        }
        div.results-layout {
            @if !sidebar.groups.is_empty() {
                aside.facets {
                    @for g in &sidebar.groups {
                        div.facet-group {
                            h3 { (g.label) }
                            ul {
                                @for it in &g.items {
                                    li.facet-item.active[it.active] {
                                        a href=(it.href) {
                                            span.facet-value { (it.value) }
                                            span.facet-count { (it.count) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div.results-main {
                @if !rows.is_empty() {
                    table.results {
                        tbody {
                            @for r in rows {
                                tr {
                                    td {
                                        div.result-title { a href=(r.href) { (r.title) } }
                                        div.result-meta {
                                            @if r.is_collection {
                                                span.result-coll-badge { "Collection" }
                                            } @else {
                                                div.result-url { (r.url) }
                                            }
                                            @if !r.is_collection && !r.timestamp_display.is_empty() {
                                                div.result-ts {
                                                    (r.timestamp_display)
                                                    @if r.capture_count > 1 {
                                                        span.capture-count { " · captured " (r.capture_count) " times" }
                                                    }
                                                }
                                            }
                                        }
                                        @if let Some(s) = &r.snippet_html {
                                            div.snippet { (PreEscaped(s)) }
                                        }
                                        div.result-coll {
                                            "in " a href=(format!("/collection/{}", r.coll_href)) { em { (r.coll_display) } }
                                        }
                                    }
                                    td.replay-col {
                                        a.result-replay href=(r.href) { "Replay →" }
                                    }
                                }
                            }
                        }
                    }
                }
                @if nav.total_pages > 1 {
                    nav.pagination {
                        @if nav.page > 1 {
                            a.page-prev href=(page_href(nav.page - 1)) { "← Previous" }
                        } @else {
                            span.page-prev.disabled { "← Previous" }
                        }
                        span.page-info { "Page " (nav.page) " of " (nav.total_pages) }
                        @if nav.page < nav.total_pages {
                            a.page-next href=(page_href(nav.page + 1)) { "Next →" }
                        } @else {
                            span.page-next.disabled { "Next →" }
                        }
                    }
                }
            }
        }
    };
    layout(&format!("{query} - rustyweb"), body)
}

// ── Shared metadata / provenance rows ────────────────────────────────────────

/// A single `<th>/<td>` row in a metadata table. `mono` renders the value in a
/// monospace cell (for URLs, user-agents, hashes).
pub struct MetaRow {
    pub label: String,
    pub value: String,
    pub mono: bool,
}

impl MetaRow {
    pub fn new(label: &str, value: impl Into<String>) -> Self {
        MetaRow {
            label: label.to_string(),
            value: value.into(),
            mono: false,
        }
    }
    pub fn mono(label: &str, value: impl Into<String>) -> Self {
        MetaRow {
            label: label.to_string(),
            value: value.into(),
            mono: true,
        }
    }
}

fn meta_table(rows: &[MetaRow]) -> Markup {
    html! {
        table.meta {
            @for r in rows {
                tr {
                    th { (r.label) }
                    @if r.mono { td.mono { (r.value) } } @else { td { (r.value) } }
                }
            }
        }
    }
}

// ── Collection detail ────────────────────────────────────────────────────────

/// A member crawl (WACZ) as shown in a collection's grid.
pub struct MemberItem {
    pub id: String,
    pub name: String,
    pub present: bool,
    /// Whether this crawl is hosted remotely (streamed) rather than local.
    pub remote: bool,
    /// One-line provenance summary (plain text), if any is known.
    pub provenance: Option<String>,
    /// `/thumb/{id}` for this crawl's representative image, if it has one.
    pub thumb: Option<String>,
}

/// A pill labelling where a crawl's WACZ lives: `💾 Local` (stored in this
/// home's `archive/`) or `🌐 Remote` (fetched from a remote host at replay time).
fn source_badge(remote: bool) -> Markup {
    if remote {
        html! {
            span.source-badge.remote
                title="Hosted remotely — rustyweb streams this at replay time and doesn't keep a local copy" {
                "🌐 Remote"
            }
        }
    } else {
        html! {
            span.source-badge.local title="Stored locally in this home's archive folder" {
                "💾 Local"
            }
        }
    }
}

/// The collection detail page: metadata + facets, then a grid of the member
/// crawls, each with its own representative image (a collection spans multiple
/// crawls of multiple sites, so the grid conveys that breadth better than one
/// hero image would).
pub fn collection(
    name: &str,
    description: Option<&str>,
    meta: &[MetaRow],
    facets: &[FacetSection],
    members: &[MemberItem],
) -> Markup {
    let body = html! {
        (top_bar(None))
        h1.page-title { (name) }
        @if let Some(d) = description { p.desc { (d) } }
        (meta_table(meta))
        (facet_browse(facets))
        h2 { "Crawls" }
        @if members.is_empty() {
            p.muted { "No crawls in this collection." }
        } @else {
            div.cards {
                @for m in members {
                    div.card {
                        a.card-thumb href=(format!("/crawl/{}", m.id)) {
                            (thumb_area(m.thumb.as_deref(), &m.name))
                        }
                        div.card-header {
                            span.card-title-wrap {
                                (source_badge(m.remote))
                                a.card-title href=(format!("/crawl/{}", m.id)) { (m.name) }
                            }
                            @if m.present {
                                span.status.ok { "✓" }
                            } @else {
                                span.status.missing { "✗" }
                            }
                        }
                        @if let Some(p) = &m.provenance { div.prov { (p) } }
                    }
                }
            }
        }
    };
    layout(&format!("{name} - rustyweb"), body)
}

// ── Crawl detail ─────────────────────────────────────────────────────────────

/// A seed page listed on a crawl detail page.
pub struct PageItem {
    pub href: String,
    pub title: String,
    pub url: String,
}

/// All the data the crawl detail page renders. The handler resolves links,
/// formats sizes/dates, and gathers provenance/file rows; the view lays them out.
pub struct CrawlPage {
    /// `(collection_id, collection_name)` breadcrumb, if the crawl has one.
    pub crumb: Option<(String, String)>,
    pub name: String,
    pub description: Option<String>,
    /// `/thumb/{id}` for this crawl's representative image, if it has one.
    pub thumb: Option<String>,
    pub replay_href: String,
    /// Whether the crawl is hosted remotely (a URL or a streamed Browsertrix
    /// source) rather than stored in `<home>/archive`.
    pub remote: bool,
    pub provenance: Vec<MetaRow>,
    pub source: String,
    pub size: String,
    pub sha_short: String,
    pub sha_full: String,
    pub crawled: Option<String>,
    pub indexed: String,
    pub present: bool,
    /// Scoped facet overview of what this crawl captured (sites/years/types/…).
    pub facets: Vec<FacetSection>,
    pub pages: Vec<PageItem>,
}

/// The crawl detail page: provenance panel, file metadata, and seed-page list.
pub fn crawl(p: &CrawlPage) -> Markup {
    let body = html! {
        (top_bar(None))
        @if let Some((id, cname)) = &p.crumb {
            div.crumb { "in " a href=(format!("/collection/{}", id)) { (cname) } }
        }
        div.detail-thumb { (thumb_area(p.thumb.as_deref(), &p.name)) }
        div.crawl-title {
            (source_badge(p.remote))
            h1.page-title { (p.name) }
        }
        @if let Some(d) = &p.description { p.desc { (d) } }
        a.replay-btn href=(p.replay_href) { "Replay →" }

        @if !p.provenance.is_empty() {
            h2 { "Provenance" }
            (meta_table(&p.provenance))
        }

        h2 { "File" }
        table.meta {
            tr { th { "Source" } td.mono { (p.source) } }
            tr { th { "Size" } td { (p.size) } }
            tr { th { "SHA-256" } td.mono title=(p.sha_full) { (p.sha_short) "…" } }
            @if let Some(c) = &p.crawled { tr { th { "Crawled" } td { (c) } } }
            tr { th { "Indexed" } td { (p.indexed) } }
            tr {
                th { "Status" }
                td {
                    @if p.present { span.ok { "✓ present" } } @else { span.missing { "✗ missing" } }
                }
            }
        }

        (facet_browse(&p.facets))

        h2 { "Pages" }
        @if p.pages.is_empty() {
            p.muted { "No pages are listed in this crawl." }
        } @else {
            ul.pages {
                @for pg in &p.pages {
                    li {
                        a href=(pg.href) { (pg.title) }
                        div.result-url { (pg.url) }
                    }
                }
            }
        }
    };
    layout(&format!("{} - rustyweb", p.name), body)
}
