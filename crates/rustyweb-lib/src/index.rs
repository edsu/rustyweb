use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::{debug, info};

use crate::collections::{Manifest, Source, Wacz, file_sha256, wacz_id};
use crate::search::{SearchIndex, extract_html_text};
use crate::warc::{Warcinfo, WarcRecord, iter_records};
use crate::wacz::{extract_warc_from_wacz, iter_warc_paths, read_datapackage};

/// Paths derived from a rustyweb home directory.
pub fn index_dir(home: &Path) -> PathBuf {
    home.join("index")
}
pub fn archive_dir(home: &Path) -> PathBuf {
    home.join("archive")
}

/// Progress sink for indexing, implemented by the binary (e.g. with a progress
/// bar). The library stays UI- and dependency-free: it only reports counts.
/// Streaming a remote WACZ can be slow (each page record is a separate HTTP
/// range request, and reading the CDX up front takes a moment), so this makes
/// both the setup and the per-record work visible.
///
/// Lifecycle per WACZ: `begin` once → optionally `set_total` then `set_records*`
/// (streaming path, where a record count is known) → `finish` once.
pub trait IndexProgress: Sync {
    /// Work on a WACZ has begun. The record total isn't known yet (the ZIP
    /// directory and CDX must be read first), so this is the cue for an
    /// indeterminate spinner. `label` is the WACZ URL or path.
    fn begin(&self, label: &str);
    /// Describe the current setup activity (e.g. "downloading", "reading index"),
    /// so the spinner reflects what's actually happening before the record total
    /// is known.
    fn phase(&self, phase: &str);
    /// The CDX has been read: `total` page records will be streamed. Cue to
    /// switch the spinner to a determinate bar.
    fn set_total(&self, total: u64);
    /// `done` of the current WACZ's page records have been fetched.
    fn set_records(&self, done: u64);
    /// Work on the current WACZ finished (clear the spinner/bar).
    fn finish(&self);
}

/// Index a local WACZ file (which must live under `<home>/archive`) under the
/// given home dir. Thin wrapper over [`index_location`].
pub fn index_path(path: &Path, home: &Path, name: Option<&str>) -> Result<()> {
    index_location(&path.to_string_lossy(), home, name, None, false, false, None)
}

/// Index a WACZ from a location into the home directory's `index/`. The location
/// is either a local `.wacz` file that already lives under `<home>/archive`, or
/// a remote `http(s)://` URL (downloaded to a temp file for indexing).
///
/// Local WACZ paths are stored relative to `home`, so the home folder (archive +
/// index together) is portable. rustyweb does not copy files for you: a local
/// WACZ outside the archive folder, a directory, or a non-`.wacz` path is an
/// error (see [`resolve_sources`]).
///
/// Idempotent: re-indexing the same source upserts its manifest entry and
/// replaces its documents in Tantivy.
/// `name` overrides the collection display name; otherwise it comes from the
/// WACZ metadata, falling back to the filename/URL stem.
pub fn index_location(
    location: &str,
    home: &Path,
    name: Option<&str>,
    collection: Option<&str>,
    // Force CDX (streaming) extraction for a local file. Remote URLs stream by
    // default.
    stream: bool,
    // Download a remote WACZ into <home>/archive and index it as a local file
    // instead of streaming.
    download: bool,
    // Optional progress sink for streaming indexes (the binary renders a bar).
    progress: Option<&dyn IndexProgress>,
) -> Result<()> {
    // Validate the argument first (a bad path errors before we touch the index).
    let sources = resolve_sources(location, home)?;

    let index_dir = index_dir(home);
    std::fs::create_dir_all(&index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;

    let search = Mutex::new(
        SearchIndex::open(index_dir.join("full_text").as_path())
            .with_context(|| format!("opening search index at {}", index_dir.display()))?,
    );

    let mut manifest = Manifest::open(&index_dir)?;

    // Resolve `--collection NAME` to (id, name); `None` => a singleton per WACZ.
    let group = collection.map(|cn| (crate::collections::slugify(cn), cn.to_string()));

    for source in &sources {
        let c = group.as_ref().map(|(id, n)| (id.as_str(), n.as_str()));
        index_one(source, home, &mut manifest, &search, name, c, stream, download, progress)?;
    }

    search.into_inner().unwrap().commit()?;
    manifest.save()?;

    Ok(())
}

/// Rebuild the full-text index from the sources already recorded in
/// `collections.json`, preserving the manifest (including each collection's
/// display name).
///
/// Unlike [`index_location`] (which scans `<home>/archive`), this re-indexes
/// every registered source - including remote URLs, which are re-fetched - and
/// recreates the Tantivy index from scratch, so a schema change is picked up.
/// Local files that have gone missing are skipped with a warning rather than
/// aborting the whole run.
pub fn reindex(home: &Path) -> Result<()> {
    let index_dir = index_dir(home);
    let mut manifest = Manifest::open(&index_dir)?;
    if manifest.waczs.is_empty() {
        info!("no WACZs registered; nothing to reindex");
        return Ok(());
    }

    // Snapshot each WACZ (source, name, collection id + name) before upserting
    // back, so its collection membership and the collection's metadata survive.
    let targets: Vec<(Source, String, String, String)> = manifest
        .waczs
        .iter()
        .map(|w| {
            let coll_name = manifest
                .collection_by_id(&w.collection)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| w.name.clone());
            (w.source.clone(), w.name.clone(), w.collection.clone(), coll_name)
        })
        .collect();

    // Drop the old full-text index so it is recreated with the current schema.
    let full_text = index_dir.join("full_text");
    if full_text.exists() {
        std::fs::remove_dir_all(&full_text)
            .with_context(|| format!("removing stale index at {}", full_text.display()))?;
    }
    let search = Mutex::new(
        SearchIndex::open(full_text.as_path())
            .with_context(|| format!("creating search index at {}", index_dir.display()))?,
    );

    let total = targets.len();
    let mut done = 0usize;
    for (source, name, collection_id, collection_name) in &targets {
        // Skip local files that no longer exist rather than failing the run;
        // their manifest entry is preserved (see `rustyweb verify`).
        if !source.is_url() {
            match source.resolve(home) {
                Some(p) if p.exists() => {}
                _ => {
                    tracing::warn!(source = %source.location(), "skipping missing local WACZ");
                    continue;
                }
            }
        }
        info!(
            source = %source.location(),
            progress = format!("{}/{}", done + 1, total),
            "reindexing"
        );
        // Fail fast, but say which collection failed and make clear the index is
        // now partially rebuilt (the old one was already dropped) so the user
        // knows to fix the cause and run reindex again. Membership is preserved
        // by re-supplying each WACZ's existing collection.
        index_one(
            source,
            home,
            &mut manifest,
            &search,
            Some(name),
            Some((collection_id, collection_name)),
            false,
            false,
            None,
        )
        .with_context(|| {
            format!(
                "reindexing collection \"{}\" ({}) failed after {}/{} done; \
                 the search index is now incomplete - fix the problem and run \
                 `rustyweb reindex` again",
                name,
                source.location(),
                done,
                total,
            )
        })?;
        done += 1;
    }

    search.into_inner().unwrap().commit()?;
    manifest.save()?;
    info!(reindexed = done, total, "reindex complete");
    Ok(())
}

