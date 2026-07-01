# rustyweb - Design Document

rustyweb is a minimal, high-performance web archive server written in Rust. It provides full-text search over WACZ collections and serves them for in-browser replay via the ReplayWebPage/wabac.js service worker in **WACZ-direct mode** - the mode where the browser reads the archive directly, without a server-side proxy interpreting individual resource requests.

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
│   │       └── wacz.rs      - WACZ ZIP handling, datapackage.json, CDX reader
│   └── rustyweb-bin/        (thin CLI entry point)
│       └── src/main.rs      - Clap CLI, subcommand dispatch, tokio::main
└── static/replay/           (ReplayWebPage assets - embedded at compile time)
```

---

## CLI Interface

```
rustyweb index      [--index-dir <DIR>] [--name <NAME>] <PATH>...
rustyweb serve      [--index-dir <DIR>] [--bind <ADDR>]
rustyweb search-url [--index-dir <DIR>] <URL>
rustyweb verify     [--index-dir <DIR>]
```

- `index`: accepts `.wacz` files or directories (recursive scan). Extracts page HTML for full-text indexing, reads `datapackage.json` for collection metadata, records the SHA-256 of each WACZ, and updates `collections.json`. Default index dir: `./index`.
- `serve`: opens Tantivy read-only, starts Axum. Defaults: `127.0.0.1:8080`.
- `search-url`: opens each indexed WACZ, reads its internal `indexes/index.cdx.gz`, and prints all CDX records matching the given URL. Useful for debugging - does not require the CDX to be separately indexed.
- `verify`: re-hashes every WACZ in `collections.json` and compares against the stored SHA-256, reporting each collection as `OK`, `MODIFIED`, or `MISSING`. Exits non-zero on any failure so it can run unattended (cron/CI). This is the fixity check for the archive.

---

## Web Server Routes

| Route | Handler |
|---|---|
| `GET /` | Homepage: search box + collections with metadata and seed pages |
| `GET /search?q=...` | Server-rendered full-text search results with snippets and replay links |
| `GET /api/search?q=...` | Full-text search → JSON (used by clients) |
| `GET /files/{id}` | Stream a registered WACZ file with byte-range support |
| `GET /replay/viewer` | Viewer shell (reads `?source=&url=&ts=&name=` params) |
| `GET /replay/*` | Embedded ReplayWebPage static assets (JS, CSS, WASM, sw.js) |

---

## Tantivy Schema (Full-text Index)

Two document types share the same index, distinguished by `doc_type`.

| Field | Type | Stored | Indexed | Notes |
|---|---|---|---|---|
| `doc_type` | STRING | ✓ | exact | `"page"` or `"collection"` |
| `collection_id` | STRING | ✓ | exact | Short collection hash (e.g. `e02536ec`) |
| `collection_name` | STRING | ✓ | - | Human-readable collection name |
| `url` | STRING | ✓ | exact | Page URL (empty for collection docs) |
| `timestamp` | STRING | ✓ | - | 14-digit crawl timestamp |
| `title` | TEXT | ✓ | BM25 | Page title or collection name |
| `body` | TEXT | ✓ | BM25 | Page body text or collection description + seed URLs |

`body` is stored (not just indexed) so that Tantivy's `SnippetGenerator` can produce hit-highlighted excerpts without re-reading the source files.

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

`datapackage.json` inside each WACZ (WACZ spec §4) is read at index time and stored in `collections.json` alongside the existing file metadata.

Fields extracted:
- `title` - collection display name (falls back to filename stem)
- `description` - free-text description
- `created` - ISO 8601 crawl date
- Seed pages - first 3–5 entries from the `pages` array (url, title, timestamp)

The homepage displays this metadata per collection, giving users a preview of what each archive contains before searching or replaying.

---

## Collection Management

`{index_dir}/collections.json` - written/updated by `rustyweb index`, read by `rustyweb serve`.

```json
[
  {
    "id": "e02536ec",
    "path": "/data/archives/attar.wacz",
    "name": "Attar Silas",
    "date_indexed": "2026-07-01T00:00:00Z",
    "file_size": 104857600,
    "sha256": "e3b0c44298fc1c149afbf4c8996fb924...",
    "description": "Personal website of Attar Silas",
    "crawl_date": "2026-02-24T00:00:00Z",
    "seed_pages": [
      { "url": "https://www.attarsilas.fr/", "title": "Attar Silas", "ts": "20260224005439" }
    ]
  }
]
```

- `id`: first 8 hex chars of SHA-256 of the absolute path - stable as long as the file doesn't move
- Re-indexing the same path upserts the entry
- `GET /files/{id}` only serves files registered in `collections.json` - arbitrary filesystem access is not possible

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
       └── Iterate archive/*.warc.gz members
            └── For each HTML response record:
                 ├── Extract title (<title> tag)
                 ├── Extract body text (scraper, strips script/style/noscript)
                 └── Write page document to Tantivy (url, timestamp, title, body, collection_id)
```

Parallelism: Rayon parallel iterator over the WARC member list within each WACZ.

---

## ReplayWebPage Assets

`static/replay/` holds the ReplayWebPage JS bundle, embedded at compile time via `rust-embed`. The directory is **not committed** - a script downloads from npm before building.

```sh
./scripts/fetch-replay.sh          # download latest
./scripts/fetch-replay.sh 2.4.0   # pin a version
```

The script downloads `ui.js` and `sw.js` from the ReplayWebPage GitHub release. Builds are reproducible without network access - users run the script once on setup and re-run to upgrade.

---

## Key Crates

| Crate | Role |
|---|---|
| `axum` 0.8 | HTTP server |
| `tokio` 1.x | Async runtime |
| `tower-http` 0.7 | Compression, tracing middleware |
| `clap` 4.x | CLI (derive API) |
| `tantivy` 0.26 | Full-text search engine with snippet generation |
| `zip` 2.x | WACZ ZIP reading |
| `url` 2.x | URL parsing |
| `scraper` 0.27 | HTML parsing and text extraction |
| `serde_json` 1.x | JSON APIs and CDXJ parsing |
| `rust-embed` 8.x | Embed ReplayWebPage assets at compile time |
| `rayon` 1.x | Parallel WARC indexing |
| `tracing` + `tracing-subscriber` | Structured logging with per-level line coloring |
| `anyhow` 1.x | Error propagation |
| `flate2` 1.x | gzip decompression (WARC, WACZ CDX) |
| `sha2` 0.10 | Collection file hashing |
| `chrono` 0.4 | Date formatting |
