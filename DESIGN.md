# rustyweb — Design Document

rustyweb is a minimal, high-performance web archive server written in Rust. It takes inspiration from **pywb** (Python Wayback), specifically its CDX API and WARC serving pipeline, but strips out everything not needed for read-only archive replay: no live recording, no proxy mode, no crawling. The scope is:

- Index WARC and WACZ files into a local on-disk index
- Serve the ReplayWebPage JS client as static assets
- Implement the IA-compatible CDX API that ReplayWebPage calls during replay
- Implement a fulltext search API over indexed content
- Ship as a single self-contained binary (no Solr, no Elasticsearch, no separate database server)
- Scale target: billions of CDX records

---

## Architecture Overview

```
rustyweb index <files>           rustyweb serve              rustyweb check
       │                                │                           │
       ▼                                ▼                           ▼
  [Indexing pipeline]           [Axum HTTP server]        [Integrity check]
       │                                │                     collections.json
       ├── CDX records ──► Fjall        ├── GET /            → verify sha256
       ├── HTML text ───► Tantivy       ├── GET /search?q=
       └── manifest ────► collections   ├── GET /cdx/search/cdx ──► Fjall
             .json        .json         ├── GET /api/search ──────► Tantivy
                                        ├── GET /files/{id} ───────► registered archive file
                                        ├── GET /replay/viewer
                                        ├── GET /warcreplay/* ─────► WARC record (seek by offset)
                                        └── GET /replay/* ──────────► embedded ReplayWebPage assets
```

---

## Index Storage: Fjall + Tantivy

CDX lookups during replay are **latency-critical prefix key scans** — ReplayWebPage calls the CDX API on every resource request while a page loads. Tantivy is an inverted index optimized for fulltext relevance ranking; it is the wrong tool for ordered key scans. Fjall is a pure-Rust LSM-tree (RocksDB-equivalent) specifically designed for sorted prefix/range iteration.

This matches how **OutbackCDX** works in production (RocksDB at 8–9 billion records for national libraries). Fjall is the pure-Rust equivalent, requiring no C++ build dependencies.

| Concern | Storage | Why |
|---|---|---|
| CDX index (URL → capture metadata) | **Fjall** | LSM-tree, sorted prefix scans, ~200 bytes/record, billions of records |
| Fulltext search (HTML body text) | **Tantivy** | Inverted index, BM25 relevance scoring |

---

## Fjall CDX Key Schema

**Key**: `<surt_url>\x00<timestamp>`

- `surt_url`: SURT-canonicalized URL (e.g. `https://www.example.com/path` → `com,example,www)/path`)
- `\x00`: null byte separator (cannot appear in SURT URLs)
- `timestamp`: 14-digit string (`20240115120000`) — string sort order = chronological order

This schema answers all CDX query patterns with a single `range()` call:

| CDX query | Fjall operation |
|---|---|
| Exact URL | range `surt\x00` .. `surt\x00\xff\xff` |
| URL prefix (`example.com/*`) | `scan_prefix("com,example)/")` |
| URL + time range | range `surt\x00T1` .. `surt\x00T2` |
| Domain (all subdomains) | `scan_prefix("com,example,")` |
| Closest capture | prefix scan, seek to nearest timestamp |

**Value** (bincode-serialized struct):

```rust
struct CdxRecord {
    original_url: String,
    timestamp: String,       // 14-digit
    mimetype: String,
    status: u16,
    digest: String,          // "sha1:ABCDEF..."
    length: u64,             // WARC payload length
    warc_path: String,       // "path/to/file.warc.gz" or "file.wacz!archive/data.warc.gz"
    warc_offset: u64,        // byte offset of WARC record
    warc_record_length: u64, // compressed record length (for range reads)
}
```

The `!` separator in `warc_path` encodes WACZ-internal entries: `file.wacz!archive/data.warc.gz`.

---

## Tantivy Schema (Fulltext Index)

Only HTML responses are indexed for fulltext — binary assets (images, JS, CSS) are skipped.

| Field | Type | Stored | Indexed |
|---|---|---|---|
| `surt_url` | STRING | ✓ | exact (for filtering) |
| `original_url` | TEXT | ✓ | no |
| `timestamp` | STRING | ✓ | no |
| `title` | TEXT | ✓ | BM25 tokenized |
| `body` | TEXT | ✗ | BM25 tokenized (not stored to save space) |
| `mimetype` | STRING | ✓ | no |
| `warc_path` | TEXT | ✓ | no |
| `warc_offset` | u64 | ✓ | no |