/// Create or update a collection's curatorial metadata (its id is the slug of
/// `name`). Only the provided fields change. Returns the collection id.
pub fn set_collection(
    home: &Path,
    name: &str,
    description: Option<String>,
    curator: Option<String>,
) -> Result<String> {
    let index_dir = index_dir(home);
    std::fs::create_dir_all(&index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;
    let mut manifest = Manifest::open(&index_dir)?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let id = manifest.set_collection(name, description, curator, &now);
    manifest.save()?;
    info!(collection = %id, "collection metadata updated");
    Ok(id)
}

/// Turn one `index` argument into a source to index. An `http(s)://` URL yields
/// a URL source. Otherwise the argument must be a `.wacz` file that already lives
/// under `<home>/archive` - rustyweb keeps local archives there so the home
/// directory is self-contained and portable, and does not copy files for you.
/// Directories, non-`.wacz` paths, and files outside the archive folder are
/// errors with guidance.
fn resolve_sources(location: &str, home: &Path) -> Result<Vec<Source>> {
    match Source::parse(location) {
        url @ Source::Url(_) => Ok(vec![url]),
        Source::File(p) => Ok(vec![resolve_archive_file(&p, home)?]),
    }
}

/// Validate a local WACZ argument and return it as a home-relative [`Source`].
fn resolve_archive_file(path: &Path, home: &Path) -> Result<Source> {
    if path.is_dir() {
        anyhow::bail!(
            "{} is a directory; pass individual .wacz files instead \
             (e.g. `rustyweb index archive/*.wacz`)",
            path.display()
        );
    }
    if path.extension().and_then(|e| e.to_str()) != Some("wacz") {
        anyhow::bail!("{} is not a .wacz file or an http(s) URL", path.display());
    }
    if !path.exists() {
        anyhow::bail!("{} does not exist", path.display());
    }

    let abs = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    let archive = archive_dir(home);
    let in_archive = archive
        .canonicalize()
        .map(|a| abs.starts_with(&a))
        .unwrap_or(false);
    if !in_archive {
        anyhow::bail!(
            "{} is not inside the archive folder {}\n\
             rustyweb only indexes local WACZ files kept in the archive folder, so the \
             home directory stays self-contained. Move the file into {} and index it \
             from there, or pass an http(s) URL instead.",
            abs.display(),
            archive.display(),
            archive.display()
        );
    }

    Ok(Source::for_file(&abs, home))
}

/// Index a single WACZ source: obtain a local readable copy (downloading a URL
/// to a temp file), index its pages and metadata, and upsert its manifest entry.
#[allow(clippy::too_many_arguments)]
fn index_one(
    source: &Source,
    home: &Path,
    manifest: &mut Manifest,
    search: &Mutex<SearchIndex>,
    name: Option<&str>,
    // The collection (id, display name) this WACZ joins. `None` makes the WACZ
    // its own singleton collection (id == WACZ id, name == WACZ name).
    collection: Option<(&str, &str)>,
    // Force CDX (streaming) extraction (for a local file). Remote URLs stream by
    // default regardless.
    stream: bool,
    // Download a remote WACZ into <home>/archive and index it as a local file
    // (durable copy, whole-file fixity, offline replay) instead of streaming.
    download: bool,
    // Optional progress sink for streaming indexes.
    progress: Option<&dyn IndexProgress>,
) -> Result<()> {
    // Show an indeterminate spinner from the very start: the setup work (probing
    // the host, downloading, reading the ZIP directory and CDX) happens before
    // any record total is known, and can take many seconds on a large remote
    // WACZ. The streaming path later calls `set_total` to switch to a bar.
    if let Some(p) = progress {
        p.begin(&source.location());
    }

    // Resolve how the WACZ is read (`.3` mode logic):
    //  - A local File source: read in place.
    //  - A remote URL + --download: fetch into <home>/archive and adopt as a
    //    local File source (durable/offline; the recorded source becomes local).
    //  - A remote URL (default): stream over HTTP range requests, no download —
    //    but only if its WARCs are Stored; if they're deflated, fall back to a
    //    temp download + scan (keeping the URL as the source).
    let effective_source: Source = match source {
        Source::Url(u) if download => {
            info!(url = %u, "downloading remote WACZ into archive");
            if let Some(p) = progress {
                p.phase("downloading");
            }
            Source::File(download_into_archive(u, home)?)
        }
        _ => source.clone(),
    };

    let mut _tmp: Option<tempfile::NamedTempFile> = None;
    let (remote_url, local): (Option<String>, Option<PathBuf>) = match &effective_source {
        Source::File(_p) => (None, Some(effective_source.resolve(home).unwrap())),
        Source::Url(u) => {
            // Stream by default, but only if the host supports range requests and
            // the WARCs are Stored. Otherwise (no range, deflated WARCs, or a
            // probe error) fall back to downloading a temp copy and scanning it,
            // keeping the URL as the source.
            if remote_warcs_streamable(u).unwrap_or(false) {
                (Some(u.clone()), None)
            } else {
                info!(url = %u, "remote WACZ can't be streamed (no range support or compressed WARCs); downloading to index");
                if let Some(p) = progress {
                    p.phase("downloading");
                }
                let tmp = download_to_temp(u).with_context(|| format!("downloading {u}"))?;
                let p = tmp.path().to_path_buf();
                _tmp = Some(tmp);
                (None, Some(p))
            }
        }
    };
    // Stream a local file only when explicitly asked (--stream on a File source).
    let stream_local = stream && matches!(&effective_source, Source::File(_));

    let id = wacz_id(&effective_source);

    // Read metadata from the WACZ datapackage.json up front so its title can
    // name the collection. Precedence: explicit --name, then the WACZ title,
    // then the filename/URL stem.
    let meta = match &remote_url {
        Some(u) => crate::wacz::read_datapackage_from(crate::http_range::open_remote(u)?)
            .unwrap_or_default(),
        None => read_datapackage(local.as_ref().unwrap()).unwrap_or_default(),
    };
    let display_name = name
        .map(|n| n.to_string())
        .or_else(|| meta.title.clone().filter(|t| !t.trim().is_empty()))
        .unwrap_or_else(|| source_display_name(&effective_source));

    // Resolve the curated collection this WACZ joins: the one given, else a
    // singleton of its own (id == WACZ id, name == WACZ name).
    let (collection_id, collection_name) = match collection {
        Some((cid, cname)) => (cid.to_string(), cname.to_string()),
        None => (id.clone(), display_name.clone()),
    };

    // Drop this WACZ's prior documents so re-indexing upserts, not appends.
    search.lock().unwrap().delete_collection(&id);

    // Index pages, tagging each with this WACZ (id/name) and its collection. The
    // pass also collects provenance and capture stats (no re-read of the WARCs).
    // `--stream` uses CDX-guided extraction over the file; the default scans
    // every WARC record.
    let stats = match &remote_url {
        Some(u) => {
            info!(url = %u, "streaming remote WACZ index (no download)");
            let reader = crate::http_range::open_remote(u)?;
            index_wacz_streaming(reader, &id, &display_name, &collection_id, search, u, progress)?
        }
        None if stream_local => {
            let p = local.as_ref().unwrap();
            let file = std::fs::File::open(p)
                .with_context(|| format!("opening {} for streaming index", p.display()))?;
            index_wacz_streaming(file, &id, &display_name, &collection_id, search, &p.display().to_string(), progress)?
        }
        None => index_wacz(local.as_ref().unwrap(), &id, &display_name, &collection_id, search)?,
    };

    // Index the WACZ's metadata as a searchable document, tagged with its collection.
    let coll_body = build_collection_body(&meta);
    search
        .lock()
        .unwrap()
        .index_collection(&id, &display_name, &collection_id, &coll_body)?;

    // Fixity: a streamed remote is never fully read, so there's no whole-file
    // SHA-256 (empty; `verify` already skips remote sources). Its size comes
    // from the HTTP Content-Length. A local/downloaded file is hashed as before.
    let (sha, file_size) = match &remote_url {
        Some(u) => (String::new(), crate::http_range::open_remote(u)?.total_len()),
        None => {
            let p = local.as_ref().unwrap();
            let sha = file_sha256(p).with_context(|| format!("computing sha256 of {}", p.display()))?;
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            (sha, size)
        }
    };
    let date_indexed = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Provenance: collect the software reported by the datapackage and by the
    // warcinfo record (deduped) - we don't label which crawled vs packaged.
    // operator/user-agent/robots come from warcinfo when present.
    let warcinfo = stats.warcinfo.unwrap_or_default();
    let mut software: Vec<String> = Vec::new();
    for s in meta.software.into_iter().chain(warcinfo.software) {
        if !software.contains(&s) {
            software.push(s);
        }
    }
    manifest.ensure_collection(&collection_id, &collection_name, &date_indexed);
    manifest.upsert_wacz(Wacz {
        id,
        collection: collection_id,
        source: effective_source.clone(),
        name: display_name,
        date_indexed,
        file_size,
        sha256: sha,
        description: meta.description,
        crawl_date: meta.created,
        seed_pages: meta.seed_pages,
        software,
        operator: warcinfo.operator,
        user_agent: warcinfo.user_agent,
        robots: warcinfo.robots,
        page_count: Some(stats.pages),
        capture_start: stats.earliest_capture,
        capture_end: stats.latest_capture,
    });

    if let Some(p) = progress {
        p.finish();
    }
    Ok(())
}

/// Download a remote WACZ to a temp file for indexing.
fn download_to_temp(url: &str) -> Result<tempfile::NamedTempFile> {
    use std::io::{copy, Write};

    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("HTTP GET {url}"))?;
    let mut tmp = tempfile::Builder::new().suffix(".wacz").tempfile()?;
    let mut reader = resp.into_reader();
    copy(&mut reader, &mut tmp).with_context(|| format!("writing {url} to temp file"))?;
    tmp.flush()?;
    Ok(tmp)
}

