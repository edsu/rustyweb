# rustyweb

[![CI](https://github.com/edsu/rustyweb/actions/workflows/ci.yml/badge.svg)](https://github.com/edsu/rustyweb/actions/workflows/ci.yml)

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
separate database server. That's a deliberate design goal: rustyweb is built for
**small, local, and private** use - a person indexing a handful of their own
WACZ files on a laptop, with nothing sent to a hosted service - while using the
same model to **scale up** toward institutional collections. It aims to fit both
ends of that range, rather than assuming the infrastructure of a large web
archive.

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
  trades their Solr backend for a single embedded Tantivy index - so the same
  faceted search runs with no cluster to operate, fitting a private laptop
  archive as readily as an institutional one.
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
├── archive/<slug>/     your WACZ files, organized by collection
├── collections/<slug>/ finding aids you author + commit (README.md, thumbnails, notes)
└── index/              search index + derived metadata (rebuildable; git-ignore it)
```

The `collections/` folder is the part worth keeping in version control - the prose
and images a curator writes. `index/` is derived from the WACZs and rebuilt by
`rustyweb reindex`, so a home in git typically `.gitignore`s `/index`.

Index one or more WACZ files into a collection, then serve:

```sh
rustyweb index my-archive.wacz --collection "My Web Archive"   # files it into archive/my-web-archive/
rustyweb serve                                                 # http://127.0.0.1:8080
```

Every crawl belongs to a **collection**, so `index` requires `--collection <NAME>`
(created if new). This is a deliberate nudge to say what a crawl is a part of and
why you're keeping it — the curatorial context rustyweb is built to surface.
Describe a collection further (creator, dates, rights, a scope note) with
[`rustyweb collection set`](#command-line), which writes a git-committable finding
aid at `collections/<slug>/README.md`.

A local WACZ can live anywhere - `rustyweb index path/to/foo.wacz --collection
"Bar"` files it into `<home>/archive/bar/` for you (**moving** it if it's already
under `archive/`, **copying** it otherwise, so your original is left intact). The
source is stored relative to home, so you can move or copy the whole `<home>`
directory to another disk or machine and it still works. Point at a different home
with `--home <DIR>` (every command takes it).

`index` takes one or more archived WACZ files or `http(s)` URLs, so you can also
index a single file or a remote WACZ:

```sh
rustyweb index archive/my-archive.wacz --collection "My Web Archive"
rustyweb index https://example.org/site.wacz --collection "My Web Archive"
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
rustyweb index https://edsu-webarchives.s3.amazonaws.com/docnow.wacz --collection "DocNow"
rustyweb serve
```

By default rustyweb **streams** a remote WACZ - it never downloads the whole
file. Using the WACZ's internal CDX index, it reads (via HTTP range requests)
only the pieces it needs - the ZIP central directory, the CDX, and the HTML/PDF
page records - and skips images, video, JS, and CSS entirely. On a media-heavy
archive that's a tiny fraction of the file: a 323 MB WACZ can be indexed in a
few seconds without writing anything to disk. The URL is recorded as the
source, and at replay time the browser reads the remote WACZ directly (also via
range requests) - rustyweb never proxies the bytes.

For this to work the remote host must serve the WACZ with **HTTP range support
and CORS** allowing rustyweb's origin. The S3 bucket above is configured that
way (`Accept-Ranges: bytes` and `Access-Control-Allow-Origin: *`), which is why
S3 and other object stores work with no special support - expose the object as a
range- and CORS-capable HTTPS URL (public or presigned) and index it.

If you'd rather keep a **local copy**, add `--download`:

```sh
rustyweb index --download https://edsu-webarchives.s3.amazonaws.com/docnow.wacz --collection "DocNow"
```

This fetches the WACZ into `<home>/archive`, indexes it as a local file, and
records a whole-file SHA-256 - a durable copy you can replay offline and check
with `rustyweb verify`. rustyweb also falls back to downloading automatically if
a remote host doesn't support range requests, or if the WACZ stores its WARCs
compressed (the WACZ spec says the `archive/` WARCs *should* be stored
uncompressed so they can be read by range; a few tools don't).

Streaming a large remote WACZ makes one HTTP range request per page record. Those
requests are latency-bound and independent, so rustyweb fetches them concurrently
(4 at a time by default — gentle on arbitrary hosts; raise it, e.g.
`--concurrency 16`, for object stores like S3). Fetches
retry transient failures (rate limits and `5xx`) with backoff, honoring
`Retry-After`, so a long ingest survives blips and stays gentle on the host - be
mindful that a high `--concurrency` all hits a single host, so dial it down for
small servers (it's fine for object stores like S3). As a backstop the worker
count is capped at 64 per host, so a mis-typed value can't flood a single server. `index` shows a progress
bar - a spinner while it reads the CDX, then a bar with the throughput and an ETA
once it knows how many records there are - so you can see it working. Add
`-v`/`--verbose` for detailed logs instead of the bar; when output isn't a
terminal (piping to a file or CI) it prints plain log lines and no bar.

### How indexing reads a WACZ

By default rustyweb reads a WACZ through its internal **CDX index**, fetching only
the records that become pages (HTML, PDFs, and Browsertrix's rendered `urn:text`)
and skipping images, video, JS, and CSS. It also reads the fully rendered page
text from `pages/pages.jsonl` and `pages/extraPages.jsonl` — many crawls store the
post-JS text only there, so this keeps JS-rendered content searchable, not just
visible in replay. This works the same way for local and
remote WACZs - the only difference is *how* the bytes are read: a remote WACZ over
HTTP range requests (no download), a local WACZ straight from the file.

It falls back to a **full scan** of every WARC record only when a WACZ can't be
read via its CDX - its WARCs are stored compressed (the WACZ spec says the
`archive/` WARCs *should* be stored uncompressed so they can be read by offset; a
few tools don't), or it has no readable CDX. For a remote WACZ whose host doesn't
support range requests, the fallback downloads a temporary copy and scans it.

rustyweb trusts the CDX because **replay already does**: the in-browser player
resolves each record through the CDX, so a WACZ with a broken CDX wouldn't replay
anyway. Indexing from the same index keeps the two consistent.

## Searching

The search box matches page titles, headings, body text, descriptions,
keywords, author, and words from the page URL. A few things worth knowing
(there's also a "Search tips" panel in the app itself):

- **All words must match.** `climate policy` finds pages containing both words.
  Use `OR` for either (`climate OR weather`) and `-` to exclude (`climate -policy`).
- **Quotes** search an exact phrase: `"climate policy"`.
- **Field search**: `title:climate` and `author:hopper` match those fields;
  `site:example.com` matches a whole site across subdomains while
  `domain:www.example.com` is an exact host; `year:2021` (or `year:[2020 TO 2023]`),
  `month:202103`, and `modified:2015` (Last-Modified year) filter by date;
  `type:pdf`, `lang:en`, `status:200`, and `collection:demo` filter by media
  type, language, HTTP status, and collection.
- **Grouping and boosting**: `(climate OR weather) risk`, and `climate^2 change`
  ranks "climate" matches higher.

Title matches rank above body matches, and searches are case-insensitive.

The results page is faceted: a sidebar shows counts by collection, year, site,
type, and language, and clicking one refines the search (applied filters appear
as removable chips). A month timeline sits above the results — click a bar to
filter to that month. Repeat captures of the same URL collapse into a single
result marked "captured N times", and results are paginated. The homepage also
offers "browse by year" and "top sites" entry points into search.

Crawls carry a representative image, cached as a small thumbnail at index time.
It's taken from the crawl's home-page `og:image`; failing that, the largest
content image the page embeds; and failing *that* — for JS-rendered sites whose
saved HTML lists no images — the largest captured image on the crawl's own
domain (skipping icons/sprites and full-res originals).
Homepage collection cards and the crawl detail page show one; the collection
detail page shows a grid of its member crawls, each with its own image —
conveying that a collection spans multiple crawls of multiple sites. Crawls
without an image fall back to a CSS placeholder. A curator can pin a specific
image with `rustyweb crawl set <crawl-id> --image <file>` (kept across reindexing).

## Importing from Browsertrix

If your WACZs live in a [Browsertrix](https://browsertrix.com/) account
(Webrecorder's hosted crawler), `rustyweb import browsertrix` downloads them into
`<home>/archive` and indexes them as durable local sources.

Credentials come from the **environment**, never the command line, so they don't
show up in the process list:

```sh
export BROWSERTRIX_USER='you@example.org'
read -rs BROWSERTRIX_PASSWORD; export BROWSERTRIX_PASSWORD   # prompts, no echo
# or, instead of user/password:  export BROWSERTRIX_TOKEN='<a JWT>'
```

Then import - preview first with `--dry-run`, then pull for real:

```sh
# everything you've QA'd in your (only) org, into ~/webarchive
rustyweb import browsertrix --home ~/webarchive --dry-run
rustyweb import browsertrix --home ~/webarchive

# just one collection (by id, slug, or name) → a matching rustyweb collection
rustyweb import browsertrix --collection us-govarchive --home ~/webarchive

# a single crawl
rustyweb import browsertrix --crawl <item-id> --home ~/webarchive
```

Notes:

- **QA'd crawls only, by default.** Browsertrix lets a reviewer rate a crawl
  (`reviewStatus`); rustyweb imports only reviewed crawls so you publish vetted
  content. Add `--include-unreviewed` to import everything, or `--min-review <1-5>`
  for a rating threshold. A single named `--crawl` is always imported. When crawls
  are skipped for this reason, rustyweb says so.
- **Selection.** `--collection <ID|SLUG|NAME>` limits to one Browsertrix
  collection; `--crawl <ID>` to a single archived item; neither imports the whole
  org. `--org <SLUG>` picks the org when your account has more than one.
- **Incremental.** Re-running skips crawls already imported (matched by content
  hash), so syncing an account is cheap; `--force` re-imports anyway.
- **Durable by default.** WACZs are downloaded into `<home>/archive/<item-id>/`
  (a subfolder per Browsertrix item, so items can't clash on a shared filename),
  because Browsertrix's presigned URLs expire after ~48h - a downloaded copy keeps
  replay working long-term. `--host <URL>` targets a self-hosted Browsertrix
  (default is `https://app.browsertrix.com`).
- **`--stream` (index-only footprint).** Instead of downloading, index the WACZ
  in place from Browsertrix and store only its stable identity, not a copy. Since
  presigned URLs expire, rustyweb re-resolves a fresh one on demand — so **`serve`
  needs the same `BROWSERTRIX_*` credentials** to replay these crawls (they show a
  503 otherwise). Good for a self-hoster who wants search over their own crawls
  without keeping the bytes; download (the default) is better for a durable,
  offline, or shared library.
- **Grouping.** Importing a `--collection` groups its crawls into a rustyweb
  collection of the same name. `--into <NAME>` overrides that name (and is the way
  to group an org-wide or single-`--crawl` import, which otherwise land as
  individual collections). `--limit <N>` caps how many are imported; `--dry-run`
  lists them without downloading.

## Command line

```
rustyweb index           [--home <DIR>] [--name <NAME>] --collection <NAME> [-f|--from-file <FILE>] [--download] [--concurrency <N>] [-v|--verbose] <PATH|URL>...
rustyweb reindex         [--home <DIR>] [--concurrency <N>] [-v|--verbose]
rustyweb optimize        [--home <DIR>] [--max-segments <N>] [-v|--verbose]
rustyweb serve           [--home <DIR>] [--bind <ADDR>]
rustyweb collection set  [--home <DIR>] <NAME> [--creator <TEXT>] [--dates <TEXT>] [--rights <TEXT>] [--subject <SUBJECT>]... [--narrative <MD> | --narrative-file <FILE>] [--thumbnail <FILE>] [--description <TEXT>] [--curator <TEXT>]
rustyweb collection list [--home <DIR>]
rustyweb crawl set       [--home <DIR>] <CRAWL_ID> [--image <FILE>] [--note <MD> | --note-file <FILE>]
rustyweb search-url      [--home <DIR>] <URL>
rustyweb verify          [--home <DIR>]
rustyweb import browsertrix [--home <DIR>] [--host <URL>] [--org <SLUG>] [--collection <ID|SLUG>] [--crawl <ID>] [--into <NAME>] [--include-unreviewed] [--min-review <N>] [--limit <N>] [--dry-run] [--stream] [--force] [-v]
```

Every command takes `--home <DIR>` (default `.`); `archive/` and `index/` are
derived siblings under it.

- **`index`** - indexes one or more archived WACZ files or `http(s)://` URLs (at
  least one). By default rustyweb reads a WACZ through its internal **CDX index**,
  extracting only the page records (and falling back to a full WARC scan only when
  a WACZ can't be read that way - see [How indexing reads a
  WACZ](#how-indexing-reads-a-wacz)). A remote URL is **streamed** over HTTP range
  requests, no download (see [Remote WACZ files](#remote-wacz-files)). A local
  WACZ may live anywhere - rustyweb files it into `<home>/archive/<slug>/`
  (moving it if already under `archive/`, else copying it), and a directory or
  non-`.wacz` path is an error. Index several with a shell glob. Extracts
  searchable text from each page (HTML, Browsertrix's rendered `urn:text` records
  or `pages/*.jsonl` text, and PDFs), reads `datapackage.json` for collection
  metadata, and records
  everything in the manifest under `<home>/index/`, including the SHA-256 of each
  local WACZ. Local WACZ paths are stored relative to home so the folder is
  portable. The WACZ name comes from `--name` if given, otherwise the WACZ's
  `datapackage.json` title, otherwise the filename. **`--collection <NAME>` is
  required** — every crawl belongs to a curated collection (created if new); there
  are no auto singletons. `--download` fetches a remote WACZ into
  `<home>/archive/<collection-slug>/` for a
  durable local copy instead of streaming it in place. To index many at once, pass
  a newline-delimited list of files/URLs with `--from-file <FILE>` (or `-f -` to
  read from stdin); blank lines and `#` comments are ignored, and it combines with
  any positional args. `--concurrency <N>` sets how many records are fetched at
  once during CDX-guided (streaming) indexing (default: 4 for remote URLs — gentle
  on the host, raise for object stores like S3; CPU count for local files; capped
  at 64 per host). Indexing shows a progress bar on an interactive terminal;
  `-v`/`--verbose` replaces it with debug logs. A **multi-WACZ** (a WACZ that
  bundles other WACZs, e.g. a Browsertrix combined-collection download) is
  detected automatically and its inner crawls indexed too, into one entry.
- **`collection`** - `collection list` shows collections and their members;
  `collection set <NAME> …` writes a collection's finding-aid metadata (creator,
  dates, rights, subjects, a Markdown narrative, and an optional `--thumbnail`) to
  a git-committable `collections/<slug>/README.md` you can also hand-edit.
  `crawl set <ID> --note` adds a per-crawl Markdown note
  (`collections/<slug>/crawls/<id>.md`); `crawl set <ID> --image` pins a crawl
  thumbnail there too. (WACZ→collection membership is set when indexing, via
  `index --collection <NAME>`.)
- **`reindex`** - rebuild the search index from the WACZs already in the
  manifest, preserving collection membership and metadata. Re-fetches remote URL
  sources and recreates the index from scratch, so it's the way to migrate after
  an upgrade changes the index schema. It's resilient: a source that can't be
  indexed (a missing local file, or a remote source still failing after retries)
  is skipped with a warning rather than aborting the rebuild; the mostly-rebuilt
  index is still usable, and if anything was skipped the command exits non-zero
  with a summary count so you (or cron/CI) know to re-run it once fixed. Takes
  `--concurrency <N>` and shows the same progress bar as `index` (a full reindex
  re-streams every source, so it can take a while); `-v`/`--verbose` swaps the bar
  for debug logs. (If you try to `index` or `serve` against an index built by an
  older version, rustyweb tells you to run this.)
- **`optimize`** - compacts the search index by merging its Tantivy *segments*
  down toward `--max-segments` (default 8), **without re-fetching sources** — so
  it's much cheaper than `reindex`. Every search fans out across all segments, so
  an index that has fragmented into hundreds of tiny segments (which happens when
  Tantivy's background merges fail — classically on a full disk) gets slow;
  `optimize` merges them back down. A lower `--max-segments` compacts more but
  needs more free disk during the merge (roughly index size ÷ target). Reports the
  `before → after` segment count.
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
- **`import browsertrix`** - imports WACZ files from a [Browsertrix](https://browsertrix.com/)
  instance (Webrecorder's hosted crawler) - the "index your own crawls" path.
  See [Importing from Browsertrix](#importing-from-browsertrix).

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