---

## Cargo Workspace Layout

```
rustyweb/
├── Cargo.toml               (workspace root with [workspace.dependencies])
├── crates/
│   ├── rustyweb-lib/        (all logic — importable in tests)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── cdx.rs       — SURT canonicalization, Fjall I/O, CDX types
│   │       ├── index.rs     — Indexing pipeline orchestration
│   │       ├── search.rs    — Tantivy schema + query execution
│   │       ├── server.rs    — Axum router + all HTTP handlers
│   │       ├── warc.rs      — WARC parsing wrapper
│   │       └── wacz.rs      — WACZ ZIP handling
│   └── rustyweb-bin/        (thin CLI entry point)
│       └── src/main.rs      — Clap CLI, subcommand dispatch, tokio::main
└── static/replay/           (ReplayWebPage assets — embedded at compile time)
```

---

## CLI Interface

```
rustyweb index  [--index-dir <DIR>] [--name <NAME>] [--jobs <N>] <PATH>...
rustyweb serve  [--index-dir <DIR>] [--bind <ADDR>] [--port <PORT>]
rustyweb check  [--index-dir <DIR>]
rustyweb lookup [--index-dir <DIR>] <URL>
```

- `index`: accepts `.warc`, `.warc.gz`, `.wacz`, or directories (recursive scan). Default index dir: `./rustyweb-index`. `--name` sets the collection display name (defaults to filename stem). Updates `collections.json` after indexing.
- `serve`: opens Fjall + Tantivy read-only, starts Axum server. Defaults: `127.0.0.1:8080`.
- `check`: reads `collections.json`, re-hashes each registered file, and reports which files are present/missing/modified. Does not run on startup — called explicitly.
- `lookup`: applies the same three-level CDX lookup (exact → fuzzy → prefix) against the index and prints all matching records with their WARC location. Useful for debugging replay 404s.
- Incremental indexing is safe: Fjall inserts overwrite duplicate keys; Tantivy uses `surt_url + timestamp` as a deduplication ID.

---

## Web Server Routes

| Route | Handler |
|---|---|
| `GET /` | Homepage: search box + indexed collections table |
| `GET /search?q=...` | Server-rendered fulltext search results + replay links |
| `GET /files/{id}` | Stream a registered WARC or WACZ file by collection ID |
| `GET /replay/viewer` | Replay viewer shell (reads `?source=&url=` params, mounts `<replay-web-page>`) |
| `GET /replay/` | Serve ReplayWebPage HTML shell |
| `GET /replay/*` | Serve embedded ReplayWebPage static assets (JS, CSS, WASM) |
| `GET /cdx/search/cdx` | IA-compatible CDX API → Fjall query |
| `GET /api/search` | Fulltext search → Tantivy query (JSON) |
| `GET /warcreplay/*` | Read raw WARC record at stored offset, stream HTTP response body |

### CDX API

IA CDX API v3 compatible (same format pywb serves). Query params: `url`, `output` (`json`/`text`), `limit`, `from`, `to`, `fl`, `matchType`. Response for `output=json` is a JSON array of arrays, first row being field names — the exact format ReplayWebPage expects.

### WARC Record Serving

Handler for `/warcreplay/*`:
1. Look up `warc_path` + `warc_offset` from query params
2. Open the WARC file (or ZIP member for WACZ) and seek to offset
3. Decompress if `.warc.gz`
4. Strip WARC headers (read past `\r\n\r\n`)
5. Stream HTTP response body back via Axum streaming response

---

## Collection Management

rustyweb is primarily an **index** over archive files, not a custody system. Files stay wherever the user put them; rustyweb tracks what it has indexed via a manifest and can serve registered files for replay.

### Collections manifest

`{index_dir}/collections.json` — written/updated by `rustyweb index`, read by `rustyweb serve` and `rustyweb check`.

```json
[
  {
    "id": "abc12345",
    "path": "/data/archives/my-collection.wacz",
    "name": "my-collection",
    "kind": "wacz",
    "date_indexed": "2026-07-01T00:00:00Z",
    "record_count": 1247,
    "file_size": 104857600,
    "sha256": "e3b0c44298fc1c149afbf4c8996fb924..."
  }
]
```

- `id`: first 8 hex chars of SHA-256 of the absolute path — stable as long as the file doesn't move
- `kind`: `"wacz"` or `"warc"` — drives how the replay viewer link is constructed
- `sha256`: hash of the file contents computed once at index time
- Re-indexing the same path upserts the entry (updates `record_count`, `date_indexed`, `sha256`)