/// Download a remote WACZ into `<home>/archive/<name>.wacz` and return its path
/// relative to `home` (so the manifest stores a portable local source). The name
/// comes from the URL's last path segment. Used by `--download`.
fn download_into_archive(url: &str, home: &Path) -> Result<PathBuf> {
    use std::io::{copy, Write};

    let stem = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("download");
    let name = if stem.ends_with(".wacz") { stem.to_string() } else { format!("{stem}.wacz") };

    let archive = archive_dir(home);
    std::fs::create_dir_all(&archive)
        .with_context(|| format!("creating archive dir {}", archive.display()))?;
    let dest = archive.join(&name);

    let resp = ureq::get(url).call().with_context(|| format!("HTTP GET {url}"))?;
    let mut file = std::fs::File::create(&dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    copy(&mut resp.into_reader(), &mut file)
        .with_context(|| format!("writing {url} to {}", dest.display()))?;
    file.flush()?;

    Ok(PathBuf::from("archive").join(&name))
}

/// Whether a remote WACZ can be stream-indexed: reachable, range-capable, and
/// its WARC entries Stored (uncompressed). Reads only the ZIP central directory.
fn remote_warcs_streamable(url: &str) -> Result<bool> {
    let reader = crate::http_range::open_remote(url)?;
    let mut zip = zip::ZipArchive::new(reader)
        .with_context(|| format!("reading remote ZIP central directory of {url}"))?;
    crate::wacz::warcs_stored(&mut zip)
}

/// Display name for a source: the WACZ filename stem, for a file or URL.
fn source_display_name(source: &Source) -> String {
    match source {
        Source::File(p) => file_display_name(p),
        Source::Url(u) => {
            let path = u.split(['?', '#']).next().unwrap_or(u);
            let base = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(u);
            base.strip_suffix(".wacz").unwrap_or(base).to_string()
        }
    }
}

/// Build the body text for a collection-level Tantivy document from its metadata.
fn build_collection_body(meta: &crate::wacz::WaczMetadata) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(desc) = &meta.description {
        parts.push(desc.clone());
    }
    for page in &meta.seed_pages {
        if let Some(title) = &page.title {
            parts.push(title.clone());
        }
        parts.push(page.url.clone());
    }
    parts.join(" ")
}

