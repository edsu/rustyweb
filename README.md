# rustyweb

**Note bene**: *rustyweb is alpha software and has been written extensively
with the support of Claude Code. Like any piece of software it may contain
bugs, and while the software was designed through several iterations and
abandoned prototypes, the developer's understanding of how it operates at a low
level may be limited. See the [DESIGN.md](DESIGN.md) document for the overall
approach that was used. Technical reviews of the code and design are always
welcome!*

---

**rustyweb** is a small, fast web archive server written in Rust. Point it at a
pile of [WACZ] files and it gives you:

- **Full-text search** across the archived pages, with hit-highlighted snippets
- **A homepage** that surfaces each collection's metadata (title, description,
  crawl date, seed pages)
- **In-browser replay** of the archived pages via [ReplayWeb.page] / wabac.js

It ships as a single self-contained binary - no Solr, no Elasticsearch, no
separate database server.

> **The web archive replay is entirely [Webrecorder]'s work.** rustyweb bundles
> and serves [ReplayWeb.page] and [wabac.js] - the browser-side engine that does
> all the actual replay - and adds a thin Rust layer for indexing, search, and
> serving. Webrecorder did the heavy lifting; please support them. See
> [Credits](#credits).

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

Put your WACZ files in `archive/`, then index and serve:

```sh
mkdir -p archive
cp my-archive.wacz archive/

rustyweb index      # indexes everything in ./archive
rustyweb serve      # http://127.0.0.1:8080
```

Point at a different home with `--home <DIR>` (both `index` and `serve` take
it). Because the archive and its index live together under one folder, you can
move or copy the whole `<home>` directory to another disk or machine and it
still works - the paths are stored relative to home.

You can also index specific files or a URL instead of the whole archive:

```sh
rustyweb index archive/my-archive.wacz
rustyweb index https://example.org/site.wacz
```

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

## Command line

```
rustyweb index      [--home <DIR>] [--name <NAME>] [<PATH|URL>...]
rustyweb serve      [--home <DIR>] [--bind <ADDR>]
rustyweb search-url [--home <DIR>] <URL>
rustyweb verify     [--home <DIR>]
```

Every command takes `--home <DIR>` (default `.`); `archive/` and `index/` are
derived siblings under it.

- **`index`** - with no path, indexes every `.wacz` under `<home>/archive`. Also
  accepts explicit `.wacz` files, directories (scanned for `.wacz`), or
  `http(s)://` URLs (the remote WACZ is downloaded to a temp file for indexing).
  Extracts searchable text from each page (HTML, Browsertrix's rendered
  `urn:text` records, and PDFs), reads `datapackage.json` for collection
  metadata, and records everything in `<home>/index/collections.json`, including
  the SHA-256 of each WACZ. Local WACZ paths are stored relative to home so the
  folder is portable. The collection name comes from `--name` if given,
  otherwise the WACZ's `datapackage.json` title, otherwise the filename.
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