### Integrity checking

Re-hashing a large WARC on every server startup would be too slow. Instead, `rustyweb check` is an explicit subcommand:

```
rustyweb check --index-dir ./rustyweb-index
```

It reads `collections.json`, re-hashes each file, and reports:
- ✓ OK — file present, hash matches
- ⚠ MISSING — file not found at registered path
- ⚠ MODIFIED — file present but hash does not match

The homepage shows a last-checked timestamp per collection if a check has been run.

### File serving security

`GET /files/{id}` only serves files that are registered in `collections.json`. It looks up the collection by `id`, retrieves the stored `path`, and streams the file. Arbitrary filesystem access is not possible.

### Format philosophy

- WACZ files are served whole — preserves producer-generated metadata (`datapackage.json`, `pages.jsonl`) and collection grouping
- WARC files are served directly — ReplayWebPage accepts both WACZ and WARC via its `source=` attribute
- No format conversion in either direction; if a user wants WARC metadata enriched, `py-wacz` is the right tool before ingest

---

## Homepage and Search UI

All UI is server-rendered HTML with minimal inline CSS — no template engine dependency, no JavaScript framework.

### Homepage (`GET /`)

- Search box submitting to `GET /search?q=...`
- "Indexed collections" table: name, file, record count, date indexed, integrity status

### Search results (`GET /search?q=...`)

- Queries Tantivy fulltext index
- Each result row: title, URL, timestamp, **Replay** link
- Replay link: `/replay/viewer?source=/files/{collection_id}&url={original_url}`
- Collection ID resolved by matching `warc_path` in the search result against the manifest

### Replay viewer (`GET /replay/viewer`)

Static HTML page embedded at `static/replay/viewer.html`. Reads `source` and `url` from URL params, mounts:

```html
<replay-web-page source="..." url="..." embed="replayonly"></replay-web-page>
```

Requires real ReplayWebPage assets (see §ReplayWebPage Assets below).

---

## ReplayWebPage Assets

`static/replay/` holds the ReplayWebPage JS bundle, embedded at compile time via `rust-embed`. The directory is **not committed** — a script downloads from npm before building.

```sh
./scripts/fetch-replay.sh          # download latest
./scripts/fetch-replay.sh 2.4.0   # pin a version
```