/// A raw contribution to a page's search document, parsed from one WARC record.
enum RawRecord {
    /// An HTML response: source of the page title, description, headings, and a
    /// scraped-text fallback body. (PDF responses reuse this variant with just a
    /// title and body.)
    Html {
        url: String,
        timestamp: String,
        title: String,
        body: String,
        description: String,
        headings: String,
        keywords: String,
        author: String,
        /// `"html"` or `"pdf"`.
        media_type: String,
        /// `<html lang>` value (empty for PDFs).
        lang: String,
        /// HTTP response status code, if known.
        status: Option<u16>,
        /// Year from the HTTP `Last-Modified` header, if present.
        modified_year: Option<u64>,
    },
    /// A `urn:text:` resource record: Browsertrix's fully rendered (post-JS)
    /// page text. Richer than scraped HTML, especially for SPAs.
    Text {
        url: String,
        timestamp: String,
        text: String,
    },
}

/// Accumulated per-URL data merged from all WARC records for that page.
#[derive(Default)]
struct MergedPage {
    timestamp: String,
    title: Option<String>,
    html_body: Option<String>,
    rendered_text: Option<String>,
    description: Option<String>,
    headings: Option<String>,
    keywords: Option<String>,
    author: Option<String>,
    media_type: Option<String>,
    lang: Option<String>,
    status: Option<u16>,
    modified_year: Option<u64>,
}

/// Provenance and capture stats gathered during one WACZ indexing pass, so the
/// manifest can record them without re-reading the WARCs.
#[derive(Default, Debug)]
struct CrawlStats {
    pages: u64,
    earliest_capture: Option<String>,
    latest_capture: Option<String>,
    warcinfo: Option<Warcinfo>,
}

/// Index all WARC entries inside a WACZ file into the Tantivy full-text index.
///
/// Records are collected across every inner WARC (rendered `urn:text:` records
/// often live in a separate WARC from the HTML response), merged into one
/// document per URL, and indexed once. The body prefers Browsertrix's rendered
/// text and falls back to scraped HTML; the title comes from the HTML.
fn index_wacz(
    wacz_path: &Path,
    // WACZ id/name (tagged on each page as collection_id/collection_name).
    collection_id: &str,
    collection_name: &str,
    // Curated collection id (slug) the WACZ belongs to.
    collection: &str,
    search: &Mutex<SearchIndex>,
) -> Result<CrawlStats> {
    let warc_paths: Vec<_> = iter_warc_paths(wacz_path)?
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("listing WARC entries in {}", wacz_path.display()))?;

    let per_warc: Vec<(Vec<RawRecord>, Option<Warcinfo>)> = warc_paths
        .par_iter()
        .map(|entry_name| {
            let tmp = extract_warc_from_wacz(wacz_path, entry_name)
                .with_context(|| format!("extracting {} from {}", entry_name, wacz_path.display()))?;
            collect_page_records(tmp.path())
        })
        .collect::<Result<Vec<_>>>()?;

    // Flatten to all records + the first warcinfo (warcinfo leads its WARC, so
    // the first WARC's is the crawl-level record), then merge and index.
    let mut warcinfo: Option<Warcinfo> = None;
    let mut raws: Vec<RawRecord> = Vec::new();
    for (r, wi) in per_warc {
        if warcinfo.is_none() {
            warcinfo = wi;
        }
        raws.extend(r);
    }
    index_merged(raws, warcinfo, collection_id, collection_name, collection, search, &wacz_path.display().to_string())
}

