# rustyweb

**rustyweb** is a small, fast web archive server written in Rust. Point it at a
pile of [WACZ] files and it gives you:

- **Full-text search** across the archived pages, with hit-highlighted snippets
- **A homepage** that surfaces each collection's metadata (title, description,
  crawl date, seed pages)
- **In-browser replay** of the archived pages via [ReplayWeb.page] / wabac.js

It ships as a single self-contained binary — no Solr, no Elasticsearch, no
separate database server.

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
the WACZ, and serves every resource from the WARC records — all client-side.
rustyweb's job during replay is simply to serve bytes efficiently. Everything
else (search, metadata, the collection homepage) is what rustyweb is actually
good at.

See [DESIGN.md](DESIGN.md) for the full architecture.

## Quick start

rustyweb embeds the ReplayWeb.page assets at compile time, so fetch them once
before building:

```sh
./scripts/fetch-replay.sh        # downloads ui.js and sw.js into static/replay/
cargo build --release
```

Index one or more WACZ files (or a directory of them), then serve:

```sh
# Index — writes a search index and collection manifest into ./index
./target/release/rustyweb index my-archive.wacz

# Serve — defaults to http://127.0.0.1:8080
./target/release/rustyweb serve
```

Open <http://127.0.0.1:8080/>, search, and click a result to replay it.

Re-indexing the same WACZ is an upsert — safe to re-run any time to add or
refresh collections.

## Command line

```
rustyweb index      [--index-dir <DIR>] [--name <NAME>] <PATH>...
rustyweb serve      [--index-dir <DIR>] [--bind <ADDR>]
rustyweb search-url [--index-dir <DIR>] <URL>
rustyweb verify     [--index-dir <DIR>]
```

- **`index`** — accepts `.wacz` files or directories (scanned for `.wacz`).
  Extracts page HTML for full-text search, reads `datapackage.json` for
  collection metadata, and records everything in `{index-dir}/collections.json`
  — including the SHA-256 of each WACZ. Defaults to `./index`.
- **`serve`** — opens the index read-only and starts the HTTP server. Defaults
  to `127.0.0.1:8080`.
- **`search-url`** — a debugging aid: reads the CDX index *inside* each WACZ and
  prints the records matching a URL. No separate CDX store is maintained; the
  WACZ's own index is authoritative.
- **`verify`** — re-hashes every registered WACZ and compares against the
  SHA-256 recorded at index time, reporting each as `OK`, `MODIFIED`, or
  `MISSING`. Exits non-zero if any collection fails, so it works in a cron job
  or CI. This is rustyweb's fixity check — a small guard against the archive
  quietly bit-rotting or being tampered with.

## Why "rustyweb"?

The tool is written in Rust, but it's really a nod to an older idea. In 2013
[Olivier Thereaux] gave a talk at [Paris Web] called *"Esthétique et pratique du
Web qui rouille"* — the aesthetics and practice of **the web that rusts** — and
gathered notes and references under the name [rustyweb][rustyweb-orig]. It was an
exploration of how web content ages, decays, and transforms over time, and how
we might redesign sites without razing what came before.

A touchstone of that conversation is [Karl Dubost]'s essay
[*Un site web de 1000 ans*][1000ans] ("A 1000-year website"). Dubost argues that
we should build sites whose information is *allowed to become obsolete* rather
than destroyed — treating a website like an archive or a library, where content
follows a lifecycle from fresh to obsolete to historical. He makes the case for
durable URIs ("will this address still resolve in 50 years?"), dated URLs, and
using HTTP deliberately as a memory-management tool (`308 Permanent Redirect`,
`307 Temporary Redirect`, `410 Gone`).

This rustyweb is a small tool in service of that same idea: keep the archived
web readable, searchable, and replayable — let it rust gracefully, but keep
parts of it around.

[WACZ]: https://specs.webrecorder.net/wacz/latest/
[ReplayWeb.page]: https://replayweb.page/
[Paris Web]: https://www.paris-web.fr/
[Olivier Thereaux]: https://github.com/olivierthereaux
[rustyweb-orig]: https://github.com/olivierthereaux/rustyweb
[Karl Dubost]: https://www.la-grange.net/karl/
[1000ans]: https://www.24joursdeweb.fr/2012/un-site-web-de-1000-ans/
[wabac.js]: https://github.com/webrecorder/wabac.js