The script downloads `ui.js` and `sw.js` directly from `https://replayweb.page/` (the project's GitHub Pages deployment — there is no versioned npm/jsDelivr distribution). Builds are reproducible without network access — users run the script once on setup and re-run to upgrade.

---

## Indexing Pipeline

```
Input files → [WARC reader] → for each response record:
    ├── Build CdxRecord (URL, timestamp, MIME, status, digest, offset, length)
    │   └── Compute SURT URL
    │   └── Write to Fjall partition "cdx"
    └── If text/html:
        └── Parse HTML with `scraper`, extract title + body text
        └── Write Tantivy document
```

WACZ files are opened as ZIP archives (`zip` crate). WARC members under `archive/` are iterated as WARC streams. The `pages/pages.jsonl` file inside WACZ can supplement titles for pages where HTML body wasn't stored.

Parallelism: Rayon parallel iterator over the WARC file list. Fjall supports concurrent writes. Tantivy's `IndexWriter` is shared via `Arc<Mutex<>>` or its built-in thread-safe API.

---

## SURT Canonicalization

Implemented as a pure function (no dependencies beyond `url` crate):

1. Parse URL
2. Split host on `.`, reverse, join with `,`
3. Append `)` + path + query
4. Drop scheme

`https://www.example.com/path?q=1` → `com,example,www)/path?q=1`

For CDX wildcard `example.com/*`: strip `/*`, compute SURT prefix `com,example)/`, use `scan_prefix`.

---

## Non-GET Request Handling

The CDX API is **transparent to HTTP method** — wabac.js/ReplayWebPage encodes POST/PUT bodies into the URL at request time, and rustyweb encodes them at index time. The same CDX endpoint and key structure handles both GET and non-GET.

### Encoding algorithm (per [IIPC spec](https://iipc.github.io/warc-specifications/guidelines/cdx-non-get-requests/))

| Request | CDX lookup key |
|---|---|
| `GET /page?q=1` | `http://example.com/page?q=1` |
| `POST /api` with form body `a=1&b=2` | `http://example.com/api?__wb_method=POST&a=1&b=2` |
| `POST /api` with JSON body `{"id":42}` | `http://example.com/api?__wb_method=POST&id=42.0` |
| `POST /api` with binary body | `http://example.com/api?__wb_method=POST&__wb_post_data=<base64>` |

Encoding rules by `Content-Type`:
- `application/x-www-form-urlencoded` → decode, append params individually
- `application/json` → parse JSON, flatten to query params (arrays as `field.2_=val`, booleans as `True`/`False`)
- `multipart/*` → parse per RFC 2388, append params
- anything else → base64-encode entire body, append as `__wb_post_data`

WARC files pair `request` + `response` records by `WARC-Concurrent-To` headers. The indexer buffers the request record to extract method and body before writing the CDX key.

---

## Fuzzy URL Matching

Fuzzy matching happens at two distinct layers in the replay stack, and it is important to understand where each layer operates.

### Layer 1 — wabac.js client-side (IndexedDB)

wabac.js **does not call the rustyweb CDX API** for resource lookups during replay. Instead it:

1. Loads the CDX index from inside the WACZ file (the `indexes/` directory) when the collection is first opened.
2. Stores all CDX records in a browser-side **IndexedDB** database.
3. On every resource request, performs the CDX lookup locally against that database.
4. Only after finding a record does it issue a byte-range request to `GET /files/{id}` to retrieve the actual WARC bytes.

The rustyweb `GET /cdx/search/cdx` endpoint is used by the viewer page to find the collection ID and build the initial replay URL — it is **not** used per-resource during replay.

#### wabac.js fuzzy rules

wabac.js ships ~168 built-in fuzzy rules (`src/fuzzymatcher.ts`, `DEFAULT_RULES`). When an exact URL lookup fails:

1. The URL is tested against each rule's `match` regex.
2. If a rule matches, a `fuzzyCanonUrl` is computed (e.g. strip everything after a query separator).
3. A **prefix scan** is performed against IndexedDB for all captures starting with that canonical prefix.
4. Results are **scored** by comparing query parameters numerically and textually (Levenshtein distance); the highest-scoring capture wins.

Key rule for media assets:

```typescript
// strips query string from common media extensions
{ match: /(\.(?:js|webm|mp4|gif|jpg|png|css|json|m3u8))\?.*/i, replace: "$1", maxResults: 2 }
```

**Gap**: this rule lists `.jpg` but not `.jpeg`. URLs ending in `.jpeg?w=50` receive no client-side fuzzy treatment and fall through to the network (and therefore to the rustyweb backend).

### Layer 2 — rustyweb `ir_resource_inner` backend

When the wabac.js service worker cannot find a resource in its local IndexedDB, or when byte-range serving fails, the browser falls through to the network. For URLs that wabac.js has already rewritten to the form `http://host/replay/{id}/{ts}{mod}/{url}`, this means rustyweb's `replay_handler` receives the request.

#### Query-string capture problem

HTTP parses the first `?` as the query string delimiter. A browser request for:

```
GET /replay/8efb2ac7/20260609213407im_/https://cdn.example.com/img.jpeg?w=50
```

arrives at Axum with:
- **path** `{*path}` = `8efb2ac7/20260609213407im_/https://cdn.example.com/img.jpeg`  (no `?w=50`)
- **query** = `w=50`

Axum's `{*path}` wildcard captures only the path component, so `?w=50` is silently dropped unless explicitly extracted with `RawQuery`. The inner URL reconstructed from the path alone is `https://cdn.example.com/img.jpeg`, which has no query string.

The handler must extract `RawQuery` and re-attach it to the inner URL before CDX lookup.

#### Three-level fallback in `ir_resource_inner`

```
1. Exact match:    CDX lookup for the full reconstructed URL (with query string)
2. Scheme flip:    retry with http:// if https:// returned nothing
3. Fuzzy strip:    normalize_url_fuzzy() — removes tracking params, sorts the rest
4. Prefix match:   strip the entire query string; scan CDX prefix
                   → picks the result closest in timestamp to the requested ts
                   → handles ?w=50 → {?w=20, ?w=800} and similar CDN size variants
```

The prefix match (step 4) runs whether or not the URL originally had a query string, because the inner URL may arrive without one (query was stripped by Axum). This ensures that `.jpeg` files, and any other URL where the archived version differs in query parameters from what the browser requests, are served correctly.

### CDX API fuzzy fallback

The `GET /cdx/search/cdx` endpoint has its own simpler fuzzy fallback for the viewer's initial page lookup: when an exact match returns no results, `normalize_url_fuzzy` is applied and the query retried. This is sufficient for the CDX API use case (finding the top-level page) and does not need the full prefix-match treatment.

---

## Key Crates

| Crate | Role |
|---|---|
| `axum` 0.8 | HTTP server |
| `tokio` 1.x | Async runtime |
| `tower-http` 0.7 | Compression, static file middleware |
| `clap` 4.x | CLI (derive API) |
| `fjall` 3.x | Sorted CDX index (LSM-tree, pure Rust) |
| `tantivy` 0.26 | Fulltext search engine |
| `warc` / `rust_warc` / `fastwarc` | WARC file parsing (evaluate all three) |
| `zip` 2.x | WACZ ZIP reading |
| `url` 2.x | URL parsing for SURT |
| `scraper` 0.27 | HTML parsing + text extraction |
| `serde` + `bincode` 2.x | Fjall value serialization |
| `serde_json` 1.x | CDX API JSON responses |
| `rust-embed` 8.x | Embed ReplayWebPage assets at compile time |
| `rayon` 1.x | Parallel WARC indexing |
| `tracing` + `tracing-subscriber` | Structured logging |
| `anyhow` 1.x | Error propagation |

---

## TDD Approach

Write failing tests first for each module, then implement to make them pass.

### Initial tests (`src/cdx.rs` unit tests)

```rust
fn surt_simple_url()         // "https://example.com/path" → "com,example)/path"
fn surt_strips_scheme()      // http and https produce same key
fn surt_subdomain()          // "www.example.com" → "com,example,www)"
fn surt_preserves_path()     // query string preserved after ")"
fn surt_wildcard_prefix()    // "example.com/*" → prefix "com,example)/"
fn surt_domain_matchtype()   // "example.com" domain → prefix "com,example,"

fn cdx_key_encodes_get()     // GET url → surt + "\x00" + timestamp
fn cdx_key_post_form()       // POST with form body → __wb_method=POST&... in URL before SURT
fn cdx_key_post_json()       // POST with JSON body → flattened params
fn cdx_key_post_binary()     // POST with binary body → __wb_post_data=<base64>

fn fuzzy_strips_utm()        // utm_source/utm_campaign removed
fn fuzzy_strips_fbclid()     // fbclid removed
fn fuzzy_normalizes_params() // remaining params sorted alphabetically
fn fuzzy_unchanged_url()     // URL with no ephemeral params → unchanged
```

### Integration tests (`tests/`)

```rust
fn index_warc_produces_cdx_records()
fn index_warc_html_response_indexed()
fn index_warc_binary_not_indexed()
fn index_wacz_extracts_inner_warcs()
fn index_post_request_encoded_key()
fn index_incremental_is_idempotent()

fn cdx_api_exact_match()
fn cdx_api_prefix_match()
fn cdx_api_time_range()
fn cdx_api_no_match_returns_empty()
fn cdx_api_fuzzy_fallback()
fn search_api_returns_results()
fn search_api_no_results()
fn replay_assets_served()
```

Fixture files in `crates/rustyweb-lib/tests/fixtures/`: `simple.warc.gz`, `post.warc.gz`, `simple.wacz`.

---

## Implementation Sequence

1. Scaffold workspace — `cargo check` passes
2. Write all failing tests (stubs with `todo!()`)
3. SURT + CDX key encoding — SURT unit tests pass
4. POST body encoding — POST unit tests pass
5. Fjall CDX store — write, prefix scan, range queries
6. WARC parser — request+response pairing; `index_warc_*` tests pass
7. WACZ ZIP support — `index_wacz_*` test passes
8. `index` subcommand wired end-to-end
9. Axum server + CDX API route — `cdx_api_*` tests pass
10. Fuzzy matching fallback — `cdx_api_fuzzy_fallback` passes
11. ReplayWebPage assets embedded — **browser replay works** ✓ first milestone
12. Tantivy indexing — HTML extraction wired in
13. `/api/search` route — `search_api_*` tests pass
14. Rayon parallelism, structured logging, graceful shutdown
15. `scripts/fetch-replay.sh` — download real ReplayWebPage assets
16. Collection manifest — `collections.json` written on `rustyweb index`; fix WACZ `warc_path` bug
17. `rustyweb check` subcommand — re-hash files, report integrity status
18. `GET /files/{id}` route — serve registered WARC/WACZ files
19. Homepage (`GET /`) — search box + collections table
20. Search results page (`GET /search?q=`) — server-rendered HTML with replay links
21. Replay viewer (`static/replay/viewer.html` + `GET /replay/viewer`) — mounts `<replay-web-page>`