/// Index a WACZ by CDX-guided/streaming extraction over a `Read + Seek` source
/// (a local file or an HTTP range reader): read only the page-relevant records
/// the CDX points at, rather than scanning every WARC record. Produces the same
/// index as [`index_wacz`] (both share [`record_to_raw`] and [`index_merged`]).
fn index_wacz_streaming<R: std::io::Read + std::io::Seek>(
    reader: R,
    collection_id: &str,
    collection_name: &str,
    collection: &str,
    search: &Mutex<SearchIndex>,
    label: &str,
    progress: Option<&dyn IndexProgress>,
) -> Result<CrawlStats> {
    let (raws, warcinfo) = collect_page_records_via_cdx(reader, progress)?;
    index_merged(raws, warcinfo, collection_id, collection_name, collection, search, label)
}

/// Merge per-record contributions into one document per URL and index them.
/// Shared by the scan-everything ([`index_wacz`]) and CDX-guided
/// ([`index_wacz_streaming`]) paths.
fn index_merged(
    raws: Vec<RawRecord>,
    warcinfo: Option<Warcinfo>,
    collection_id: &str,
    collection_name: &str,
    collection: &str,
    search: &Mutex<SearchIndex>,
    label: &str,
) -> Result<CrawlStats> {
    let mut pages: HashMap<String, MergedPage> = HashMap::new();
    {
        for raw in raws {
            match raw {
            RawRecord::Html { url, timestamp, title, body, description, headings, keywords, author, media_type, lang, status, modified_year } => {
                let e = pages.entry(url).or_default();
                // The HTML capture is the authoritative timestamp for replay.
                e.timestamp = timestamp;
                if !title.is_empty() {
                    e.title = Some(title);
                }
                if !body.is_empty() {
                    e.html_body = Some(body);
                }
                if !description.is_empty() {
                    e.description = Some(description);
                }
                if !headings.is_empty() {
                    e.headings = Some(headings);
                }
                if !keywords.is_empty() {
                    e.keywords = Some(keywords);
                }
                if !author.is_empty() {
                    e.author = Some(author);
                }
                if !media_type.is_empty() {
                    e.media_type = Some(media_type);
                }
                if !lang.is_empty() {
                    e.lang = Some(lang);
                }
                if status.is_some() {
                    e.status = status;
                }
                if modified_year.is_some() {
                    e.modified_year = modified_year;
                }
            }
            RawRecord::Text { url, timestamp, text } => {
                let e = pages.entry(url).or_default();
                if e.timestamp.is_empty() {
                    e.timestamp = timestamp;
                }
                e.rendered_text = Some(text);
                // Rendered text always comes from an HTML page.
                e.media_type.get_or_insert_with(|| "html".to_string());
            }
            }
        }
    }

    let mut count = 0u64;
    let mut earliest: Option<String> = None;
    let mut latest: Option<String> = None;
    {
        use crate::search::Page;
        let mut s = search.lock().unwrap();
        for (url, m) in pages {
            // Prefer the fully rendered text; fall back to scraped HTML.
            let body = m.rendered_text.or(m.html_body).unwrap_or_default();
            let title = m.title.unwrap_or_default();
            let description = m.description.unwrap_or_default();
            let headings = m.headings.unwrap_or_default();
            let keywords = m.keywords.unwrap_or_default();
            let author = m.author.unwrap_or_default();
            let media_type = m.media_type.unwrap_or_default();
            let lang = m.lang.unwrap_or_default();
            if title.is_empty() && body.is_empty() && description.is_empty() {
                continue;
            }
            s.index_page(&Page {
                url: &url,
                timestamp: &m.timestamp,
                title: &title,
                body: &body,
                description: &description,
                headings: &headings,
                keywords: &keywords,
                author: &author,
                media_type: &media_type,
                lang: &lang,
                status: m.status,
                modified_year: m.modified_year,
                collection_id,
                collection_name,
                collection,
            })?;
            count += 1;
            // Track the capture date range (14-digit timestamps sort
            // chronologically as plain strings).
            if !m.timestamp.is_empty() {
                if earliest.as_deref().is_none_or(|e| m.timestamp.as_str() < e) {
                    earliest = Some(m.timestamp.clone());
                }
                if latest.as_deref().is_none_or(|l| m.timestamp.as_str() > l) {
                    latest = Some(m.timestamp.clone());
                }
            }
        }
    }

    info!(pages = count, wacz = %label, "indexed pages from WACZ");
    Ok(CrawlStats {
        pages: count,
        earliest_capture: earliest,
        latest_capture: latest,
        warcinfo,
    })
}

/// Parse an extracted WARC file into raw per-record contributions (HTML
/// responses and `urn:text:` rendered-text resources). Other record types
/// (images, JS, CSS, redirects, other `urn:` pseudo-records) are ignored.
fn collect_page_records(warc_path: &Path) -> Result<(Vec<RawRecord>, Option<Warcinfo>)> {
    let records: Vec<WarcRecord> = iter_records(warc_path)
        .with_context(|| format!("reading {}", warc_path.display()))?
        .collect::<Result<Vec<_>>>()?;

    let mut out = Vec::new();
    let mut warcinfo: Option<Warcinfo> = None;
    for record in &records {
        // Capture the crawl's warcinfo (checked before the URI gate in
        // record_to_raw, since warcinfo records carry no WARC-Target-URI).
        if warcinfo.is_none() {
            if let Some(info) = Warcinfo::from_record(record) {
                if !info.is_empty() {
                    warcinfo = Some(info);
                }
            }
        }
        if let Some(raw) = record_to_raw(record) {
            out.push(raw);
        }
    }

    Ok((out, warcinfo))
}

