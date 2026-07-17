# rustyweb

**Note bene**: *rustyweb is alpha software and has been written extensively
with the support of Claude Code. Like any piece of software it may contain
bugs. The developer's understanding of how it operates at a low level may be
limited. See the [DESIGN.md](DESIGN.md) document for the overall design
principles. Technical reviews of the code and design are always welcome!*

---

**rustyweb** is a small, fast web archive server written in Rust. Think of it
as a [reading room] for web archives. Point it at a pile of local or remote
[WACZ] files and it gives you:

- **Full-text search with faceted, temporal browsing** - hit-highlighted
  snippets, then narrow by collection, site, date, type, or language, with a
  timeline for navigating through time
- **Provenance up front** - see how each crawl was made (software, operator,
  dates, seeds, page counts) and verify each WACZ's fixity, instead of taking
  the archive on faith
- **In-browser replay** of the archived pages via [ReplayWeb.page] / wabac.js

It ships as a single self-contained binary - no Solr, no Elasticsearch, no
separate database server.

> **The web archive replay is entirely [Webrecorder]'s work.** rustyweb bundles
> and serves [ReplayWeb.page] and [wabac.js] - the browser-side engine that does
> all the actual replay - and adds a thin Rust layer for indexing, search, and
> serving. Webrecorder did the heavy lifting; please support them. See
> [Credits](#credits).

## Discovery and provenance

The "reading room" idea is that you should be able to *find* things in an
archive and *understand* what you're looking at - not just replay a URL you
already know. Two findings from web-archiving research shape rustyweb (both
expanded, with citations, in [DESIGN.md](DESIGN.md)):

- **Web-archive use is mostly navigational and temporal** - seeing a page or
  site as it was, or how it changed over time (Costa & Silva's query-log study
  of the Portuguese Web Archive). So time is a first-class axis, and facets beat
  one long scrolling list as an archive grows. rustyweb has a faceted results
  page (collection, site, date, type, language), a month timeline, and grouping
  of repeat captures of the same URL - the faceted, full-text "slice and dice"
  browsing that [SHINE] (UK Web Archive) and [SolrWayback] (Royal Danish Library)
  established over the [warc-indexer]. rustyweb owes both a clear debt; it just
  trades their Solr backend for a single embedded Tantivy index.
- **Provenance is part of the record** - to trust and interpret an archive you
  need to know how it was made: the crawler software, operator, dates, and seeds
  (Maemura et al., *If These Crawls Could Talk*). rustyweb reads this from the
  WACZ and WARC and surfaces it on each collection and WACZ - and lets you
  verify each file's fixity - rather than burying it.

## Install

rustyweb is a single self-contained binary. You need a
[Rust toolchain](https://rustup.rs) (Rust 2021 / a recent stable compiler).

### With cargo (recommended)

```sh
cargo install --git https://github.com/edsu/rustyweb --locked rustyweb
```

This builds and installs the `rustyweb` command into `~/.cargo/bin`. The
ReplayWeb.page assets are embedded at build time, so there is nothing else to
fetch or configure.

### From a clone (for development)

```sh
git clone https://github.com/edsu/rustyweb
cd rustyweb
cargo build --release
# binary at ./target/release/rustyweb
```

The bundled ReplayWeb.page assets are committed to the repo, so a fresh clone
builds and runs as-is. To upgrade them later, run `./scripts/fetch-replay.sh`
and rebuild.

## How it works

rustyweb runs [ReplayWeb.page] in **WACZ-direct mode**. Rather than
reimplementing web replay on the server (URL rewriting, redirect handling,
fuzzy matching, serving individual archived resources), rustyweb hands the whole
job to the well-tested [wabac.js] service worker running in the browser:

```
 rustyweb index <files>                 rustyweb serve
        │                                      │
        ▼                                      ▼
  [ Indexing ]                        [ Axum HTTP server ]
        │                                      │
        ├── page HTML ──► Tantivy      GET /             homepage + collections
        ├── WACZ metadata ─► Tantivy   GET /search?q=    search results + snippets
        └── datapackage ─► collections GET /api/search   search results as JSON
                             .json      GET /files/{id}   the WACZ, with byte-range
                                        GET /replay/…     ReplayWeb.page assets + viewer
```

When you open a page for replay, the browser fetches the WACZ directly from
`GET /files/{id}` using HTTP range requests, reads the CDX index embedded inside
the WACZ, and serves every resource from the WARC records - all client-side.
rustyweb's job during replay is simply to serve bytes efficiently. Everything
else (search, metadata, the collection homepage) is what rustyweb is actually
good at.

See [DESIGN.md](DESIGN.md) for the full architecture.

## Quick start

rustyweb keeps everything under a **home directory** (default: the current
directory):

```
<home>/
├── archive/   your WACZ files
└── index/     search index + metadata (created by `rustyweb index`)
```

Keep your WACZ files in `archive/`, then index and serve:

```sh
mkdir -p archive
cp my-archive.wacz archive/

rustyweb index archive/*.wacz   # index the WACZs in your archive folder
rustyweb serve                  # http://127.0.0.1:8080
```

Local WACZ files must live under `<home>/archive` - rustyweb indexes them in
place (it does not copy them) and stores each source relative to home, so you
can move or copy the whole `<home>` directory to another disk or machine and it
still works. Point at a different home with `--home <DIR>` (every command takes
it).

`index` takes one or more archived WACZ files or `http(s)` URLs, so you can also
index a single file or a remote WACZ:

```sh
rustyweb index archive/my-archive.wacz
rustyweb index https://example.org/site.wacz
```

To rebuild the index later from what you've already indexed, use
[`rustyweb reindex`](#command-line) instead of re-listing everything.

Open <http://127.0.0.1:8080/>, search, and click a result to replay it.

(If you built from a clone instead of installing, use `./target/release/rustyweb`
in place of `rustyweb`.)

Re-indexing the same WACZ is an upsert - safe to re-run any time to add or
refresh collections.

### Remote WACZ files

A WACZ can also live at an `http(s)` URL. For example, this one is hosted on S3:

```sh
rustyweb index https://edsu-webarchives.s3.amazonaws.com/docnow.wacz
rustyweb serve
```

Indexing downloads the WACZ once to read its text and metadata, but records the
URL as the collection's source. At replay time the browser reads the remote WACZ
directly (via HTTP range requests) - rustyweb does not proxy the bytes. For that
to work the remote host must serve the WACZ with **HTTP range support and CORS**
allowing rustyweb's origin. The bucket above is configured that way
(`Accept-Ranges: bytes` and `Access-Control-Allow-Origin: *`).

This is also why S3 and other object stores work without any special support in
rustyweb: expose the object as a range- and CORS-capable HTTPS URL (a public
object like the one above, or a presigned URL) and index that.

## Searching

The search box matches page titles, headings, page text, descriptions, and
words from the page URL. A few things worth knowing (there's also a "Search
tips" panel in the app itself):

- **All words must match.** `climate policy` finds pages containing both words.
  Use `OR` for either (`climate OR weather`) and `-` to exclude (`climate -policy`).
- **Quotes** search an exact phrase: `"climate policy"`.
- **Field search**: `title:climate` matches only the title; `domain:example.com`
  restricts to pages from that exact host; `year:2021` (or `year:[2020 TO 2023]`)
  and `month:202103` (or `month:[202101 TO 202106]`) filter by crawl date;
  `type:pdf`, `lang:en`, and `collection:demo` filter by media type, language,
  and collection.
- **Grouping and boosting**: `(climate OR weather) risk`, and `climate^2 change`
  ranks "climate" matches higher.

Title matches rank above body matches, and searches are case-insensitive.

The results page is faceted: a sidebar shows counts by collection, year, site,
type, and language, and clicking one refines the search (applied filters appear
as removable chips). A month timeline sits above the results — click a bar to
filter to that month. Repeat captures of the same URL collapse into a single
result marked "captured N times", and results are paginated. The homepage also
offers "browse by year" and "top sites" entry points into search.

## Command line

```
rustyweb index           [--home <DIR>] [--name <NAME>] [--collection <NAME>] <PATH|URL>...
rustyweb reindex         [--home <DIR>]
rustyweb serve           [--home <DIR>] [--bind <ADDR>]
rustyweb collection set  [--home <DIR>] <COLLECTION> <WACZ_ID>...
rustyweb collection list [--home <DIR>]
rustyweb search-url      [--home <DIR>] <URL>
rustyweb verify          [--home <DIR>]
```

Every command takes `--home <DIR>` (default `.`); `archive/` and `index/` are
derived siblings under it.

- **`index`** - indexes one or more archived WACZ files or `http(s)://` URLs (at
  least one; a remote WACZ is downloaded to a temp file for indexing). A local
  WACZ must live under `<home>/archive`; rustyweb indexes it in place rather than
  copying it, and a path outside the archive folder (or a directory) is an error.
  Index several with a shell glob: `rustyweb index archive/*.wacz`. Extracts
  searchable text from each page (HTML, Browsertrix's rendered `urn:text`
  records, and PDFs), reads `datapackage.json` for collection metadata, and
  records everything in the manifest under `<home>/index/`, including the SHA-256
  of each WACZ. Local WACZ paths are stored relative to home so the folder is
  portable. The WACZ name comes from `--name` if given, otherwise the WACZ's
  `datapackage.json` title, otherwise the filename. `--collection <NAME>` groups
  the WACZs into a curated collection (created if new); without it each WACZ is
  its own collection.
- **`collection`** - `collection list` shows collections and their members;
  `collection set <COLLECTION> <WACZ_ID>...` moves WACZs into a collection.
- **`reindex`** - rebuild the search index from the WACZs already in the
  manifest, preserving collection membership and metadata. Re-fetches remote URL
  sources and recreates the index from scratch, so it's the way to migrate after
  an upgrade changes the index schema. (If you try to `index` or `serve` against
  an index built by an older version, rustyweb tells you to run this.)
- **`serve`** - opens the index read-only and starts the HTTP server (so you can
  `index` while it runs). Defaults to `127.0.0.1:8080`.
- **`search-url`** - a debugging aid: reads the CDX index *inside* each WACZ and
  prints the records matching a URL. No separate CDX store is maintained; the
  WACZ's own index is authoritative.
- **`verify`** - re-hashes every registered WACZ and compares against the
  SHA-256 recorded at index time, reporting each as `OK`, `MODIFIED`, or
  `MISSING`. Exits non-zero if any collection fails, so it works in a cron job
  or CI. This is rustyweb's fixity check - a small guard against the archive
  quietly bit-rotting or being tampered with.

## Testing

```sh
cargo test              # unit + integration tests (no browser needed)
```

Most tests run without a browser, including server-side *replay-contract* tests
that assert what wabac.js depends on: the WACZ we serve is byte-identical to
disk, byte-range requests return the correct slice, the served archive's CDX
resolves a page, and the viewer wires up `<replay-web-page>` correctly.

Actual replay rendering can only be checked in a real browser, so there's one
`#[ignore]`d end-to-end test that drives headless Chrome via WebDriver and
confirms an archived page renders from a WACZ we serve:

```sh
chromedriver --port=9515 &          # WebDriver server; must match your Chrome's major version
cargo test -p rustyweb-lib --test browser -- --ignored
```

- Override the WebDriver endpoint with `WEBDRIVER_URL` (default
  `http://localhost:9515`).
- `chromedriver`'s major version must match your installed Chrome. If they
  differ, grab a matching build from
  [Chrome for Testing](https://googlechromelabs.github.io/chrome-for-testing/).
- On macOS, a Homebrew `chromedriver` is quarantined and gets killed on launch;
  clear it once with `xattr -d com.apple.quarantine $(which chromedriver)`.

## Why "rustyweb"?

The tool is written in Rust, and (somewhat confusingly) there's a rusty-web
library on crates.io But the name rustyweb (no hyphen) is really a nod to an
older idea. In 2013 [Olivier Thereaux] gave a talk at [Paris Web] called
*"Esthétique et pratique du Web qui rouille"* - the aesthetics and practice of
**the web that rusts** - and gathered notes and references under the name
[rustyweb][rustyweb-orig]. It was an exploration of how web content ages,
decays, and transforms over time, and how we might redesign sites without
razing what came before.

A touchstone of that conversation is [Karl Dubost]'s essay
[*Un site web de 1000 ans*][1000ans] ("A 1000-year website"). Dubost argues that
we should build sites whose information is *allowed to become obsolete* rather
than destroyed - treating a website like an archive or a library, where content
follows a lifecycle from fresh to obsolete to historical. He makes the case for
durable URIs ("will this address still resolve in 50 years?"), dated URLs, and
using HTTP deliberately as a memory-management tool (`308 Permanent Redirect`,
`307 Temporary Redirect`, `410 Gone`).

This rustyweb is a small tool in service of that same idea: keep the archived
web readable, searchable, and replayable - let it rust gracefully, but keep
parts of it around.

## Credits

rustyweb stands almost entirely on the shoulders of [Webrecorder]. The hard
part - faithfully replaying an archived page in the browser - is done by their
[ReplayWeb.page] and [wabac.js] (which bundles wombat.js), both of which rustyweb
ships and serves unmodified. It also builds on the open [WACZ] format and the
broader web-archiving community. If rustyweb is useful to you, please support
Webrecorder's work.

## License

rustyweb is licensed under the **GNU Affero General Public License v3.0 or
later** (AGPL-3.0-or-later) - the same license as the ReplayWeb.page and
wabac.js components it bundles. See [LICENSE](LICENSE) for the full text and
[NOTICE](NOTICE) for third-party attributions and bundled-asset details.

[WACZ]: https://specs.webrecorder.net/wacz/latest/
[Webrecorder]: https://webrecorder.net/
[ReplayWeb.page]: https://replayweb.page/
[Paris Web]: https://www.paris-web.fr/
[Olivier Thereaux]: https://github.com/olivierthereaux
[rustyweb-orig]: https://github.com/olivierthereaux/rustyweb
[Karl Dubost]: https://www.la-grange.net/karl/
[1000ans]: https://www.24joursdeweb.fr/2012/un-site-web-de-1000-ans/
[wabac.js]: https://github.com/webrecorder/wabac.js
[reading room]: https://inkdroid.org/2026/06/03/jan6-doj-archive/
[SHINE]: https://github.com/ukwa/shine
[SolrWayback]: https://github.com/netarchivesuite/solrwayback
[warc-indexer]: https://github.com/ukwa/webarchive-discovery
