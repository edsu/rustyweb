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
rustyweb index <files>           rustyweb serve
       │                                │
       ▼                                ▼
  [Indexing pipeline]           [Axum HTTP server]
       │                                │
       ├── CDX records ──► Fjall        ├── GET /cdx/search/cdx ──► Fjall
       └── HTML text ───► Tantivy       ├── GET /api/search ──────► Tantivy
                                        ├── GET /warcreplay/* ─────► WARC files (seek by offset)
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
rustyweb index [--index-dir <DIR>] [--jobs <N>] <PATH>...
rustyweb serve [--index-dir <DIR>] [--bind <ADDR>] [--port <PORT>]
```

- `index`: accepts `.warc`, `.warc.gz`, `.wacz`, or directories (recursive scan). Default index dir: `./rustyweb-index`.
- `serve`: opens Fjall + Tantivy read-only, starts Axum server. Defaults: `127.0.0.1:8080`.
- Incremental indexing is safe: Fjall inserts overwrite duplicate keys; Tantivy uses `surt_url + timestamp` as a deduplication ID.

---

## Web Server Routes

| Route | Handler |
|---|---|
| `GET /replay/` | Serve ReplayWebPage HTML shell |
| `GET /replay/*` | Serve embedded ReplayWebPage static assets (JS, CSS) |
| `GET /cdx/search/cdx` | IA-compatible CDX API → Fjall query |
| `GET /api/search` | Fulltext search → Tantivy query |
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

When a CDX request returns no results, rustyweb falls back to a fuzzy match: strip ephemeral query parameters from the URL, recompute SURT, and do a prefix scan.

### URL normalization rules

| Category | Parameters stripped |
|---|---|
| Analytics | `utm_*` (any parameter starting with `utm_`) |
| Ad tracking | `fbclid`, `gclid`, `msclkid`, `dclid`, `yclid` |
| Cache-busting | `_`, `cb`, `_cb`, `_ts`, `nocache`, `bust` |
| JSONP | `_callback`, `_jsonp`, `callback` |
| Session IDs | `sessionid`, `session_id`, `jsessionid`, `phpsessid` |

After stripping, remaining params are sorted alphabetically for consistent matching.

Per-domain rules can be added via an optional `rules.yaml` config (modeled on [pywb's rules.yaml](https://github.com/webrecorder/pywb/blob/main/pywb/rules.yaml)).

### Fallback logic

```
fn cdx_lookup(url, params, fjall) -> Vec<CdxRecord> {
    let results = fjall_query(to_surt(url), params);
    if !results.is_empty() { return results; }

    let normalized = normalize_url_fuzzy(url);
    if normalized != url {
        return fjall_query(to_surt(&normalized), &prefix_params);
    }
    vec![]
}
```

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