/// CDX-guided extraction over a `Read + Seek` WACZ: read the CDX, fetch only the
/// page-relevant records (HTML/PDF responses and `urn:text:` rendered text) by
/// seeking to `data_start + offset`, and transform each with [`record_to_raw`].
/// Images/JS/JSON/pageinfo/thumbnail captures are never fetched. Streaming
/// indexes exactly what the CDX lists (authoritative for Browsertrix WACZs).
fn collect_page_records_via_cdx<R: std::io::Read + std::io::Seek>(
    reader: R,
    progress: Option<&dyn IndexProgress>,
) -> Result<(Vec<RawRecord>, Option<Warcinfo>)> {
    use crate::wacz;
    if let Some(p) = progress {
        p.phase("reading index");
    }
    let mut zip = zip::ZipArchive::new(reader).context("opening WACZ ZIP")?;
    wacz::ensure_warcs_stored(&mut zip)?;
    let cdx = wacz::cdx_records(&mut zip)?;
    let starts = wacz::warc_data_starts(&mut zip)?;
    let warcinfo = wacz::find_warcinfo_streaming(&mut zip)?;
    let mut reader = zip.into_inner();

    // Records that can become a page; skip media/pseudo-records. Counting up
    // front gives the progress bar a total (each fetch is an HTTP range request),
    // which switches the setup spinner over to a determinate bar.
    let wanted = |c: &crate::wacz::CdxjRecord| {
        c.length != 0
            && (c.url.starts_with("urn:text:") || c.mime.contains("html") || c.mime.contains("pdf"))
    };
    let total = cdx.iter().filter(|c| wanted(c)).count() as u64;
    if let Some(p) = progress {
        p.set_total(total);
    }

    let mut out = Vec::new();
    let mut done: u64 = 0;
    for c in cdx.iter().filter(|c| wanted(c)) {
        let base = c.filename.rsplit('/').next().unwrap_or(&c.filename);
        if let Some(&start) = starts.get(base) {
            match wacz::record_at(&mut reader, start + c.offset, c.length) {
                Ok(records) => out.extend(records.iter().filter_map(record_to_raw)),
                Err(e) => tracing::warn!(url = %c.url, "skipping unreadable CDX record: {e:#}"),
            }
        }
        done += 1;
        if let Some(p) = progress {
            p.set_records(done);
        }
    }
    Ok((out, warcinfo))
}

/// Transform one WARC record into an indexable [`RawRecord`], or `None` if it is
/// not a page: warcinfo, `dns:`, other `urn:` pseudo-records (pageinfo,
/// thumbnail, …), non-HTML/PDF responses, or empty payloads. Shared by the
/// scan-everything path ([`collect_page_records`]) and the CDX-guided path
/// ([`collect_page_records_via_cdx`]) so both index identically.
fn record_to_raw(record: &WarcRecord) -> Option<RawRecord> {
    let uri = record.target_uri.as_str();
    if uri.is_empty() || uri.starts_with("dns:") {
        return None;
    }

    // Browsertrix stores fully rendered page text as a `urn:text:<url>` resource
    // record (WARC-Type: resource). Map it back to the real URL as the body.
    if let Some(real_url) = uri.strip_prefix("urn:text:") {
        let text = String::from_utf8_lossy(&record.payload).trim().to_string();
        if text.is_empty() {
            return None;
        }
        return Some(RawRecord::Text {
            url: real_url.to_string(),
            timestamp: record.timestamp.clone(),
            text,
        });
    }

    // Skip other urn: pseudo-records (pageinfo, thumbnail, view, …).
    if uri.starts_with("urn:") || !record.warc_type.eq_ignore_ascii_case("response") {
        return None;
    }
    let mime = record.content_type.to_ascii_lowercase();

    // PDF responses: extract text as the body, title from the URL's filename.
    if mime.contains("pdf") {
        if record.payload.is_empty() {
            return None;
        }
        let Some(text) = crate::pdf::extract_pdf_text(&record.payload) else {
            debug!(url = uri, "PDF text extraction yielded nothing; skipping");
            return None;
        };
        return Some(RawRecord::Html {
            url: uri.to_string(),
            timestamp: record.timestamp.clone(),
            title: pdf_title_from_url(uri),
            body: text,
            description: String::new(),
            headings: String::new(),
            keywords: String::new(),
            author: String::new(),
            media_type: "pdf".to_string(),
            lang: String::new(),
            status: record.http_status,
            modified_year: last_modified_year(&record.http_headers),
        });
    }

    if !mime.contains("html") || record.payload.is_empty() {
        return None;
    }
    let html = extract_html_text(&record.payload);
    if html.title.is_empty() && html.body.is_empty() && html.description.is_empty() {
        return None;
    }
    Some(RawRecord::Html {
        url: uri.to_string(),
        timestamp: record.timestamp.clone(),
        title: html.title,
        body: html.body,
        description: html.description,
        headings: html.headings,
        keywords: html.keywords,
        author: html.author,
        media_type: "html".to_string(),
        lang: html.lang,
        status: record.http_status,
        modified_year: last_modified_year(&record.http_headers),
    })
}

