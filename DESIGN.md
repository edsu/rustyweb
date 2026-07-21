# rustyweb - Design Document

rustyweb is a minimal, high-performance web archive server written in Rust. It provides full-text search over WACZ collections and serves them for in-browser replay via the ReplayWebPage/wabac.js service worker in **WACZ-direct mode** - the mode where the browser reads the archive directly, without a server-side proxy interpreting individual resource requests.

A guiding design goal is **range**: rustyweb should serve small, local, and private use - an individual indexing a handful of their own WACZ files on a laptop, with nothing sent to a hosted service - and use the same model to scale up toward institutional collections. Peer tools like SHINE and SolrWayback assume the infrastructure of a large web archive (a Solr cluster); rustyweb deliberately does not, so it fits both ends of that range. This is why it ships as one binary with an embedded index, and why the two-level collection model (below) is built to serve both a solo curator and an institution reorganizing TBs of WARC.

Scope:
- Index WACZ files into a local full-text search index
- Serve WACZ files with byte-range support so wabac.js can read them directly
- Implement full-text search with hit-highlighted snippets
- Surface WACZ metadata (title, description, crawl date, seed pages) on the homepage
- Ship as a single self-contained binary (no Solr, no Elasticsearch, no separate database server)

---

## Architecture Overview

```
rustyweb index <files>                  rustyweb serve
       │                                       │
       ▼                                       ▼
  [Indexing pipeline]               [Axum HTTP server]
       │                                       │
       ├── HTML text ──► Tantivy              ├── GET /             → homepage
       ├── WACZ metadata ──► Tantivy          ├── GET /search?q=    → search results
       └── manifest ───► collections.json     ├── GET /api/search   → search JSON
                                              ├── GET /files/{id}   → WACZ byte-range
                                              └── GET /replay/viewer → viewer shell
```

Replay is handled entirely by the wabac.js service worker running in the browser. The service worker reads the WACZ file from `GET /files/{id}` using HTTP byte-range requests, extracts the CDX index from `indexes/index.cdx.gz` inside the ZIP, loads it into browser IndexedDB, and fetches individual WARC records by offset - all without making per-resource requests back to the rustyweb server. rustyweb's job during replay is purely to serve bytes efficiently.

---

## Cargo Workspace Layout

```
rustyweb/
├── Cargo.toml               (workspace root with [workspace.dependencies])
├── crates/
│   ├── rustyweb-lib/        (all logic - importable in tests)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── index.rs     - Indexing pipeline orchestration
│   │       ├── search.rs    - Tantivy schema, indexing, query execution, snippets
│   │       ├── server.rs    - Axum router and all HTTP handlers
│   │       ├── collections.rs - Collection manifest (collections.json)
│   │       ├── warc.rs      - WARC record iteration and HTML extraction
│   │       ├── wacz.rs      - WACZ ZIP handling, datapackage.json, CDX reader
│   │       ├── thumbnail.rs - Representative-image thumbnails (og:image → cached JPEG)
│   │       └── http_range.rs - Read+Seek over HTTP range requests (remote streaming)
│   └── rustyweb-bin/        (thin CLI entry point)
│       └── src/main.rs      - Clap CLI, subcommand dispatch, tokio::main
└── static/replay/           (ReplayWebPage assets - embedded at compile time)
```

---

## CLI Interface

```
rustyweb index          [--home <DIR>] [--name <NAME>] [--collection <NAME>] [-f|--from-file <FILE>] [--download] [--concurrency <N>] [-v|--verbose] <PATH|URL>...
rustyweb reindex        [--home <DIR>] [--concurrency <N>] [-v|--verbose]
rustyweb serve          [--home <DIR>] [--bind <ADDR>]
rustyweb collection set [--home <DIR>] <COLLECTION> <WACZ_ID>...
rustyweb collection list[--home <DIR>]
rustyweb crawl set      [--home <DIR>] <CRAWL_ID> --image <FILE>
rustyweb search-url     [--home <DIR>] <URL>
rustyweb verify         [--home <DIR>]
rustyweb import browsertrix [--home <DIR>] [--host <URL>] [--org <SLUG>] [--collection <ID|SLUG>] [--crawl <ID>] [--into <NAME>] [--include-unreviewed] [--min-review <N>] [--limit <N>] [--dry-run] [--force] [-v]
```

Every command takes `--home <DIR>` (default `.`). The home directory holds two
derived siblings: `<home>/archive/` (WACZ files) and `<home>/index/` (Tantivy
index + the JSON manifest, see *Collection Management*). Keeping them together
makes a home folder portable - move it to another disk or machine and it still
resolves.