/// The year from an HTTP `Last-Modified` header, or `None` if the header is
/// absent or unparseable. Only the modern IMF-fixdate form (RFC 7231, e.g.
/// `Wed, 21 Oct 2015 07:28:00 GMT`) is parsed — the two obsolete HTTP-date
/// formats (RFC 850 and asctime) are rare and yield `None`.
fn last_modified_year(headers: &[(String, String)]) -> Option<u64> {
    let value = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("last-modified"))
        .map(|(_, v)| v.as_str())?;
    // IMF-fixdate is RFC 2822-compatible (chrono accepts the "GMT" zone).
    let dt = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
    let year = chrono::Datelike::year(&dt);
    (year > 0).then_some(year as u64)
}

/// Derive a page title for a PDF from the last path segment of its URL
/// (e.g. `https://x.org/docs/report.pdf` -> `report.pdf`), falling back to the
/// full URL when there is no usable segment.
fn pdf_title_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/')
        .find(|seg| !seg.is_empty())
        .unwrap_or(url)
        .to_string()
}

/// Strip archive extensions to get a clean display name.
fn file_display_name(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
    for suffix in &[".warc.gz", ".warc", ".wacz"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Index a fixture WACZ either by scanning (default) or CDX-guided streaming,
    /// returning the page count for a parity comparison. (Per-record extraction
    /// correctness is covered by `wacz::record_at` tests + the offset proof.)
    fn indexed_page_count(fixture_name: &str, stream: bool) -> u64 {
        use crate::search::SearchIndex;
        let f = fixture(fixture_name);
        let tmp = TempDir::new().unwrap();
        let search = Mutex::new(SearchIndex::open(tmp.path()).unwrap());
        let stats = if stream {
            let file = std::fs::File::open(&f).unwrap();
            index_wacz_streaming(file, "cid", "cname", "coll", &search, fixture_name, None).unwrap()
        } else {
            index_wacz(&f, "cid", "cname", "coll", &search).unwrap()
        };
        stats.pages
    }

    #[test]
    fn streaming_matches_scan_on_a_stored_wacz() {
        // a.wacz stores its WARCs uncompressed, so streaming can seek into them.
        let scan = indexed_page_count("a.wacz", false);
        let stream = indexed_page_count("a.wacz", true);
        assert!(scan > 0, "fixture should index some pages");
        assert_eq!(scan, stream, "CDX-guided streaming must index the same page count as scanning");
    }

    #[test]
    fn streaming_refuses_a_deflated_wacz() {
        use crate::search::SearchIndex;
        // simple.wacz deflates its WARC entries, which streaming can't seek into.
        let f = fixture("simple.wacz");
        let tmp = TempDir::new().unwrap();
        let search = Mutex::new(SearchIndex::open(tmp.path()).unwrap());
        let file = std::fs::File::open(&f).unwrap();
        let err = index_wacz_streaming(file, "cid", "cname", "coll", &search, "simple.wacz", None)
            .unwrap_err()
            .to_string()
            .to_lowercase();
        assert!(err.contains("stored") || err.contains("compress"), "unexpected error: {err}");
    }

    #[test]
    fn last_modified_year_parses_http_date() {
        let headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            ("Last-Modified".to_string(), "Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        ];
        assert_eq!(last_modified_year(&headers), Some(2015));
        // Header name match is case-insensitive.
        let headers = vec![("last-modified".to_string(), "Mon, 01 Jan 2001 00:00:00 GMT".to_string())];
        assert_eq!(last_modified_year(&headers), Some(2001));
        // Absent or unparseable -> None.
        assert_eq!(last_modified_year(&[]), None);
        assert_eq!(
            last_modified_year(&[("Last-Modified".to_string(), "not a date".to_string())]),
            None
        );
    }
    use tempfile::TempDir;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(FIXTURES).join(name)
    }

    /// Copy a fixture WACZ into `<home>/archive` and index it from there, which
    /// the archive requirement demands for local files. Returns the copied path.
    fn index_fixture(name: &str, home: &Path, display: Option<&str>) -> std::path::PathBuf {
        let archive = home.join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let dest = archive.join(name);
        std::fs::copy(fixture(name), &dest).unwrap();
        index_path(&dest, home, display).unwrap();
        dest
    }

    #[test]
    fn index_path_wacz_writes_manifest() {
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), Some("my-collection"));

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(manifest.waczs.len(), 1);
        let col = &manifest.waczs[0];
        assert_eq!(col.name, "my-collection");
        assert!(!col.sha256.is_empty());
        assert!(col.file_size > 0);
    }

    #[test]
    fn index_path_name_defaults_to_stem() {
        // simple.wacz has no title in its datapackage, so the name falls back
        // to the filename stem.
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(manifest.waczs[0].name, "simple");
    }

    #[test]
    fn indexed_local_wacz_is_stored_relative_to_home() {
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);
        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(
            manifest.waczs[0].source,
            Source::File(PathBuf::from("archive/simple.wacz")),
            "a local WACZ under archive/ should be stored relative to home"
        );
    }

    #[test]
    fn provenance_is_recorded_on_the_manifest() {
        // a.wacz carries crawler software (datapackage + warcinfo) and real
        // captures, so the manifest entry should record provenance.
        let tmp = TempDir::new().unwrap();
        index_fixture("a.wacz", tmp.path(), None);

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        let col = &manifest.waczs[0];
        assert!(
            col.software.iter().any(|s| s.contains("Browsertrix-Crawler")),
            "unexpected software: {:?}",
            col.software
        );
        assert!(col.page_count.is_some(), "page_count should be recorded");
    }

    #[test]
    fn index_into_named_collection_groups_the_wacz() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let dest = archive.join("simple.wacz");
        std::fs::copy(fixture("simple.wacz"), &dest).unwrap();

        index_location(&dest.to_string_lossy(), tmp.path(), None, Some("My Project"), false, false, None).unwrap();

        let m = crate::collections::Manifest::open(&tmp.path().join("index")).unwrap();
        assert!(
            m.collections.iter().any(|c| c.id == "my-project" && c.name == "My Project"),
            "collection should be created: {:?}",
            m.collections.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        assert_eq!(m.waczs[0].collection, "my-project", "WACZ should reference the collection");
    }

    #[test]
    fn index_rejects_wacz_outside_archive() {
        // A valid WACZ that is not under <home>/archive is refused with guidance.
        let home = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let stray = elsewhere.path().join("simple.wacz");
        std::fs::copy(fixture("simple.wacz"), &stray).unwrap();

        let err = index_path(&stray, home.path(), None)
            .err()
            .expect("indexing a WACZ outside the archive folder should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("archive"), "error should mention the archive folder: {msg}");
    }

    #[test]
    fn index_rejects_a_directory() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();

        let err = index_path(&archive, tmp.path(), None)
            .err()
            .expect("indexing a directory should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("directory"), "error should say it is a directory: {msg}");
    }

    #[test]
    fn index_name_comes_from_datapackage_title() {
        // pdf-doc.wacz has "title": "PDF Test Collection" in its datapackage,
        // which should name the collection when --name is not given.
        let tmp = TempDir::new().unwrap();
        index_fixture("pdf-doc.wacz", tmp.path(), None);

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(manifest.waczs[0].name, "PDF Test Collection");
    }

    #[test]
    fn explicit_name_overrides_datapackage_title() {
        // --name wins even when the WACZ has a title.
        let tmp = TempDir::new().unwrap();
        index_fixture("pdf-doc.wacz", tmp.path(), Some("Custom Name"));

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(manifest.waczs[0].name, "Custom Name");
    }

    #[test]
    fn reindex_rebuilds_from_manifest() {
        // Index once with a custom name, then blow away just the full-text index
        // (as a schema change / corruption would require) and reindex from the
        // manifest.
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), Some("keepname"));

        let full_text = tmp.path().join("index").join("full_text");
        std::fs::remove_dir_all(&full_text).unwrap();

        reindex(tmp.path()).unwrap();

        // The manifest (and the custom name) is preserved...
        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(manifest.waczs.len(), 1);
        assert_eq!(manifest.waczs[0].name, "keepname");

        // ...and the content is searchable again.
        let idx = crate::search::SearchIndex::open(full_text.as_path()).unwrap();
        assert!(!idx.search("example", 10).unwrap().is_empty(), "reindexed content should be searchable");
    }

    #[test]
    fn reindex_with_no_collections_is_ok() {
        let tmp = TempDir::new().unwrap();
        // No collections.json yet: reindex should be a no-op, not an error.
        reindex(tmp.path()).unwrap();
    }

    #[test]
    fn reindex_failure_names_the_collection_and_suggests_rerun() {
        // A manifest that points at a file which exists but isn't a valid WACZ.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("index")).unwrap();
        std::fs::create_dir_all(tmp.path().join("archive")).unwrap();
        std::fs::write(tmp.path().join("archive/bad.wacz"), b"not a zip file").unwrap();
        let manifest = r#"[{"id":"deadbeef","source":"archive/bad.wacz","name":"BadOne","date_indexed":"2026-01-01T00:00:00Z","file_size":14,"sha256":"00"}]"#;
        std::fs::write(tmp.path().join("index/collections.json"), manifest).unwrap();

        let err = reindex(tmp.path()).err().expect("reindex should fail on a corrupt WACZ");
        let msg = format!("{err:#}");
        assert!(msg.contains("BadOne"), "error should name the failing collection: {msg}");
        assert!(msg.contains("reindex"), "error should tell the user to reindex again: {msg}");
    }

    #[test]
    fn pdf_pages_are_filterable_by_type() {
        // End-to-end: a PDF response in the WACZ should be tagged type:pdf so
        // it can be filtered from the search box.
        let tmp = TempDir::new().unwrap();
        index_fixture("pdf-doc.wacz", tmp.path(), None);

        let idx = crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
        let results = idx.search("type:pdf", 10).unwrap();
        assert!(
            results.iter().any(|r| r.doc_type == "page"),
            "PDF page should be reachable via type:pdf"
        );
    }

    #[test]
    fn index_wacz_html_is_searchable() {
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);

        let idx = crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
        let results = idx.search("example", 10).unwrap();
        assert!(!results.is_empty(), "should find HTML content from WACZ");
        assert_eq!(results[0].collection_name, "simple");
    }

    #[test]
    fn index_wacz_stores_seed_pages_in_manifest() {
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        let col = &manifest.waczs[0];
        assert!(
            !col.seed_pages.is_empty(),
            "simple.wacz has pages in pages.jsonl"
        );
        assert_eq!(col.seed_pages[0].url, "http://example.com/");
    }

    #[test]
    fn index_wacz_collection_is_searchable() {
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);

        let idx = crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
        // The seed page URL "http://example.com/" is part of the collection body.
        let results = idx.search("example.com", 10).unwrap();
        assert!(
            results.iter().any(|r| r.doc_type == "collection"),
            "collection document should be searchable"
        );
    }

    #[test]
    fn reindexing_does_not_duplicate_documents() {
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);
        index_fixture("simple.wacz", tmp.path(), None);

        let idx = crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path()).unwrap();
        let results = idx.search("example", 50).unwrap();
        let pages = results.iter().filter(|r| r.doc_type == "page").count();
        assert_eq!(pages, 1, "re-indexing should upsert, not duplicate pages");
    }

    #[test]
    fn mime_display_name_strips_extension() {
        let p = Path::new("/data/my-archive.wacz");
        assert_eq!(file_display_name(p), "my-archive");
        let p2 = Path::new("/data/my.warc.gz");
        assert_eq!(file_display_name(p2), "my");
    }
}