- `index`: indexes one or more `.wacz` files or `http(s)://` URLs - at least one argument is required. **CDX-guided extraction is the default** for every WACZ (local or remote), reading only the records the CDX lists; a remote URL is read over HTTP range requests with no download, a local file straight from disk (see *Indexing Pipeline*). It falls back to a full WARC scan only when a WACZ can't be CDX-guided (deflated WARCs, or no readable CDX). `--download` fetches a remote WACZ into `<home>/archive` for a durable local copy instead of streaming it in place. A local WACZ must already live under `<home>/archive`; rustyweb indexes it in place (it does *not* copy files for you) and stores the source relative to home. A path outside the archive folder, a directory, or a non-`.wacz` file is an error with guidance; index several at once with a shell glob (`rustyweb index archive/*.wacz`). Extracts searchable page text (HTML, rendered `urn:text` or `pages/*.jsonl` text, PDFs), reads `datapackage.json` + `warcinfo` for provenance, records the SHA-256 of each local WACZ, and updates the manifest. `--collection <NAME>` groups the given WACZs under a curated collection (created if new); without it, each WACZ is its own singleton collection. `--from-file <FILE>` (or `-f -` for stdin) reads a newline-delimited list of files/URLs, ignoring blank lines and `#` comments, and combines with any positional args. Progress is shown as a bar on an interactive terminal; `-v`/`--verbose` replaces it with `DEBUG` logs (see *Indexing Pipeline → Progress reporting*). (A bare `index` with no arguments prints guidance pointing to `index archive/*.wacz` and `reindex`.)
- `reindex`: rebuild the full-text index from the sources already recorded in the manifest, preserving collection membership and metadata. Unlike `index`, this re-indexes every registered source - including remote URLs, which are re-fetched - and recreates the Tantivy index from scratch, so a schema change is picked up. It is *resilient*: a source that can't be indexed - a missing local file, or a remote source still failing after the retry budget - is skipped with a warning rather than aborting the whole rebuild, so one bad source can't torch a long reindex over many. The skipped source's manifest entry is preserved, the mostly-rebuilt index is still committed (usable), and if anything was skipped the command exits non-zero with a summary count - so a partial rebuild is visible to a human *and* to cron/CI, and re-running once the cause is fixed picks the skipped sources back up. Like `index`, it takes `--concurrency <N>` (records fetched at once per source) and shows the same per-WACZ progress bar on an interactive terminal (`-v`/`--verbose` swaps it for `DEBUG` logs) - welcome here since a full reindex re-streams every source. This is the intended way to migrate the index after a schema change (see below).
- `serve`: opens Tantivy read-only (so `index` can run concurrently), starts Axum. Defaults: `127.0.0.1:8080`.
- `collection set` / `collection list`: reassign WACZs to a collection (by WACZ id) / list collections and their members. Metadata like description and curator is edited in `collections.json` directly.
- `search-url`: opens each indexed WACZ, reads its internal `indexes/index.cdx.gz`, and prints all CDX records matching the given URL. Useful for debugging - does not require the CDX to be separately indexed.
- `verify`: re-hashes every WACZ in the manifest and compares against the stored SHA-256, reporting each as `OK`, `MODIFIED`, or `MISSING`. Exits non-zero on any failure so it can run unattended (cron/CI). This is the fixity check for the archive.
- `import <source>`: a group of importers that pull content from external web-archiving services (each source is its own subcommand, since their auth and selection differ; grouped so future sources — Archive-It, a WARC→WACZ builder — are siblings rather than new top-level verbs). `import browsertrix` authenticates to a [Browsertrix](https://browsertrix.com/) instance (credentials from `BROWSERTRIX_USER`/`BROWSERTRIX_PASSWORD` or `BROWSERTRIX_TOKEN` in the environment, never argv), resolves the org, and for each selected archived item **downloads** the WACZ into `<home>/archive/<item-id>/` (a per-item subfolder, so two items can't clash on a shared resource filename) via its presigned `replay.json` URL and indexes it as a durable local (File) source. Downloading (rather than streaming in place) is deliberate: Browsertrix presigned URLs expire in ~48h, so a streamed remote source would break replay — the download keeps replay durable. Selection: `--collection <ID|SLUG|NAME>` (resolved to the collection UUID the API requires) or `--crawl <ID>`; default is the whole org. **QA filter:** by default only crawls a reviewer has QA'd in Browsertrix (`reviewStatus` set) are imported; `--include-unreviewed` / `--min-review <1-5>` adjust this, and a single named `--crawl` is always included. **Incremental:** provenance recorded on each crawl (`browsertrix` field in the manifest: host, item id, resource hash) lets a re-run skip already-imported items unless `--force`. Importing a `--collection` groups its crawls into a rustyweb collection of the same name (`--into <NAME>` overrides, and groups org-wide/single-crawl imports); `--dry-run` lists without downloading. The HTTP client (`browsertrix.rs`) is transport-abstracted for testing (mirrors `http_range::RangeFetch`). See *Indexing Pipeline*.

---

## Web Server Routes

| Route | Handler |
|---|---|
| `GET /` | Homepage: search box, browse-by-facet entry points, and the collection overview |
| `GET /search?q=...&page=N` | Server-rendered results with a facet sidebar, month timeline, snippets, and pagination |
| `GET /collection/{id}` | Collection detail: metadata, a scoped facet overview, and member crawls |
| `GET /crawl/{id}` | Crawl detail: provenance, file metadata, a scoped facet overview, and seed pages (a crawl is one WACZ) |
| `GET /api/search?q=...` | Full-text search → JSON (results, `total`, `capped`, `facets`) |
| `GET /thumb/{id}` | A crawl's cached representative-image thumbnail (small JPEG); 404 when it has none |
| `GET /files/{id}` | Stream a registered WACZ file with byte-range support |
| `GET /assets/*` | Embedded site assets (the shared `app.css` stylesheet) |
| `GET /replay/viewer` | Viewer shell (reads `?source=&url=&ts=&name=&collection=` params) |
| `GET /replay/*` | Embedded ReplayWebPage static assets (JS, CSS, WASM, sw.js) |

---

## Tantivy Schema (Full-text Index)

Two document types share the same index, distinguished by `doc_type`.

| Field | Type | Stored | Indexed | Fast | Notes |
|---|---|---|---|---|---|
| `doc_type` | STRING | ✓ | exact | - | `"page"` or `"collection"` |
| `crawl_id` | STRING | ✓ | exact | - | Per-crawl (per-WACZ) hash, e.g. `e02536ec`; scoped by the `crawl:` filter |
| `crawl_name` | STRING | ✓ | - | - | Human-readable crawl (WACZ) name |
| `collection` | STRING | ✓ | exact | ✓ | Curated collection slug the crawl belongs to, for `collection:` filtering + faceting |
| `url` | STRING | ✓ | exact | - | Page URL (empty for collection docs) |
| `timestamp` | STRING | ✓ | - | - | 14-digit crawl timestamp |
| `title` | TEXT | ✓ | BM25 | - | Page title or collection name |
| `body` | TEXT | ✓ | BM25 | - | Page body text or collection description + seed URLs |
| `description` | TEXT | ✓ | BM25 | - | Page `<meta description>` / og:description; shown as a snippet fallback |
| `headings` | TEXT | - | BM25 | - | Page `<h1>`/`<h2>` text; boosted at query time |
| `keywords` | TEXT | - | BM25 | - | `<meta name=keywords>`; searchable via default fields |
| `author` | TEXT | - | BM25 | - | `<meta name=author>` / `article:author`; also `author:name` |
| `domain` | STRING | ✓ | exact | ✓ | Exact host of the page URL, for `domain:` filtering + results display |
| `site` | STRING | ✓ | exact | ✓ | Registrable domain (eTLD+1, via the PSL), for the cross-subdomain `site:` filter + the Site facet |
| `url_tokens` | TEXT | - | BM25 | - | URL host + path split into words, so URL words are searchable as ordinary terms |
| `year` | u64 | ✓ | numeric | ✓ | Four-digit crawl year, for `year:2021` / `year:[2020 TO 2023]` + the Year facet |
| `month` | u64 | ✓ | numeric | ✓ | Six-digit crawl month `YYYYMM`, for `month:202103` / ranges + the results timeline |
| `type` | STRING | ✓ | exact | ✓ | Coarse media type (`html` or `pdf`), for `type:pdf` filtering + facet |
| `lang` | STRING | ✓ | exact | ✓ | Primary language subtag (`en`); from `<html lang>`, else detected from body text; `lang:en` filtering + facet |
| `status` | u64 | ✓ | numeric | - | HTTP status code, for `status:200` / `status:[200 TO 299]` |
| `modified` | u64 | ✓ | numeric | - | Year from the HTTP `Last-Modified` header, for `modified:2015` |

`body` and `description` are stored (not just indexed) so that Tantivy's `SnippetGenerator` can produce hit-highlighted excerpts without re-reading the source files, and so a result can show the description when the query didn't match the body. `headings`, `keywords`, `author`, and `url_tokens` are indexed but not stored: they exist only to make that text findable; the canonical URL is kept in `url`.

The facet dimensions (`collection`, `site`, `type`, `lang`) and the numeric `year`/`month` are **fast** (columnar) fields. Fast storage is what lets them back Tantivy *terms aggregations*, which compute the per-value counts for the facet sidebar and the timeline. The string facet fields use the `raw` tokenizer so each field value is a single term (one bucket), rather than being split into words. Note the **Site facet is the registrable domain (`site`)**, not the raw host — so a whole site groups across subdomains; `domain:` remains available for exact-host filtering. `lang` is taken from the declared `<html lang>` when present, else detected from the body text with `whatlang` (single dominant language, only when confident); the code is normalized to a 639-1 subtag so declared and detected values unify.

### Query behavior

`SearchIndex::search_faceted(query, limit, offset)` runs one query and returns a page of
results, the total, facet counts, and the month timeline together (see *Faceted, temporal
discovery* below). `SearchIndex::search` is a thin wrapper returning just the hits.
`SearchIndex::facet_overview()` runs only the aggregation over a match-all query (the
homepage browse entry points); `facet_overview_scoped(FacetScope)` does the same restricted
to one collection (`collection`) or crawl (`crawl_id`) — this backs the **scoped facet
overview on the collection and crawl detail pages**, where each value (top sites, years,
types, languages) links into a search already scoped to that collection or crawl. The
crawl-scoped links use a `crawl:<id>` filter - a short alias rewritten to the `crawl_id`
field before parsing. Since the id is opaque, the search page resolves it to the crawl's
name for the active-filter chip.

Queries go through Tantivy's `QueryParser`, configured in `search_faceted`:

- **Default fields** are `title`, `headings`, `body`, `description`, and `url_tokens`, so a bare word matches any of them. Other fields are reachable with explicit `field:` syntax (`title:climate`, `domain:example.com`).
- **AND by default** (`set_conjunction_by_default`): `climate policy` requires both terms. Users can still write `OR`, `-`, `+`, `"phrases"`, `(groups)`, and `^boost`.
- **Field boosts** (`set_field_boost`): title matches rank highest, headings next, then body/description/URL.
- **Lenient parsing** (`parse_query_lenient`): a malformed query (stray quote, empty `field:`) yields a best-effort query rather than an error, so the search box never returns a 500 while a user experiments with syntax.

The `<details>` "Search tips" panel on the homepage and results page documents this syntax for end users; its examples must stay in sync with this configuration.

### Schema changes and migration

Tantivy persists the schema inside the index directory (`index/full_text/meta.json`) and reuses it when the index is opened. Changing the schema — adding a searchable field (as `domain`, `year`, `type`, `month` did) or making fields *fast* for faceting — therefore makes an index built by an older binary *stale*. To avoid writing/querying against a mismatched schema, `SearchIndex::open` compares the stored schema to the current one and, if they differ, returns an error telling the user to run `rustyweb reindex` rather than proceeding (which would otherwise panic on a missing field).

`rustyweb reindex` performs the migration: it reads `collections.json`, deletes the old `index/full_text`, recreates it with the current schema, and re-indexes every registered source (files and remote URLs). The manifest and collection names are preserved.

### Collection documents

One document per WACZ is indexed at `rustyweb index` time. Its `body` concatenates the collection description and seed page titles and URLs from `datapackage.json`. This makes the collection itself searchable: a query for "attar" returns both individual pages from that site and the collection whose metadata mentions it.

### Page documents

One document per HTML response in the WACZ. `body` is extracted from the `<body>` element with `<script>`, `<style>`, and `<noscript>` removed.

---

## Snippets and Hit Highlighting

Search results include a `snippet` field generated by Tantivy's `SnippetGenerator`. The generator:

1. Re-tokenizes the stored `body` text
2. Locates the window with the highest density of matched query terms
3. Returns the window as a string with matched terms wrapped in `<b>` tags

The server renders these `<b>` tags in the search results HTML; CSS applies a highlight background color.

---

## Collection Metadata

`datapackage.json` inside each WACZ (WACZ spec §4) is read at index time and stored on the
WACZ's manifest entry.

Fields extracted:
- `title` - WACZ display name (falls back to filename stem)
- `description` - free-text description
- `created` - ISO 8601 crawl date
- `software` - crawler/packager software (also enriched from the WARC `warcinfo`)
- Seed pages - first entries from the `pages` array (url, title, timestamp)

The crawl detail page shows this per crawl; the collection page aggregates it across members.

---

## Collection Management

The manifest is **two files** under `<home>/index/`, written by `rustyweb index` and read by
`rustyweb serve` - collections (curated groups) and the WACZs that belong to them:

```jsonc
// collections.json - one entry per curated collection
[
  { "id": "demo", "name": "Demo", "description": "A test set", "curator": null,
    "created": "2026-07-01T00:00:00Z" }
]

// waczs.json - one entry per WACZ member
[
  {
    "id": "e02536ec",
    "collection": "demo",                       // -> collections.json id
    "source": "archive/attar.wacz",
    "name": "Attar Silas",
    "date_indexed": "2026-07-01T00:00:00Z",
    "file_size": 104857600,
    "sha256": "e3b0c44298fc1c149afbf4c8996fb924...",
    "crawl_date": "2026-02-24T00:00:00Z",
    "software": ["browsertrix-crawler 1.0.0"],
    "seed_pages": [ { "url": "https://www.attarsilas.fr/", "title": "Attar Silas", "ts": "20260224005439" } ]
  }
]
```

- `source`: a local file path (stored relative to `<home>` when under it, e.g. `archive/attar.wacz`; absolute otherwise) or an `http(s)://` URL. Relative paths resolve against `<home>` at serve time, so the whole home folder is portable.
- `id`: first 8 hex chars of SHA-256 of the source string - relative sources give IDs that are stable across moves. Collection ids are slugs of the collection name.
- Re-indexing the same source upserts its WACZ entry; a WACZ with no `--collection` gets a singleton collection of its own.
- An older single-file `collections.json` (flat, per-WACZ with a `source` key) is detected and **migrated** on open into the two-file form.
- For a **file** source, `GET /files/{id}` streams the registered file with byte-range support; only registered files are served, so arbitrary filesystem access is not possible.
- For a **URL** source, replay points wabac.js directly at the remote URL (the host must provide range + CORS); `GET /files/{id}` just redirects there. rustyweb never proxies remote bytes.

---

## Discovery, Provenance & Collections

Discovery in rustyweb is search-first and faceted, over a two-level collection model, with
provenance surfaced rather than buried. This section explains that design and the reasoning
behind it; the *Planned* subsection at the end lists what is deliberately not built yet.

### Why (grounded in the literature)

- **Needs are mostly navigational and temporal.** Costa & Silva's query-log study of the
  Portuguese Web Archive found web-archive needs are ~53-81% *navigational* (see a page/site
  as it was, or how it changed over time), 14-38% *informational* (find information on a
  topic from the past), 5-16% *transactional*. So **time is a first-class axis**, and both
  known-item lookup (URL + date + versions) and topical full-text search matter.
- **Faceted "slice and dice" scales navigation better than clever ranking.** SHINE (UK Web
  Archive) and SolrWayback (Royal Danish Library), both built on the UK Web Archive's
  warc-indexer, offer facets for content-type, domain, crawl year, links, and public suffix.
  Facets are the established answer to a growing, unwieldy list. rustyweb follows this
  lineage directly — SolrWayback pairs the same faceted full-text search with in-browser
  replay — but swaps their Solr backend for a single embedded Tantivy index, so the same
  faceted search fits a private laptop archive as readily as an institutional one.
- **Provenance is essential and usually buried.** Maemura, Worby, Milligan & Becker, *If
  These Crawls Could Talk* (JASIST 2018): to trust and interpret an archive you must be able
  to evaluate its provenance, scope, and absences (curatorial intent, seeds/scope, crawler
  software/parameters, operator, dates).

### Two-level collection model

rustyweb uses a **two-level model** (see *Collection Management* above for the on-disk form):

- **Collection** - a curated grouping with *curatorial* provenance (name, description,
  curator, created date). The primary unit users browse and facet by; stored in
  `collections.json`.
- **WACZ members** - each carries *technical* provenance (crawler software, operator,
  user-agent, crawl date range, seeds, page counts, fixity). Stored in `waczs.json`, each
  pointing at its collection.

A WACZ indexed without `--collection` becomes a singleton collection, so the flat case still
works. This model serves both audiences: an **individual** self-hosting WACZs made with wget
or browsertrix-crawler gets context with no hosted-service dependency; an **institution**
(e.g. TBs of WARC behind pywb) can reorganize crawls into navigable, provenance-bearing
collections. It is also the structural fix for the "long list" problem.

**Vocabulary (UI vs. data model).** WACZ is a *packaging format* - a technical container -
which most users don't think in terms of; they think in **crawls**. So the web UI presents
each WACZ member as a "crawl" (the `/crawl/{id}` detail page, the "Crawls" count on
collections). "WACZ" is kept only where the file/format is genuinely what's meant - the
`index`/`reindex` CLI, `/files/{id}` byte-range serving, replay source, and fixity. The
data model (`Wacz`, `waczs.json`, `wacz_by_id`) stays WACZ-named, since there it *is* the
file; the rename is a presentation-layer relabel. (A WACZ is 1:1 with a crawl today; if the
Browsertrix importer later distinguishes crawls from uploads, the label can follow the
item's actual type.)

### Provenance

rustyweb extracts provenance from the WACZ/WARC and presents it **prominently** - a
provenance panel on the WACZ detail page and a compact provenance line on collection member
listings - rather than tucking it away. Sources used:

- **`datapackage.json`** (WACZ 1.1.1): `title`, `description`, `created`, `software`.
- **WARC `warcinfo` record** (`application/warc-fields`, one per WARC, read by `warc.rs`):
  `software`, `operator`, `http-header-user-agent`, `robots`.
- **Timestamps**: capture date range (earliest/latest) and page counts.

Fixity is verifiable with `rustyweb verify` (re-hashing each WACZ against the stored
SHA-256). Signature-based authenticity (`datapackage-digest.json`, the WACZ auth spec) is
*Planned* (below).

### Faceted, temporal discovery

The results page is search-first and faceted, implemented in `search_faceted` +
`views.rs`:

- **Facet sidebar** with live counts for collection, year, site (registrable domain),
  content type, and language. Each value is a link that toggles a `field:value` filter on
  the query; applied filters show as removable chips. Counts come from Tantivy *terms
  aggregations* over the fast fields, computed in the **same query pass** as the results, so
  they always reflect the current query. Beyond the faceted fields, results can also be
  filtered by `author:`, `domain:` (exact host), `status:` (HTTP status), and `modified:`
  (Last-Modified year).
- **Month timeline** - a chronological histogram (a terms aggregation on `month`) above the
  results; each bar toggles a `month:` filter.
- **Repeat-capture grouping** - multiple captures of the same URL collapse into one result
  showing "captured N times". Tantivy has no native field collapsing, so grouping is done
  over the top `CANDIDATE_CAP` scored captures per query (`SearchResponse.capped` flags when
  more matched).
- **Pagination** over the grouped results (`?page=N`).
- **Search-first homepage** - a prominent search box, "browse by year"/"top sites" entry
  points (from an archive-wide facet overview), then the collection cards.

A key distinction: **facet and timeline counts count captures and are exact over the whole
match set; the result total counts distinct URLs and is bounded by `CANDIDATE_CAP`.** They
measure different things, so a facet count is generally larger than the number of grouped
results it yields.

### Planned / not yet built

- **Authenticity**: verify `datapackage-digest.json` signatures (WACZ auth spec), surfaced
  alongside fixity. Tracked by `rustyweb-authenticity-671`.
- **Search enrichment**: keywords/author, language-detection fallback, the `site:`
  registrable-domain facet, HTTP `status:`, and `modified:` (Last-Modified year) have
  shipped. Still open under `rustyweb-search-enrichment-6by`: outbound-link fields (deferred
  over index size), plus a `crawler` facet.
- **Browsertrix import**: pull WACZs from a Browsertrix org's public API into `<home>/archive`.
  Tracked by `rustyweb-15z` (includes nested/multi-WACZ indexing).

### References

- Costa & Silva, *Understanding the Information Needs of Web Archive Users*, IWAW 2010.
- Maemura, Worby, Milligan & Becker, *If These Crawls Could Talk: Studying and Documenting
  Web Archives Provenance*, JASIST 2018.
- SHINE (`github.com/ukwa/shine`) and SolrWayback (`github.com/netarchivesuite/solrwayback`),
  both on the UK Web Archive's warc-indexer / webarchive-discovery
  (`github.com/ukwa/webarchive-discovery`).
- WACZ 1.1.1 and the WACZ auth spec; the WARC 1.0 format specification.

---

## Replay Viewer

`GET /replay/viewer` serves a thin HTML shell (`static/replay/viewer.html`) that:

1. Reads `source`, `url`, `ts`, and `name` from the URL query string
2. Renders a banner bar showing the collection name and current page URL
3. Mounts a `<replay-web-page>` component with the given `source` and `url`

```html
<div id="banner">
  <a href="/">rustyweb</a>
  <span id="collection-name"></span>
  <span id="current-url"></span>
</div>
<replay-web-page id="rp"></replay-web-page>
```

The `<replay-web-page>` component fires a `rwp-url-change` event as the user navigates within the archive; the banner listens for this event and updates the displayed URL in real time.

In WACZ-direct mode the component reads the WACZ from `/files/{id}` via byte-range requests, loads the internal CDX into browser IndexedDB, and serves all resources from WARC bytes without making per-resource calls to rustyweb. All URL rewriting, wombat.js injection, fuzzy matching, and redirect handling are performed client-side by wabac.js.

---

## WACZ CDX Reader (`wacz.rs`)

`rustyweb search-url` reads WACZ CDX files on-the-fly without a separate CDX store. The implementation:

1. Opens the WACZ as a ZIP
2. Reads and decompresses `indexes/index.cdx.gz`
3. Parses each CDXJ line (space-separated SURT + timestamp + JSON fields)
4. Matches lines by URL equality or SURT prefix

This is intentionally lazy - no persistent CDX index is maintained by rustyweb. The WACZ's built-in CDX is authoritative; rustyweb simply reads it when asked.

---

## Indexing Pipeline

```
Input WACZ
  └── Open as ZIP
       ├── Read datapackage.json ──► WaczMetadata (title, description, crawl date, seed pages)
       │    └── Index as collection document in Tantivy
       │    └── Write to collections.json
       ├── Iterate archive/*.warc(.gz) members, collecting per record:
       │    ├── HTML response        ──► title (<title>) + scraped body text
       │    ├── urn:text: resource   ──► fully rendered (post-JS) page text
       │    └── application/pdf resp ──► extracted PDF text (title from filename)
       └── Read pages/pages.jsonl + pages/extraPages.jsonl `text` field
            └── fully rendered page text (where a crawl stores it here
                instead of as urn:text: records) + a fallback title
                 │
                 └── Merge into one document per URL (body prefers rendered
                     text, then PDF text, then scraped HTML; title from HTML)
                     └── Derive domain/year/month/type/lang, then write the page
                         document to Tantivy (see the schema table above)
```

Records are collected across all inner WARCs before merging, because a page's
rendered `urn:text` often lives in a different WARC than its HTML response.
Rendered text is *also* read from `pages/pages.jsonl` and `pages/extraPages.jsonl`
(Browsertrix's `text` field): many crawls - including this era of SUCHO WACZs -
store the fully rendered, post-JS page text only there, not as `urn:text:` WARC
records. Without it, JS-rendered content is visible in replay but unsearchable.
The jsonl text is merged into the same per-URL document (interchangeable with
`urn:text:`), so it enriches the existing HTML-response document rather than
adding a duplicate.
Collapsing to one document per URL deduplicates repeat captures *within a WACZ*;
repeat captures of the same URL *across* WACZs stay as separate documents and are
grouped at query time instead (see *Faceted, temporal discovery*).

Parallelism: Rayon parallel iterator over the WARC member list within each WACZ;
merge and Tantivy writes happen once per WACZ.

### CDX-guided extraction (default) vs. full scan (fallback)

There are two extraction modes, sharing the same per-record transform
(`record_to_raw`) and merge step (`index_merged`) so they produce an identical
index:

- **CDX-guided** (`index_wacz_streaming`, over a pluggable `RangeFetch` byte
  source) - **the default**: read the WACZ's CDX, then fetch *only* the page
  records (HTML/PDF responses and `urn:text:` rendered text) at
  `data_start + offset` for the length the CDX gives. Media (images/JS/CSS/JSON)
  and pseudo-records (pageinfo, thumbnail) are never read. It also reads the
  `pages/*.jsonl` `text` once during setup (cheap, alongside the CDX), folding it
  in as rendered text - so crawls that store rendered text only in the pages files
  are fully searchable even though the CDX never points at it. The byte source is
  pluggable (`http_range.rs`): a local `FileFetch`, or an `HttpFetch` issuing HTTP
  range requests for a remote WACZ - so a remote WACZ is indexed **without
  downloading it**, fetching only the central directory, the CDX, and the page
  records. The same primitive wabac.js uses for replay.
- **Scan** (`index_wacz`) - **the fallback**: decompress and inspect *every*
  WARC record. Used only when a WACZ can't be CDX-guided (see below).

**Why CDX-guided is the default everywhere.** Extraction mode is really about
CDX-guided vs. full-scan, not local vs. remote - the reader abstracts over where
the bytes live. And **replay already resolves records through the CDX** (wabac.js
range-reads against it), so a WACZ with a broken CDX wouldn't replay regardless;
indexing from the same index keeps the two consistent, and there's no additional
trust to lose. The remaining reason to scan is purely mechanical: some WACZs
*can't* be CDX-guided.

Requirements and caveats, grounded in the WACZ spec:

- CDX-guided extraction relies on the spec's SHOULD that `archive/` WARCs are
  **Stored** (uncompressed) in the ZIP, so a CDX byte offset maps to an absolute
  position. A WARC gzip member is one record; the CDX `index.cdx.gz` is often a
  multi-member gzip (ZipNum blocks), so it's read with `MultiGzDecoder`.
- **Automatic fallback to scan.** `local_warcs_streamable` / `remote_warcs_streamable`
  probe the central directory: if the WARCs aren't Stored (a WACZ deflates them,
  violating the SHOULD) or there's no readable CDX, rustyweb scans instead. For a
  remote host without range support, the fallback downloads a temp copy and scans
  it, keeping the URL as the source. No user flag selects the mode; it's decided
  per WACZ.
- `--download` instead fetches the remote WACZ into `<home>/archive` and indexes
  it as a local file (durable copy, whole-file SHA-256, offline replay). The
  downloaded copy is itself CDX-guided when its WARCs are Stored.
- **Fixity of streamed sources**: a streamed remote is never read in full, so it
  has no whole-file SHA-256 (empty in the manifest) and its `file_size` comes
  from the HTTP `Content-Length`. `verify` already skips remote sources, so this
  is consistent; per-resource integrity from `datapackage-digest.json` is future
  work (see *Planned*).

The ZIP/CDX **setup** (central directory, CDX, per-WARC data-starts, warcinfo)
runs serially over a buffered `RangeReader`, which uses a rolling read-ahead
buffer for forward reads plus a one-time cache of the last 1 MiB (the EOCD +
central directory). The ZIP central directory is at the end of the file and is
touched once per entry while local headers are read scattered across the file;
without the tail cache those two regions thrash a single buffer, turning the open
of a multi-GB ZIP64 WACZ into hundreds of range requests.

The per-record **fetch** phase is **concurrent**: the CDX gives each record an
independent `(offset, length)`, so a dedicated pool of `--concurrency` workers
(default 4 for remote — gentle on the host; CPU count for local, and clamped to a
per-host ceiling of 64) each does an independent
`RangeFetch::fetch` + gunzip + extract, driven by an atomic progress counter.
This hides per-record round-trip latency - the big win for remote WACZs, which
would otherwise be one serial HTTP round trip each - and parallelizes HTML/PDF
text extraction (CPU) across cores. Remote is latency-bound so more workers than
cores helps; local fetch is cheap and the work is CPU-bound extraction, so the
core count is the sweet spot.

**Resilient + polite remote fetching.** Every HTTP fetch (the range GETs and the
whole-file downloads) retries transient failures - network errors and HTTP
`429`/`502`/`503`/`504` - with capped exponential backoff + jitter, honoring a
server `Retry-After` (`with_retry` in `http_range.rs`). This makes a long ingest
survive blips, and is deliberately *polite*: when a host pushes back we wait
rather than hammer it. That matters because a single WACZ's `--concurrency`
requests all hit one host, so an aggressive setting against a small (non-object-
store) server could otherwise overload it or get the client IP-blocked. As a
proactive backstop the resolved worker count is clamped to a per-host ceiling
(`MAX_CONCURRENCY` = 64), so even a mis-typed `--concurrency 500` can never put an
unbounded number of range requests in flight against a single host. The agent
is built with `http_status_as_error(false)` so `4xx`/`5xx` come back as
inspectable responses (status + `Retry-After`) rather than opaque errors.

### Representative-image thumbnails

To make the UI visual, each crawl gets a small representative image on its card
and detail pages, chosen in tiers (first hit wins):

0. a **Browsertrix page screenshot** of the main page, if the crawl captured one.
   Browsertrix (with screenshots enabled - common) stores a rendered image of each
   page as a WARC record keyed by a `urn:` URL: `urn:thumbnail:<page>` (a small,
   ready-made JPEG - preferred) or `urn:view:<page>` (the full-page PNG). It's an
   actual picture of the page, so it beats every heuristic below and works even for
   JS-rendered sites; matched on the exact page URL (tolerating a trailing-slash
   difference).
1. else the crawl's **main-page `og:image`** (the site's own social-preview image;
   `twitter:image` next);
2. else the **largest content image the main page embeds** (`<img>`/`srcset`,
   resolved against the page URL);
3. else the **largest captured image on the crawl's own registrable domain**
   (`site_of`), read straight from the CDX.

Tiers 1-3 matter for crawls *without* screenshots: `og:image` is far from
universal - cultural-heritage crawls (SUCHO) and even some magazines omit it - and
tier 3 specifically handles **JS-rendered sites**, whose *saved* HTML has no
`og:image` and no `<img>` at all (images are injected client-side), yet the crawl
still captured them; it ignores third-party/CDN/ad images on other domains. Tiers
2-3 pick by captured byte size within a window (`MIN_IMAGE_BYTES` 5 KB ..
`MAX_IMAGE_BYTES` 3 MB): the floor skips icons/sprites/tracking pixels, the ceiling
avoids fetching + decoding a full-res original for a 400px thumbnail. (Tier 0's
screenshot is purpose-built, so it skips that window.)

After indexing a CDX-streamable WACZ, `thumbnail::generate` (best-effort) checks
for a screenshot first, then reads the main page's HTML for the `og:image` /
embedded-image tiers, range-fetches the chosen image from the CDX, decodes +
downscales it (the `image` crate; longest edge 400px), and writes
`<home>/index/thumbs/<crawl_id>.jpg`. Any failure - no usable image, an image that
isn't captured or won't decode - just means no thumbnail (the UI shows a
placeholder; a curator can pin one).

A curator can **pin a specific image** with `rustyweb crawl set <id> --image
<file>` (any local PNG/JPEG/WebP/GIF): it's downscaled, cached, and marked pinned
via a sidecar `<crawl_id>.pinned` file, so a later (re)index leaves it untouched.

The server serves the thumbnail at `GET /thumb/{id}`. The homepage collection card and the
crawl detail page each show one image; the **collection detail page shows a grid
of its member crawls**, each with its own thumbnail, so the page conveys that a
collection spans multiple crawls of multiple sites. When a crawl has no image the
UI falls back to a **CSS-only placeholder** (a gradient tinted by a hash of the
name - no image bytes). Thumbnails are generated at index time, so populating
them needs a (re)index.

### Progress reporting

Indexing reports progress through a small, UI-agnostic `IndexProgress` trait
(`begin` / `phase` / `set_total` / `set_records` / `finish`): the library only
emits counts and phase labels, so it stays free of any UI dependency. The binary
implements the trait with an [indicatif] bar - an indeterminate spinner during
setup (probe / download / reading the CDX), which flips to a determinate bar with
throughput and ETA once the CDX yields the page-record total. A fresh bar is
created per WACZ (and cleared when it finishes), so it's only on screen while a
WACZ is being worked on and never collides with log lines.

Logging vs. the bar (all overridable via `RUST_LOG`): an interactive `index`
hushes `INFO` (the bar carries progress; `WARN`/`ERROR` still print); `-v` /
`--verbose` shows `DEBUG` logs and no bar; a non-TTY (piping / CI) keeps `INFO`
and shows no bar, so logs aren't lost.

[indicatif]: https://docs.rs/indicatif

---

## ReplayWebPage Assets

`static/replay/` holds the [ReplayWeb.page][rwp] JS bundle (`ui.js` + `sw.js`), embedded in the binary at compile time via `rust-embed` and served under `/replay/`. Replay runs in WACZ-direct mode: the `<replay-web-page>` component reads the WACZ over byte-range from our `/files/{id}` endpoint and serves every resource client-side through its service worker (`sw.js`); rustyweb does no server-side rewriting (see *viewer.html* / `server.rs`).

These two files **are committed**, **pinned** to a specific `replaywebpage` npm release (currently **2.4.6**), so builds are reproducible and offline. They are vendored assets, not a Cargo dependency, so **Dependabot does not track them** - upgrading is a deliberate manual step via `scripts/fetch-replay.sh`:

```sh
./scripts/fetch-replay.sh          # re-fetch the pinned VERSION (2.4.6)
./scripts/fetch-replay.sh 2.4.7    # fetch a specific version (one-off)
```

To upgrade: pick a version from <https://www.npmjs.com/package/replaywebpage>, bump `VERSION` in `scripts/fetch-replay.sh`, re-run it (downloads `ui.js`/`sw.js` from the jsDelivr npm CDN), rebuild, **re-test replay in a browser** (`cargo test -p rustyweb-lib --test browser` needs Chrome + chromedriver), then commit the refreshed assets. Do this periodically so replay keeps up with wabac.js fixes.

[rwp]: https://replayweb.page

---

## Key Crates

| Crate | Role |
|---|---|
| `axum` 0.8 | HTTP server |
| `tokio` 1.x | Async runtime |
| `tower-http` 0.7 | Compression, tracing middleware |
| `clap` 4.x | CLI (derive API) |
| `indicatif` 0.17 | Indexing progress bar / spinner |
| `tantivy` 0.26 | Full-text search engine with snippet generation |
| `zip` 2.x | WACZ ZIP reading |
| `url` 2.x | URL parsing |
| `scraper` 0.27 | HTML parsing and text extraction |
| `serde_json` 1.x | JSON APIs and CDXJ parsing |
| `rust-embed` 8.x | Embed ReplayWebPage assets at compile time |
| `rayon` 1.x | Parallel WARC scan + concurrent CDX-guided record fetch |
| `tracing` + `tracing-subscriber` | Structured logging with per-level line coloring |
| `anyhow` 1.x | Error propagation |
| `flate2` 1.x | gzip decompression (WARC, WACZ CDX) |
| `sha2` 0.10 | Collection file hashing |
| `chrono` 0.4 | Date formatting |
