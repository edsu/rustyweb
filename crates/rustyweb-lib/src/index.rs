use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rayon::prelude::*;
use tracing::{debug, info};

use crate::collections::{file_sha256, wacz_id, BrowsertrixRef, Manifest, Source, Wacz};
use crate::http_range::{RangeFetch, RangeReader};
use crate::search::{extract_html_text, SearchIndex};
use crate::wacz::{extract_warc_from_wacz, iter_warc_paths, read_datapackage};
use crate::warc::{iter_records, WarcRecord, Warcinfo};

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
    /// A WACZ was indexed with `pages` pages, and the index has been committed.
    /// Emits a persistent one-line summary (the bar itself is transient and, in
    /// bar mode, the INFO logs that would otherwise report this are hushed).
    fn wacz_indexed(&self, label: &str, pages: u64);
    /// Work on the current WACZ finished (clear the spinner/bar).
    fn finish(&self);
}

/// Resolves a refreshable remote [`Source`] — currently a
/// [`Source::Browsertrix`] resource — to a fresh, directly-fetchable URL
/// (Browsertrix presigned URLs expire, so they must be re-resolved each time we
/// index or replay). Implemented by the **binary**, which holds the credentials,
/// keeping auth/config out of the library. `Send + Sync` so it can be shared
/// while indexing and held in the server's shared state for replay.
pub trait SourceResolver: Send + Sync {
    fn resolve(&self, source: &Source) -> Result<String>;
}

/// Index a local WACZ file into the given collection (it's filed into
/// `<home>/archive/<slug>/`). Thin wrapper over [`index_location`].
pub fn index_path(path: &Path, home: &Path, name: Option<&str>, collection: &str) -> Result<()> {
    index_location(
        &path.to_string_lossy(),
        home,
        name,
        collection,
        false,
        None,
        None,
    )
}

/// Index a WACZ from a location into the home directory's `index/`. The location
/// is either a local `.wacz` file (from anywhere) or a remote `http(s)://` URL.
///
/// A local WACZ is filed into `<home>/archive/<collection-slug>/` — moved if it
/// already sits under `archive/`, copied otherwise — and its path stored relative
/// to `home`, so the home folder (archive + collections + index) is portable. A
/// directory or non-`.wacz` path is an error (see [`resolve_sources`] /
/// [`place_local_wacz`]).
///
/// Idempotent: re-indexing the same source upserts its manifest entry and
/// replaces its documents in Tantivy.
/// `name` overrides the collection display name; otherwise it comes from the
/// WACZ metadata, falling back to the filename/URL stem.
pub fn index_location(
    location: &str,
    home: &Path,
    name: Option<&str>,
    collection: &str,
    download: bool,
    concurrency: Option<usize>,
    progress: Option<&dyn IndexProgress>,
) -> Result<()> {
    index_location_with_resolver(
        location,
        home,
        name,
        collection,
        download,
        concurrency,
        None,
        progress,
    )
}

/// Like [`index_location`], but with a [`SourceResolver`] for refreshable remote
/// sources (Browsertrix). The importer's streaming mode passes one; plain
/// `index` doesn't need it.
#[allow(clippy::too_many_arguments)]
pub fn index_location_with_resolver(
    location: &str,
    home: &Path,
    name: Option<&str>,
    collection: &str,
    // Download a remote WACZ into <home>/archive and index it as a local file
    // instead of streaming it in place.
    download: bool,
    // Concurrent record fetches for CDX-guided streaming; `None` = per-source
    // default (16 remote, CPU count local).
    concurrency: Option<usize>,
    // Resolves a Browsertrix source to a fresh presigned URL (binary-provided).
    resolver: Option<&dyn SourceResolver>,
    // Optional progress sink for indexing (the binary renders a bar).
    progress: Option<&dyn IndexProgress>,
) -> Result<()> {
    // Every crawl belongs to a collection (its id is the slug of the name).
    let group = (
        crate::collections::slugify(collection),
        collection.to_string(),
    );

    let index_dir = index_dir(home);
    std::fs::create_dir_all(&index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;

    let mut manifest = Manifest::open(&index_dir)?;

    // Validate the argument and file local WACZs into the collection's archive
    // folder (a bad path errors before we touch the index; the manifest lets us
    // refuse a silent re-collection of an already-registered crawl).
    let sources = resolve_sources(location, home, &group.0, &manifest)?;

    let search = Mutex::new(
        SearchIndex::open(index_dir.join("full_text").as_path())
            .with_context(|| format!("opening search index at {}", index_dir.display()))?,
    );

    for source in &sources {
        let (wacz_name, pages) = index_one(
            source,
            home,
            &mut manifest,
            &search,
            name,
            (group.0.as_str(), group.1.as_str()),
            download,
            concurrency,
            resolver,
            progress,
        )?;
        // Print the per-WACZ summary as this one finishes, so the next WACZ's bar
        // doesn't erase the record of it (the line persists above the new bar).
        // Emitted before the shared final commit; a commit failure below still
        // surfaces as an error.
        if let Some(p) = progress {
            p.wacz_indexed(&wacz_name, pages);
        }
    }

    // The Tantivy commit (segment flush) is the slow tail, especially after fast
    // local reads. Show it as a spinner, then clear the indicator.
    if let Some(p) = progress {
        p.phase("committing");
    }
    let commit_start = std::time::Instant::now();
    search.into_inner().unwrap().commit()?;
    debug!(
        commit_ms = commit_start.elapsed().as_millis() as u64,
        "committed index"
    );
    manifest.save()?;
    if let Some(p) = progress {
        p.finish();
    }

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
pub fn reindex(
    home: &Path,
    // Concurrent record fetches per source for CDX-guided streaming; `None` picks
    // a per-source default (see `default_concurrency`).
    concurrency: Option<usize>,
    // Resolves any Browsertrix sources in the manifest to fresh presigned URLs
    // (binary-provided). `None` → such a source errors (needs credentials).
    resolver: Option<&dyn SourceResolver>,
    // Optional progress sink; drives the same per-WACZ bar as `index`.
    progress: Option<&dyn IndexProgress>,
) -> Result<()> {
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
            (
                w.source.clone(),
                w.name.clone(),
                w.collection.clone(),
                coll_name,
            )
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
    let mut skipped = 0usize;
    for (source, name, collection_id, collection_name) in &targets {
        // Skip local files that no longer exist rather than failing the run;
        // their manifest entry is preserved (see `rustyweb verify`).
        if !source.is_url() {
            match source.resolve(home) {
                Some(p) if p.exists() => {}
                _ => {
                    tracing::warn!(source = %source.location(), "skipping missing local WACZ");
                    skipped += 1;
                    continue;
                }
            }
        }
        info!(
            source = %source.location(),
            progress = format!("{}/{}", done + skipped + 1, total),
            "reindexing"
        );
        // Resilient: a source that fails after retries (e.g. a remote host that's
        // down or blocking) is skipped with a warning rather than aborting the
        // whole rebuild — a long reindex over many remote sources shouldn't be
        // torched by one bad source. Its manifest entry is preserved, and
        // membership is re-supplied so the collection survives.
        match index_one(
            source,
            home,
            &mut manifest,
            &search,
            Some(name),
            (collection_id, collection_name),
            false,
            concurrency,
            resolver,
            progress,
        ) {
            Ok((wacz_name, pages)) => {
                done += 1;
                // Print the per-WACZ summary as each one finishes, so the next
                // WACZ's progress bar doesn't erase the record of it (the line
                // persists above the new bar).
                if let Some(p) = progress {
                    p.wacz_indexed(&wacz_name, pages);
                }
            }
            Err(e) => {
                tracing::warn!(
                    source = %source.location(),
                    "skipping WACZ that failed to reindex: {e:#}"
                );
                skipped += 1;
            }
        }
    }

    // The rebuild always runs to completion and the (possibly partial) index is
    // committed and saved, so it's usable even if some sources were skipped.
    search.into_inner().unwrap().commit()?;
    manifest.save()?;
    if let Some(p) = progress {
        p.finish();
    }
    if skipped > 0 {
        // Usable but incomplete: return an error so the process exits non-zero and
        // cron/CI notices, while leaving the mostly-rebuilt index in place.
        anyhow::bail!(
            "reindex finished but {skipped} of {total} source(s) were skipped \
             (indexed {done}); the search index is missing them — fix the cause \
             and run `rustyweb reindex` again to include them"
        );
    }
    info!(reindexed = done, total, "reindex complete");
    Ok(())
}

/// Create or update a collection's curatorial (finding-aid) metadata (its id is
/// the slug of `name`). Only fields set in `fields` change; the finding aid is
/// written to `<home>/collections/<slug>/README.md`. Returns the collection id.
pub fn set_collection(
    home: &Path,
    name: &str,
    fields: &crate::collections::CollectionFields,
) -> Result<String> {
    let index_dir = index_dir(home);
    std::fs::create_dir_all(&index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;
    let mut manifest = Manifest::open(&index_dir)?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let id = manifest.apply_fields(name, fields, &now);
    manifest.save()?;
    info!(collection = %id, "collection metadata updated");
    Ok(id)
}

/// Auto-seed a collection's finding-aid metadata from ingest (WACZ datapackage,
/// Browsertrix API): fills only fields that are still empty, never clobbering a
/// curator's edits (see [`crate::collections::Manifest::seed_fields`]). A no-op
/// when `fields` is empty. Returns the collection id.
pub fn seed_collection(
    home: &Path,
    name: &str,
    fields: &crate::collections::CollectionFields,
) -> Result<String> {
    let index_dir = index_dir(home);
    std::fs::create_dir_all(&index_dir)
        .with_context(|| format!("creating index dir {}", index_dir.display()))?;
    let mut manifest = Manifest::open(&index_dir)?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let id = manifest.seed_fields(name, fields, &now);
    manifest.save()?;
    Ok(id)
}

/// Pin a curator-supplied local image as a collection's representative
/// thumbnail, committed at `collections/<slug>/thumbnail.jpg`. The collection is
/// identified by name (its slug); create it first with `collection set`.
pub fn set_collection_thumbnail(home: &Path, name: &str, image_file: &Path) -> Result<()> {
    let slug = crate::collections::slugify(name);
    let dest = crate::collections::collection_thumb_path(home, &slug);
    crate::thumbnail::set_manual(&dest, image_file)
        .with_context(|| format!("setting thumbnail for collection {slug}"))?;
    info!(collection = %slug, image = %image_file.display(), "pinned collection thumbnail");
    Ok(())
}

/// Set a crawl's curator note at `<home>/collections/<slug>/crawls/<id>.md`. Manifest-
/// only side effect (no reindex); errors if the crawl id is unknown.
pub fn set_crawl_note(home: &Path, crawl_id: &str, note: &str) -> Result<()> {
    let manifest = Manifest::open(&index_dir(home))?;
    let Some(wacz) = manifest.wacz_by_id(crawl_id) else {
        anyhow::bail!(
            "no crawl with id \"{crawl_id}\" - it's the id in the crawl's page URL (/crawl/<id>)"
        );
    };
    crate::collections::write_crawl_note(home, &wacz.collection, crawl_id, note)?;
    info!(crawl = %crawl_id, "crawl note updated");
    Ok(())
}

/// Pin a curator-supplied local image as a crawl's representative thumbnail,
/// committed under the collection (`collections/<slug>/crawls/<id>.jpg`) so it's
/// git-trackable and a later (re)index won't overwrite it. Manifest-only side
/// effect (no reindex).
pub fn set_crawl_thumbnail(home: &Path, crawl_id: &str, image_file: &Path) -> Result<()> {
    let manifest = Manifest::open(&index_dir(home))?;
    let Some(wacz) = manifest.wacz_by_id(crawl_id) else {
        anyhow::bail!(
            "no crawl with id \"{crawl_id}\" - it's the id in the crawl's page URL (/crawl/<id>)"
        );
    };
    let dest = crate::collections::pinned_thumb_path(home, &wacz.collection, crawl_id);
    crate::thumbnail::set_manual(&dest, image_file)
        .with_context(|| format!("setting thumbnail for crawl {crawl_id}"))?;
    info!(crawl = %crawl_id, image = %image_file.display(), "pinned crawl thumbnail");
    Ok(())
}

/// Record Browsertrix import provenance on an already-indexed WACZ. `wacz_file`
/// is the local file (under `<home>/archive`) that was just indexed; it's looked
/// up by the same id indexing assigns (a hash of its home-relative path). Used
/// by the importer for provenance display and incremental re-sync. Manifest-only
/// side effect (no reindex).
pub fn set_browsertrix_provenance(
    home: &Path,
    wacz_file: &Path,
    host: &str,
    item_id: &str,
    resource_hash: &str,
    review_status: Option<u8>,
) -> Result<()> {
    let abs = wacz_file
        .canonicalize()
        .with_context(|| format!("resolving {}", wacz_file.display()))?;
    let id = wacz_id(&Source::for_file(&abs, home));
    set_browsertrix_provenance_by_id(home, &id, host, item_id, resource_hash, review_status)
}

/// As [`set_browsertrix_provenance`], but for a crawl identified by its id — used
/// by the streaming importer, whose source is a [`Source::Browsertrix`] with no
/// local file (its id is `wacz_id(&source)`).
pub fn set_browsertrix_provenance_by_id(
    home: &Path,
    crawl_id: &str,
    host: &str,
    item_id: &str,
    resource_hash: &str,
    review_status: Option<u8>,
) -> Result<()> {
    let mut manifest = Manifest::open(&index_dir(home))?;
    let wacz = manifest
        .waczs
        .iter_mut()
        .find(|w| w.id == crawl_id)
        .with_context(|| format!("no indexed crawl with id {crawl_id}"))?;
    wacz.browsertrix = Some(BrowsertrixRef {
        host: host.to_string(),
        item_id: item_id.to_string(),
        resource_hash: resource_hash.to_string(),
        review_status,
    });
    manifest.save()?;
    Ok(())
}

/// Turn one `index` argument into a source to index, filing local WACZs into the
/// collection's archive folder. An `http(s)://` URL yields a URL source. A local
/// `.wacz` file may live anywhere: it's brought into `<home>/archive/<slug>/` —
/// **moved** if it already sits under `archive/` (reorganized within rustyweb's
/// own space), **copied** otherwise (the original is left intact) — so the home
/// directory stays self-contained and portable and the archive is browsable by
/// collection. Directories and non-`.wacz` paths are errors with guidance.
fn resolve_sources(
    location: &str,
    home: &Path,
    collection_slug: &str,
    manifest: &Manifest,
) -> Result<Vec<Source>> {
    match Source::parse(location) {
        url @ Source::Url(_) => Ok(vec![url]),
        bt @ Source::Browsertrix { .. } => Ok(vec![bt]),
        Source::File(p) => Ok(vec![place_local_wacz(&p, home, collection_slug, manifest)?]),
    }
}

/// The archive folder for a collection: `<home>/archive/<slug>/`, where local
/// WACZs for that collection are filed.
fn collection_archive_dir(home: &Path, slug: &str) -> PathBuf {
    archive_dir(home).join(slug)
}

/// Pick a destination for `source` inside `dir` that won't clobber a *different*
/// WACZ already filed there. A byte-identical file already present is reused (so
/// re-indexing the same WACZ is idempotent); a name clash with different content
/// gets a `-2`, `-3`, … suffix. Returns an existing path only when it's
/// byte-identical to `source`.
fn pick_archive_dest(
    dir: &Path,
    filename: &std::ffi::OsStr,
    source: &Path,
) -> Result<std::path::PathBuf> {
    let first = dir.join(filename);
    if !first.exists() {
        return Ok(first);
    }
    let source_sha = file_sha256(source)?;
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("wacz");
    let mut path = first;
    let mut n = 1u32;
    loop {
        if !path.exists() || file_sha256(&path)? == source_sha {
            return Ok(path);
        }
        n += 1;
        path = dir.join(format!("{stem}-{n}.wacz"));
    }
}

/// Bring a local WACZ into `<home>/archive/<slug>/` and return it as a
/// home-relative File [`Source`]. Moves it if it's already under `archive/`
/// (reorganizing within the managed space); copies it otherwise. A file already
/// in the collection folder is used in place; a byte-identical file already
/// filed there is reused (idempotent re-index).
fn place_local_wacz(
    path: &Path,
    home: &Path,
    collection_slug: &str,
    manifest: &Manifest,
) -> Result<Source> {
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
    let abs = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;

    let dest_dir = collection_archive_dir(home, collection_slug);

    // Already inside the collection's archive folder (at any depth — e.g. the
    // importer's archive/<slug>/<item-id>/ subdir)? Use it in place, no move.
    if dest_dir
        .canonicalize()
        .map(|d| abs.starts_with(&d))
        .unwrap_or(false)
    {
        return Ok(Source::for_file(&abs, home));
    }

    // Guard against silent re-collection: if this exact file is already a
    // registered member of a *different* collection, moving it would change its
    // id and orphan that membership (and its committed note/thumbnail). Re-homing
    // a crawl isn't supported yet, so refuse rather than corrupt.
    if let Some(existing) = manifest.waczs.iter().find(|w| {
        w.source
            .resolve(home)
            .and_then(|p| p.canonicalize().ok())
            .is_some_and(|p| p == abs)
    }) {
        if existing.collection != collection_slug {
            anyhow::bail!(
                "{} is already in collection \"{}\"; re-collecting a crawl isn't supported yet \
                 — remove it from that collection first, or index a separate copy.",
                abs.display(),
                existing.collection
            );
        }
    }

    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating archive dir {}", dest_dir.display()))?;
    let filename = abs
        .file_name()
        .with_context(|| format!("{} has no file name", abs.display()))?;
    let dest = pick_archive_dest(&dest_dir, filename, &abs)?;

    if dest.exists() {
        // pick_archive_dest returned a byte-identical file already filed here —
        // reuse it (idempotent re-index), nothing to write.
    } else {
        // A file already under archive/ is on the same filesystem as its
        // destination, so a rename (move) is cheap and non-duplicating; a file
        // from elsewhere is copied so the curator's original is left untouched.
        let under_archive = archive_dir(home)
            .canonicalize()
            .map(|a| abs.starts_with(&a))
            .unwrap_or(false);
        if under_archive {
            std::fs::rename(&abs, &dest)
                .with_context(|| format!("moving {} to {}", abs.display(), dest.display()))?;
            info!(from = %abs.display(), to = %dest.display(), "moved WACZ into collection archive");
        } else {
            std::fs::copy(&abs, &dest)
                .with_context(|| format!("copying {} to {}", abs.display(), dest.display()))?;
            info!(from = %abs.display(), to = %dest.display(), "copied WACZ into collection archive");
        }
    }
    let dest_abs = dest.canonicalize().unwrap_or(dest);
    Ok(Source::for_file(&dest_abs, home))
}

/// Index a single WACZ source: obtain a local readable copy (downloading a URL
/// to a temp file), index its pages and metadata, and upsert its manifest entry.
/// Returns the WACZ's display name and page count, for a post-commit summary.
#[allow(clippy::too_many_arguments)]
fn index_one(
    source: &Source,
    home: &Path,
    manifest: &mut Manifest,
    search: &Mutex<SearchIndex>,
    name: Option<&str>,
    // The collection (id, display name) this WACZ joins — always set; every
    // crawl belongs to a collection (no singletons).
    collection: (&str, &str),
    // Download a remote WACZ into <home>/archive and index it as a local file
    // (durable copy, whole-file fixity, offline replay) instead of streaming.
    download: bool,
    // Concurrent record fetches for CDX-guided streaming; `None` picks a
    // per-source default (see `default_concurrency`).
    concurrency: Option<usize>,
    // Resolves a Browsertrix source to a fresh presigned URL (binary-provided).
    resolver: Option<&dyn SourceResolver>,
    // Optional progress sink for indexing.
    progress: Option<&dyn IndexProgress>,
) -> Result<(String, u64)> {
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
            Source::File(download_into_archive(u, home, collection.0)?)
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
        // A Browsertrix resource: resolve a fresh presigned URL (they expire) and
        // stream it. The recorded source stays the stable Browsertrix identity,
        // so the id is stable across re-imports and replay re-resolves later.
        bt @ Source::Browsertrix { .. } => {
            if let Some(p) = progress {
                p.phase("resolving");
            }
            let resolver = resolver.ok_or_else(|| {
                anyhow::anyhow!(
                    "indexing a Browsertrix source needs credentials to resolve a fresh \
                     URL — set BROWSERTRIX_USER + BROWSERTRIX_PASSWORD (or BROWSERTRIX_TOKEN)"
                )
            })?;
            let url = resolver
                .resolve(bt)
                .with_context(|| format!("resolving {}", bt.location()))?;
            if !remote_warcs_streamable(&url).unwrap_or(false) {
                anyhow::bail!(
                    "this Browsertrix WACZ can't be stream-indexed (compressed WARCs or \
                     no range support); import it in download mode instead"
                );
            }
            (Some(url), None)
        }
    };
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

    // The curated collection this WACZ joins (always supplied by the caller).
    let (collection_id, collection_name) = (collection.0.to_string(), collection.1.to_string());

    // Drop this WACZ's prior documents so re-indexing upserts, not appends.
    search.lock().unwrap().delete_collection(&id);

    // Index pages, tagging each with this WACZ (id/name) and its collection. The
    // pass also collects provenance and capture stats (no re-read of the WARCs).
    // CDX-guided extraction is the default everywhere (replay already resolves
    // records through the CDX, so indexing trusts it too); a full scan is only
    // the fallback when a WACZ can't be CDX-guided (deflated WARCs / no CDX).
    // Resolve fetch concurrency once we know whether this WACZ is remote, then
    // clamp it to a per-host ceiling so no `--concurrency` setting can flood a
    // single host with an unbounded number of in-flight range requests (polite
    // by default, and a guard against a mis-typed value like `--concurrency 500`).
    let requested = concurrency.unwrap_or_else(|| default_concurrency(remote_url.is_some()));
    let workers = requested.clamp(1, MAX_CONCURRENCY);
    if requested > MAX_CONCURRENCY {
        tracing::warn!(
            requested,
            cap = MAX_CONCURRENCY,
            "capping fetch concurrency to the per-host ceiling"
        );
    }
    // The crawl's representative-image source: the declared main page, else the
    // first seed page. Used after indexing to cache a thumbnail (best-effort).
    let main_page_url = meta
        .main_page_url
        .clone()
        .or_else(|| meta.seed_pages.first().map(|s| s.url.clone()));
    let thumbs_dir = index_dir(home).join("thumbs");
    // A nested multi-WACZ (a WACZ of WACZs, e.g. Browsertrix's combined
    // collection download) has no top-level WARCs, so the normal paths would
    // index it as empty. Detect and index it up front, flattening all inner
    // WACZs into this one crawl; otherwise fall through to normal indexing.
    // (This costs one extra WACZ open on the common flat path — cheap: reading
    // the ZIP directory, dwarfed by the record streaming that follows — and it's
    // the deliberate price of detecting nesting structurally rather than trusting
    // the non-standard multi-wacz-package profile.)
    let stats = if let Some(nested) = index_nested(
        local.as_deref(),
        remote_url.as_deref(),
        &id,
        &display_name,
        &collection_id,
        search,
        workers,
        progress,
    )? {
        nested
    } else {
        match &remote_url {
            Some(u) => {
                info!(url = %u, "streaming remote WACZ index (no download)");
                let fetch = crate::http_range::HttpFetch::open(u)?;
                let stats = index_wacz_streaming(
                    fetch.clone(),
                    &id,
                    &display_name,
                    &collection_id,
                    search,
                    u,
                    workers,
                    progress,
                )?;
                cache_thumbnail(
                    fetch,
                    &thumbs_dir,
                    &id,
                    main_page_url.as_deref(),
                    &crate::collections::pinned_thumb_path(home, &collection_id, &id),
                );
                stats
            }
            None => {
                let p = local.as_ref().unwrap();
                // CDX-guided when the WARCs are Stored (the WACZ spec's SHOULD, always
                // true for Browsertrix output) so a CDX offset maps to a byte
                // position; otherwise fall back to a full scan of every WARC record.
                if local_warcs_streamable(p).unwrap_or(false) {
                    let fetch = crate::http_range::FileFetch::open(p)
                        .with_context(|| format!("opening {} for CDX-guided index", p.display()))?;
                    let stats = index_wacz_streaming(
                        fetch.clone(),
                        &id,
                        &display_name,
                        &collection_id,
                        search,
                        &p.display().to_string(),
                        workers,
                        progress,
                    )?;
                    cache_thumbnail(
                        fetch,
                        &thumbs_dir,
                        &id,
                        main_page_url.as_deref(),
                        &crate::collections::pinned_thumb_path(home, &collection_id, &id),
                    );
                    stats
                } else {
                    // The scan path has no cheap up-front record total, so it stays on
                    // the spinner (no determinate bar). Label it "scanning" - it reads
                    // every WARC record, unlike the CDX-guided path.
                    if let Some(pr) = progress {
                        pr.phase("scanning");
                    }
                    index_wacz(p, &id, &display_name, &collection_id, search)?
                }
            }
        }
    };

    // Capture the outcome to report once the index is committed (see
    // `index_location`), not here - the commit could still fail.
    let outcome = (display_name.clone(), stats.pages);

    // Index the WACZ's metadata as a searchable document, tagged with its collection.
    let coll_body = build_collection_body(&meta);
    search
        .lock()
        .unwrap()
        .index_collection(&id, &display_name, &collection_id, &coll_body)?;

    // Fixity: a streamed remote is never fully read, so there's no whole-file
    // SHA-256 (empty; `verify` already skips remote sources). Its size comes
    // from the HTTP Content-Length. A local/downloaded file is hashed as before -
    // reading the whole file, which dominates the tail for a large local WACZ (so
    // it gets its own "checksumming" phase and timing, not lumped into indexing).
    let (sha, file_size) = match &remote_url {
        Some(u) => (
            String::new(),
            crate::http_range::open_remote(u)?.total_len(),
        ),
        None => {
            let p = local.as_ref().unwrap();
            if let Some(pr) = progress {
                pr.phase("checksumming");
            }
            let sha_start = std::time::Instant::now();
            let sha =
                file_sha256(p).with_context(|| format!("computing sha256 of {}", p.display()))?;
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            debug!(
                sha_ms = sha_start.elapsed().as_millis() as u64,
                bytes = size,
                "computed whole-file SHA-256 (fixity)"
            );
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

    // Seed the collection's finding aid from this WACZ's datapackage — fill-gaps,
    // so only empty fields are set: the first indexed WACZ with a value wins and
    // a curator's edits are never overwritten. A single crawl's `description`
    // isn't really the whole collection's scope, but a draft beats a blank and
    // invites the curator to refine it.
    let year = meta.created.as_deref().and_then(|d| {
        d.get(..4)
            .filter(|y| y.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_string)
    });
    let seed = crate::collections::CollectionFields {
        narrative: meta.description.clone(),
        subjects: (!meta.keywords.is_empty()).then(|| meta.keywords.clone()),
        dates: year,
        creator: meta.creator.clone(),
        rights: (!meta.licenses.is_empty()).then(|| meta.licenses.join(", ")),
        ..Default::default()
    };
    if !seed.is_empty() {
        manifest.seed_fields(&collection_name, &seed, &date_indexed);
    }

    // Preserve Browsertrix import provenance (set out-of-band by the importer)
    // across a reindex, which otherwise rebuilds the entry from scratch.
    let browsertrix = manifest.wacz_by_id(&id).and_then(|w| w.browsertrix.clone());

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
        browsertrix,
        nested_waczs: stats.nested_waczs,
        // Provenance previously parsed-but-dropped / newly read.
        modified: meta.modified,
        is_part_of: warcinfo.is_part_of,
        hostname: warcinfo.hostname,
        conforms_to: warcinfo.conforms_to,
        keywords: meta.keywords,
        licenses: meta.licenses,
        status_counts: stats.status_counts,
    });

    // Note: the spinner/bar is *not* finished here - the Tantivy commit happens
    // once per `index_location` (after all sources), and that's where it's cleared
    // (via a final "committing" spinner) and the summary is emitted. See
    // `index_location`.
    Ok(outcome)
}

/// Download a remote WACZ to a temp file for indexing.
fn download_to_temp(url: &str) -> Result<tempfile::NamedTempFile> {
    use std::io::{copy, Write};

    let mut tmp = tempfile::Builder::new().suffix(".wacz").tempfile()?;
    let mut reader = crate::http_range::get_reader(url)?;
    copy(&mut reader, &mut tmp).with_context(|| format!("writing {url} to temp file"))?;
    tmp.flush()?;
    Ok(tmp)
}

/// Download a remote WACZ into `<home>/archive/<collection-slug>/<name>.wacz` and
/// return its path relative to `home` (so the manifest stores a portable local
/// source, and the archive is browsable by collection). The name comes from the
/// URL's last path segment. Downloads to a temp file first, then files it with
/// [`pick_archive_dest`] so a different WACZ that happens to share the name isn't
/// clobbered (and a re-download of identical bytes is reused). Used by `--download`.
fn download_into_archive(url: &str, home: &Path, collection_slug: &str) -> Result<PathBuf> {
    use std::io::{copy, Write};

    let stem = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("download");
    let name = if stem.ends_with(".wacz") {
        stem.to_string()
    } else {
        format!("{stem}.wacz")
    };

    let dir = collection_archive_dir(home, collection_slug);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating archive dir {}", dir.display()))?;

    // Download into a temp file in the same dir (same filesystem → cheap rename),
    // then choose a non-clobbering final name.
    let mut tmp = tempfile::Builder::new()
        .prefix(".download-")
        .suffix(".wacz")
        .tempfile_in(&dir)
        .with_context(|| format!("temp file in {}", dir.display()))?;
    copy(&mut crate::http_range::get_reader(url)?, &mut tmp)
        .with_context(|| format!("writing {url} to a temp file"))?;
    tmp.flush()?;

    let dest = pick_archive_dest(&dir, std::ffi::OsStr::new(&name), tmp.path())?;
    if !dest.exists() {
        // `persist` renames the temp file into place (and drops the temp guard).
        tmp.persist(&dest)
            .with_context(|| format!("saving download to {}", dest.display()))?;
    } // else: byte-identical file already downloaded here; the temp is discarded.

    // Home-relative path (portable), using the possibly-disambiguated file name.
    let final_name = dest.file_name().unwrap_or(std::ffi::OsStr::new(&name));
    Ok(PathBuf::from("archive")
        .join(collection_slug)
        .join(final_name))
}

/// Whether a remote WACZ can be stream-indexed: reachable, range-capable, and
/// its WARC entries Stored (uncompressed). Reads only the ZIP central directory.
fn remote_warcs_streamable(url: &str) -> Result<bool> {
    let reader = crate::http_range::open_remote(url)?;
    let mut zip = zip::ZipArchive::new(reader)
        .with_context(|| format!("reading remote ZIP central directory of {url}"))?;
    crate::wacz::warcs_stored(&mut zip)
}

/// Whether a local WACZ can be CDX-guided: its `archive/` WARC entries are Stored
/// (uncompressed), so a CDX byte offset maps to an absolute position. The file
/// counterpart of [`remote_warcs_streamable`]; reads only the central directory.
fn local_warcs_streamable(path: &Path) -> Result<bool> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP central directory of {}", path.display()))?;
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
        Source::Browsertrix { resource, .. } => resource
            .strip_suffix(".wacz")
            .unwrap_or(resource)
            .to_string(),
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
    /// Fully rendered (post-JS) page text - Browsertrix's `urn:text:` resource
    /// record, or the `text` field from `pages/*.jsonl`. Richer than scraped
    /// HTML, especially for SPAs. `title` is set only for the pages.jsonl source
    /// (used as a fallback when the HTML capture has no title).
    Text {
        url: String,
        timestamp: String,
        text: String,
        title: Option<String>,
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
    /// For a nested multi-WACZ: how many inner WACZs were flattened into this
    /// crawl. `None` for an ordinary (flat) WACZ.
    nested_waczs: Option<u64>,
    /// HTTP status-code histogram tallied from the CDX (every capture, including
    /// the bodyless 4xx/5xx that never become search documents) — the derived
    /// "capture quality" / Appraisal signal.
    status_counts: BTreeMap<u16, u64>,
}

/// Tally HTTP status codes across every CDX record — the capture-quality signal.
fn tally_status(cdx: &[crate::wacz::CdxjRecord]) -> BTreeMap<u16, u64> {
    let mut counts = BTreeMap::new();
    for rec in cdx {
        if rec.status != 0 {
            *counts.entry(rec.status).or_insert(0) += 1;
        }
    }
    counts
}

/// Detect + index a **nested multi-WACZ** (a WACZ whose payload is other WACZ
/// files; see [`crate::wacz::nested_wacz_locations`]) — e.g. Browsertrix's
/// combined collection `/download`. Each inner `.wacz` is indexed under this
/// (outer) crawl's id, so the whole thing stays one manifest entry (flatten).
/// Returns `None` when the WACZ isn't nested, so the caller falls through to
/// ordinary indexing. Replay is unaffected — wabac.js already resolves nesting.
///
/// A Stored inner WACZ is a contiguous byte window of the outer file, so it's
/// streamed **in place** via a [`SubRangeFetch`] — no extraction, and a remote
/// outer fetches only the ranges it needs. (Only a compressed inner entry, or an
/// inner whose own WARCs aren't Stored, has to be materialized to a temp file —
/// not the Browsertrix case.)
#[allow(clippy::too_many_arguments)]
fn index_nested(
    local: Option<&Path>,
    remote_url: Option<&str>,
    crawl_id: &str,
    crawl_name: &str,
    collection: &str,
    search: &Mutex<SearchIndex>,
    workers: usize,
    progress: Option<&dyn IndexProgress>,
) -> Result<Option<CrawlStats>> {
    match (local, remote_url) {
        (Some(p), _) => {
            let fetch = crate::http_range::FileFetch::open(p)?;
            index_nested_from(
                fetch, crawl_id, crawl_name, collection, search, workers, progress,
            )
        }
        (None, Some(u)) => {
            let fetch = crate::http_range::HttpFetch::open(u)?;
            index_nested_from(
                fetch, crawl_id, crawl_name, collection, search, workers, progress,
            )
        }
        _ => Ok(None),
    }
}

/// Core of [`index_nested`], generic over the outer WACZ's byte source
/// (`FileFetch` locally, `HttpFetch` remotely).
#[allow(clippy::too_many_arguments)]
fn index_nested_from<F: RangeFetch + Clone + Send + Sync>(
    outer: F,
    crawl_id: &str,
    crawl_name: &str,
    collection: &str,
    search: &Mutex<SearchIndex>,
    workers: usize,
    progress: Option<&dyn IndexProgress>,
) -> Result<Option<CrawlStats>> {
    let inners = {
        let mut zip = zip::ZipArchive::new(RangeReader::new(outer.clone()))
            .context("opening WACZ to check for nesting")?;
        crate::wacz::nested_wacz_locations(&mut zip)
    };
    if inners.is_empty() {
        return Ok(None);
    }
    info!(count = inners.len(), "indexing a nested multi-WACZ");

    let mut agg = CrawlStats::default();
    for (i, inner) in inners.iter().enumerate() {
        if let Some(pr) = progress {
            pr.phase(&format!("nested WACZ {}/{}", i + 1, inners.len()));
        }
        let stats = match inner.inline {
            // Stored: read it in place as a window of the outer file.
            Some((base, len)) => index_inner(
                crate::http_range::SubRangeFetch::new(outer.clone(), base, len),
                &inner.name,
                crawl_id,
                crawl_name,
                collection,
                search,
                workers,
                progress,
            )?,
            // Compressed inner entry: extract it (decompressing) to a temp file,
            // then full-scan it. Rare — not produced by Browsertrix.
            None => {
                let mut zip = zip::ZipArchive::new(RangeReader::new(outer.clone()))?;
                let mut tmp =
                    tempfile::NamedTempFile::new().context("temp for compressed nested WACZ")?;
                std::io::copy(&mut zip.by_name(&inner.name)?, tmp.as_file_mut())
                    .with_context(|| format!("extracting nested {}", inner.name))?;
                index_wacz(tmp.path(), crawl_id, crawl_name, collection, search)?
            }
        };
        agg.pages += stats.pages;
        merge_min(&mut agg.earliest_capture, stats.earliest_capture);
        merge_max(&mut agg.latest_capture, stats.latest_capture);
        if agg.warcinfo.is_none() {
            agg.warcinfo = stats.warcinfo;
        }
        for (code, n) in stats.status_counts {
            *agg.status_counts.entry(code).or_insert(0) += n;
        }
    }
    agg.nested_waczs = Some(inners.len() as u64);
    Ok(Some(agg))
}

/// Index one inner WACZ presented as a [`RangeFetch`] window. CDX-guided
/// (streaming, no extraction) when its WARCs are Stored; otherwise the window is
/// materialized to a temp file and full-scanned (rare).
#[allow(clippy::too_many_arguments)]
fn index_inner<F: RangeFetch + Clone + Send + Sync>(
    fetch: F,
    label: &str,
    crawl_id: &str,
    crawl_name: &str,
    collection: &str,
    search: &Mutex<SearchIndex>,
    workers: usize,
    progress: Option<&dyn IndexProgress>,
) -> Result<CrawlStats> {
    let streamable = zip::ZipArchive::new(RangeReader::new(fetch.clone()))
        .ok()
        .map(|mut z| crate::wacz::warcs_stored(&mut z).unwrap_or(false))
        .unwrap_or(false);
    if streamable {
        index_wacz_streaming(
            fetch, crawl_id, crawl_name, collection, search, label, workers, progress,
        )
    } else {
        let tmp = materialize_fetch(&fetch).context("materializing nested WACZ for scan")?;
        index_wacz(tmp.path(), crawl_id, crawl_name, collection, search)
    }
}

/// Copy an entire [`RangeFetch`] to a temp file (chunked), for the fallback
/// paths that need a seekable local file.
fn materialize_fetch<F: RangeFetch>(fetch: &F) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;
    const CHUNK: u64 = 4 * 1024 * 1024;
    let total = fetch.total_len();
    let mut tmp = tempfile::NamedTempFile::new()?;
    let mut pos = 0u64;
    while pos < total {
        let end = (pos + CHUNK).min(total);
        tmp.as_file_mut().write_all(&fetch.fetch(pos, end)?)?;
        pos = end;
    }
    Ok(tmp)
}

/// Keep the smaller of two optional 14-digit capture timestamps (they sort
/// lexicographically), for aggregating a nested WACZ's capture range.
fn merge_min(acc: &mut Option<String>, v: Option<String>) {
    if let Some(v) = v {
        if acc.as_ref().is_none_or(|a| v < *a) {
            *acc = Some(v);
        }
    }
}
/// Keep the larger of two optional 14-digit capture timestamps.
fn merge_max(acc: &mut Option<String>, v: Option<String>) {
    if let Some(v) = v {
        if acc.as_ref().is_none_or(|a| v > *a) {
            *acc = Some(v);
        }
    }
}

/// Index all WARC entries inside a WACZ file into the Tantivy full-text index.
///
/// Records are collected across every inner WARC (rendered `urn:text:` records
/// often live in a separate WARC from the HTML response), merged into one
/// document per URL, and indexed once. The body prefers Browsertrix's rendered
/// text and falls back to scraped HTML; the title comes from the HTML.
fn index_wacz(
    wacz_path: &Path,
    // WACZ id/name (tagged on each page as crawl_id/crawl_name).
    crawl_id: &str,
    crawl_name: &str,
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
            let tmp = extract_warc_from_wacz(wacz_path, entry_name).with_context(|| {
                format!("extracting {} from {}", entry_name, wacz_path.display())
            })?;
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
    // Fold in pages.jsonl/extraPages.jsonl extracted text (see the streaming
    // path); some crawls store rendered text only there, not in the WARCs. Read
    // the CDX here too for the capture-quality status tally (every capture,
    // including bodyless 4xx/5xx that never became RawRecords).
    let mut status_counts = BTreeMap::new();
    if let Ok(file) = std::fs::File::open(wacz_path) {
        if let Ok(mut zip) = zip::ZipArchive::new(file) {
            raws.extend(
                crate::wacz::read_page_texts(&mut zip)
                    .into_iter()
                    .map(|pt| RawRecord::Text {
                        url: pt.url,
                        timestamp: pt.ts,
                        text: pt.text,
                        title: pt.title,
                    }),
            );
            if let Ok(cdx) = crate::wacz::cdx_records(&mut zip) {
                status_counts = tally_status(&cdx);
            }
        }
    }
    index_merged(
        raws,
        warcinfo,
        status_counts,
        crawl_id,
        crawl_name,
        collection,
        search,
        &wacz_path.display().to_string(),
    )
}

/// Index a WACZ by CDX-guided/streaming extraction over a `Read + Seek` source
/// (a local file or an HTTP range reader): read only the page-relevant records
/// the CDX points at, rather than scanning every WARC record. Produces the same
/// index as [`index_wacz`] (both share [`record_to_raw`] and [`index_merged`]).
#[allow(clippy::too_many_arguments)]
fn index_wacz_streaming<F>(
    fetch: F,
    crawl_id: &str,
    crawl_name: &str,
    collection: &str,
    search: &Mutex<SearchIndex>,
    label: &str,
    concurrency: usize,
    progress: Option<&dyn IndexProgress>,
) -> Result<CrawlStats>
where
    F: crate::http_range::RangeFetch + Clone + Send + Sync,
{
    let (raws, warcinfo, status_counts) =
        collect_page_records_via_cdx(fetch, concurrency, progress)?;
    index_merged(
        raws,
        warcinfo,
        status_counts,
        crawl_id,
        crawl_name,
        collection,
        search,
        label,
    )
}

/// Merge per-record contributions into one document per URL and index them.
/// Shared by the scan-everything ([`index_wacz`]) and CDX-guided
/// ([`index_wacz_streaming`]) paths.
#[allow(clippy::too_many_arguments)]
fn index_merged(
    raws: Vec<RawRecord>,
    warcinfo: Option<Warcinfo>,
    status_counts: BTreeMap<u16, u64>,
    crawl_id: &str,
    crawl_name: &str,
    collection: &str,
    search: &Mutex<SearchIndex>,
    label: &str,
) -> Result<CrawlStats> {
    let build_start = std::time::Instant::now();
    let mut pages: HashMap<String, MergedPage> = HashMap::new();
    {
        for raw in raws {
            match raw {
                RawRecord::Html {
                    url,
                    timestamp,
                    title,
                    body,
                    description,
                    headings,
                    keywords,
                    author,
                    media_type,
                    lang,
                    status,
                    modified_year,
                } => {
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
                RawRecord::Text {
                    url,
                    timestamp,
                    text,
                    title,
                } => {
                    let e = pages.entry(url).or_default();
                    if e.timestamp.is_empty() {
                        e.timestamp = timestamp;
                    }
                    e.rendered_text = Some(text);
                    // Rendered text always comes from an HTML page.
                    e.media_type.get_or_insert_with(|| "html".to_string());
                    // A pages.jsonl title fills in only when the HTML capture
                    // gave none - the scraped HTML <title> wins when present.
                    if let Some(t) = title {
                        if !t.is_empty() {
                            e.title.get_or_insert(t);
                        }
                    }
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
                crawl_id,
                crawl_name,
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

    debug!(build_ms = build_start.elapsed().as_millis() as u64, wacz = %label, "built index");
    info!(pages = count, wacz = %label, "indexed pages from WACZ");
    Ok(CrawlStats {
        pages: count,
        earliest_capture: earliest,
        latest_capture: latest,
        warcinfo,
        // Set by index_nested for a multi-WACZ; a single WACZ isn't nested.
        nested_waczs: None,
        status_counts,
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

/// Page records read from a WACZ, the crawl-level `warcinfo`, and the HTTP status
/// tally over the CDX (the capture-quality signal).
type PageRecords = (Vec<RawRecord>, Option<Warcinfo>, BTreeMap<u16, u64>);

/// CDX-guided extraction over a `Read + Seek` WACZ: read the CDX, fetch only the
/// page-relevant records (HTML/PDF responses and `urn:text:` rendered text) by
/// seeking to `data_start + offset`, and transform each with [`record_to_raw`].
/// Images/JS/JSON/pageinfo/thumbnail captures are never fetched. Streaming
/// indexes exactly what the CDX lists (authoritative for Browsertrix WACZs).
fn collect_page_records_via_cdx<F>(
    fetch: F,
    concurrency: usize,
    progress: Option<&dyn IndexProgress>,
) -> Result<PageRecords>
where
    F: crate::http_range::RangeFetch + Clone + Send + Sync,
{
    use crate::wacz;
    use std::sync::atomic::{AtomicU64, Ordering};

    if let Some(p) = progress {
        p.phase("reading index");
    }
    let read_start = std::time::Instant::now();

    // Setup (serial): read the ZIP central directory, the CDX, each WARC's
    // data-start, and the warcinfo over a buffered range reader.
    let mut zip = zip::ZipArchive::new(crate::http_range::RangeReader::new(fetch.clone()))
        .context("opening WACZ ZIP")?;
    wacz::ensure_warcs_stored(&mut zip)?;
    let cdx = wacz::cdx_records(&mut zip)?;
    let status_counts = tally_status(&cdx);
    let starts = wacz::warc_data_starts(&mut zip)?;
    let warcinfo = wacz::find_warcinfo_streaming(&mut zip)?;
    // Extracted page text from pages.jsonl/extraPages.jsonl (read once, while the
    // ZIP is open). Some crawls store rendered text only here - not as urn:text:
    // WARC records the CDX points at - so without this that text is unsearchable.
    let page_texts = wacz::read_page_texts(&mut zip);
    drop(zip);

    // Records that can become a page; skip media/pseudo-records. The count is the
    // determinate-bar total (each is one fetch + extract).
    let wanted: Vec<&crate::wacz::CdxjRecord> = cdx
        .iter()
        .filter(|c| {
            c.length != 0
                && (c.url.starts_with("urn:text:")
                    || c.mime.contains("html")
                    || c.mime.contains("pdf"))
        })
        .collect();
    if let Some(p) = progress {
        p.set_total(wanted.len() as u64);
    }

    // Fetch + extract each wanted record concurrently. The CDX gives every record
    // an independent (offset, length), so fetches don't depend on each other:
    // fanning out hides per-record round-trip latency (the win for remote WACZs)
    // and parallelizes HTML/PDF text extraction (CPU) across cores.
    let done = AtomicU64::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(concurrency)
        .build()
        .context("building record-fetch thread pool")?;
    let mut out: Vec<RawRecord> = pool.install(|| {
        wanted
            .par_iter()
            .flat_map_iter(|c| {
                let base = c.filename.rsplit('/').next().unwrap_or(&c.filename);
                let raws: Vec<RawRecord> = match starts.get(base) {
                    Some(&start) => {
                        let (from, len) = (start + c.offset, c.length);
                        match fetch
                            .fetch(from, from + len)
                            .map_err(anyhow::Error::from)
                            .and_then(|bytes| wacz::records_from_slice(&bytes, from, len))
                        {
                            Ok(records) => records.iter().filter_map(record_to_raw).collect(),
                            Err(e) => {
                                tracing::warn!(url = %c.url, "skipping unreadable CDX record: {e:#}");
                                Vec::new()
                            }
                        }
                    }
                    None => Vec::new(),
                };
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(p) = progress {
                    p.set_records(n);
                }
                raws
            })
            .collect()
    });

    // Fold in the pages.jsonl/extraPages.jsonl text as rendered-text records;
    // index_merged merges them into the matching page docs by URL (most already
    // exist from their HTML response, so this enriches rather than duplicates).
    let page_text_count = page_texts.len();
    out.extend(page_texts.into_iter().map(|pt| RawRecord::Text {
        url: pt.url,
        timestamp: pt.ts,
        text: pt.text,
        title: pt.title,
    }));

    debug!(
        records = wanted.len(),
        page_texts = page_text_count,
        read_ms = read_start.elapsed().as_millis() as u64,
        concurrency,
        "read page records via CDX"
    );
    // Records read. The merge + Tantivy indexing that follows has no per-record
    // total, so drop the determinate bar back to a spinner - otherwise it sits at
    // 100% with a decaying rate/ETA during the slow tail (very visible for a local
    // file, where reads are near-instant and the tail dominates).
    if let Some(p) = progress {
        p.phase("building index");
    }
    Ok((out, warcinfo, status_counts))
}

/// Cache a representative thumbnail for a crawl (best-effort). Any failure - no
/// main page, no `og:image`, or an image we can't fetch/decode - is logged at
/// debug and ignored; the UI falls back to a CSS placeholder.
fn cache_thumbnail<F>(
    fetch: F,
    thumbs_dir: &Path,
    crawl_id: &str,
    main_page_url: Option<&str>,
    pinned_dest: &Path,
) where
    F: crate::http_range::RangeFetch + Clone + Send + Sync,
{
    let Some(url) = main_page_url else {
        return;
    };
    match crate::thumbnail::generate(fetch, thumbs_dir, crawl_id, url, pinned_dest) {
        Ok(true) => debug!(crawl_id, "cached representative thumbnail"),
        Ok(false) => {}
        Err(e) => debug!(crawl_id, "thumbnail generation failed: {e:#}"),
    }
}

/// Per-host ceiling on fetch concurrency. Even when a user asks for more (or the
/// local core count is very high), we never run more than this many concurrent
/// range requests against a single WACZ's host — a proactive politeness cap that
/// bounds simultaneous load and defends against a mis-typed `--concurrency`. 64 is
/// generous for object stores like S3 while still a hard bound for small servers.
const MAX_CONCURRENCY: usize = 64;

/// Default worker count for the concurrent CDX-guided record fetch+extract, when
/// `--concurrency` isn't given.
///
/// Remote defaults to a deliberately gentle 4: a single WACZ's requests all hit
/// one host, and rustyweb is meant to be pointed at arbitrary (often small)
/// servers, so it's polite by default while still ~4x faster than serial. Users
/// hitting an object store (e.g. S3) can raise it with `--concurrency`.
///
/// Local defaults to the core count: it's your own disk (no politeness concern)
/// and the work is CPU-bound text extraction, so cores are the sweet spot.
fn default_concurrency(remote: bool) -> usize {
    if remote {
        4
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    }
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
            title: None,
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
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
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
            let fetch = crate::http_range::FileFetch::open(&f).unwrap();
            index_wacz_streaming(
                fetch,
                "cid",
                "cname",
                "coll",
                &search,
                fixture_name,
                4,
                None,
            )
            .unwrap()
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
        assert_eq!(
            scan, stream,
            "CDX-guided streaming must index the same page count as scanning"
        );
    }

    #[test]
    fn local_warcs_streamable_gates_the_default_extraction() {
        // The auto-decision for a local file: CDX-guided when WARCs are Stored,
        // else a full scan. a.wacz is Stored; simple.wacz deflates its WARCs.
        assert!(local_warcs_streamable(&fixture("a.wacz")).unwrap());
        assert!(!local_warcs_streamable(&fixture("simple.wacz")).unwrap());
    }

    #[test]
    fn streaming_refuses_a_deflated_wacz() {
        use crate::search::SearchIndex;
        // simple.wacz deflates its WARC entries, which streaming can't seek into.
        let f = fixture("simple.wacz");
        let tmp = TempDir::new().unwrap();
        let search = Mutex::new(SearchIndex::open(tmp.path()).unwrap());
        let fetch = crate::http_range::FileFetch::open(&f).unwrap();
        let err = index_wacz_streaming(
            fetch,
            "cid",
            "cname",
            "coll",
            &search,
            "simple.wacz",
            4,
            None,
        )
        .unwrap_err()
        .to_string()
        .to_lowercase();
        assert!(
            err.contains("stored") || err.contains("compress"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn last_modified_year_parses_http_date() {
        let headers = vec![
            ("Content-Type".to_string(), "text/html".to_string()),
            (
                "Last-Modified".to_string(),
                "Wed, 21 Oct 2015 07:28:00 GMT".to_string(),
            ),
        ];
        assert_eq!(last_modified_year(&headers), Some(2015));
        // Header name match is case-insensitive.
        let headers = vec![(
            "last-modified".to_string(),
            "Mon, 01 Jan 2001 00:00:00 GMT".to_string(),
        )];
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
        let staged = archive.join(name);
        std::fs::copy(fixture(name), &staged).unwrap();
        index_path(&staged, home, display, "test").unwrap();
        // index files the WACZ into the collection's folder (collection "test"),
        // so its resting place is archive/test/<name>.
        archive.join("test").join(name)
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
    fn index_records_capture_status_histogram() {
        // a.wacz is a real Browsertrix WACZ whose CDX carries HTTP statuses; the
        // capture-quality tally should populate from it (task .9).
        let tmp = TempDir::new().unwrap();
        index_fixture("a.wacz", tmp.path(), None);
        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        let counts = &manifest.waczs[0].status_counts;
        assert!(!counts.is_empty(), "CDX statuses should be tallied");
        assert!(
            counts.keys().any(|c| (200..300).contains(c)),
            "a normal crawl is mostly 2xx; got {counts:?}"
        );
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
    fn indexed_local_wacz_is_filed_under_its_collection_relative_to_home() {
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);
        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        // `index` files the WACZ into archive/<collection-slug>/ and stores the
        // source relative to home (so the home dir stays portable).
        assert_eq!(
            manifest.waczs[0].source,
            Source::File(PathBuf::from("archive/test/simple.wacz")),
        );
        assert!(
            tmp.path().join("archive/test/simple.wacz").is_file(),
            "the WACZ was moved into its collection folder"
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
            col.software
                .iter()
                .any(|s| s.contains("Browsertrix-Crawler")),
            "unexpected software: {:?}",
            col.software
        );
        assert!(col.page_count.is_some(), "page_count should be recorded");
    }

    #[test]
    fn browsertrix_provenance_is_recorded_and_survives_reindex() {
        let tmp = TempDir::new().unwrap();
        let dest = index_fixture("simple.wacz", tmp.path(), None);

        set_browsertrix_provenance(
            tmp.path(),
            &dest,
            "https://app.browsertrix.com",
            "item-1",
            "sha256:aa",
            Some(4),
        )
        .unwrap();

        let recorded = |home: &Path| {
            Manifest::open(&home.join("index")).unwrap().waczs[0]
                .browsertrix
                .clone()
        };
        let b = recorded(tmp.path()).expect("provenance recorded");
        assert_eq!(b.item_id, "item-1");
        assert_eq!(b.resource_hash, "sha256:aa");
        assert_eq!(b.review_status, Some(4));

        // A reindex rebuilds each manifest entry from scratch; provenance set
        // out-of-band by the importer must be carried over, not wiped.
        reindex(tmp.path(), None, None, None).unwrap();
        let after = recorded(tmp.path()).expect("provenance after reindex");
        assert_eq!(after.item_id, "item-1");
        assert_eq!(
            after.review_status,
            Some(4),
            "review rating survives reindex"
        );
    }

    /// Build a nested multi-WACZ that wraps the `a.wacz` fixture, mirroring a
    /// real Browsertrix combined download: no top-level archive/ WARCs, the inner
    /// .wacz a top-level *Stored* entry, and a multi-wacz-package datapackage.
    fn nested_multi_wacz() -> Vec<u8> {
        use std::io::Write;
        let inner = std::fs::read(fixture("a.wacz")).unwrap();
        let inner_name = "20250101000000-abc-0.wacz";
        let mut outer = Vec::new();
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let opt = zip::write::SimpleFileOptions::default();
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut outer));
        zw.start_file(inner_name, stored).unwrap();
        zw.write_all(&inner).unwrap();
        zw.start_file("datapackage.json", opt).unwrap();
        let dp = format!(
            r#"{{"profile":"multi-wacz-package","resources":[{{"name":"{inner_name}","path":"{inner_name}"}}]}}"#
        );
        zw.write_all(dp.as_bytes()).unwrap();
        zw.finish().unwrap(); // consumes zw, releasing the borrow of `outer`
        outer
    }

    /// In-memory [`RangeFetch`], a stand-in for a remote `HttpFetch`.
    #[derive(Clone)]
    struct MemFetch(std::sync::Arc<Vec<u8>>);
    impl crate::http_range::RangeFetch for MemFetch {
        fn total_len(&self) -> u64 {
            self.0.len() as u64
        }
        fn fetch(&self, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
            Ok(self.0[start as usize..end as usize].to_vec())
        }
    }

    #[test]
    fn nested_multi_wacz_is_indexed() {
        let outer = nested_multi_wacz();
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let path = archive.join("nested.wacz");
        std::fs::write(&path, &outer).unwrap();
        index_path(&path, tmp.path(), None, "test").unwrap();

        // One manifest entry (approach A: flatten), with the inner crawl's pages
        // and provenance surfaced on it.
        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(manifest.waczs.len(), 1, "one manifest entry per outer file");
        let w = &manifest.waczs[0];
        assert!(
            w.page_count.unwrap_or(0) > 0,
            "the nested WACZ's inner pages should be indexed"
        );
        assert!(
            w.software.iter().any(|s| s.contains("Browsertrix-Crawler")),
            "the inner crawl's software should surface on the outer entry: {:?}",
            w.software
        );
        assert_eq!(
            w.nested_waczs,
            Some(1),
            "the entry should record how many inner WACZs it bundles"
        );
    }

    #[test]
    fn nested_multi_wacz_streams_over_a_range_fetch() {
        // Drive index_nested through an in-memory RangeFetch (a stand-in for a
        // remote HttpFetch) to prove the Stored inner WACZ is read in place via
        // SubRangeFetch — no extraction, no full download.
        let outer = MemFetch(std::sync::Arc::new(nested_multi_wacz()));
        let tmp = TempDir::new().unwrap();
        let search = Mutex::new(SearchIndex::open(&tmp.path().join("ft")).unwrap());

        let stats = index_nested_from(outer, "cid", "Nested", "coll", &search, 2, None)
            .unwrap()
            .expect("should detect and index the nested WACZ");
        assert!(
            stats.pages > 0,
            "inner pages should be indexed by streaming in place"
        );
    }

    #[test]
    fn browsertrix_source_without_resolver_errors_clearly() {
        // A Browsertrix source can't be indexed without a resolver to turn its
        // stable identity into a fresh presigned URL — the error should say so.
        let tmp = TempDir::new().unwrap();
        let loc = "browsertrix|https://app.browsertrix.com|o1|item-1|x-0.wacz";
        let err = index_location(loc, tmp.path(), None, "test", false, None, None)
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("BROWSERTRIX") || err.to_lowercase().contains("credential"),
            "{err}"
        );
    }

    #[test]
    fn index_into_named_collection_groups_the_wacz() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let dest = archive.join("simple.wacz");
        std::fs::copy(fixture("simple.wacz"), &dest).unwrap();

        index_location(
            &dest.to_string_lossy(),
            tmp.path(),
            None,
            "My Project",
            false,
            None,
            None,
        )
        .unwrap();

        let m = crate::collections::Manifest::open(&tmp.path().join("index")).unwrap();
        assert!(
            m.collections
                .iter()
                .any(|c| c.id == "my-project" && c.name == "My Project"),
            "collection should be created: {:?}",
            m.collections.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        assert_eq!(
            m.waczs[0].collection, "my-project",
            "WACZ should reference the collection"
        );
    }

    #[test]
    fn index_copies_external_wacz_into_the_collection_archive() {
        // A WACZ from anywhere is brought into archive/<slug>/ — copied when it's
        // outside the home, leaving the curator's original intact.
        let home = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let stray = elsewhere.path().join("simple.wacz");
        std::fs::copy(fixture("simple.wacz"), &stray).unwrap();

        index_path(&stray, home.path(), None, "My Coll").unwrap();

        assert!(
            stray.is_file(),
            "the external original is left in place (copied, not moved)"
        );
        assert!(
            home.path().join("archive/my-coll/simple.wacz").is_file(),
            "the WACZ is copied into archive/<slug>/"
        );
        let m = Manifest::open(&home.path().join("index")).unwrap();
        assert_eq!(
            m.waczs[0].source,
            Source::File(PathBuf::from("archive/my-coll/simple.wacz"))
        );
    }

    #[test]
    fn index_seeds_collection_finding_aid_from_the_wacz() {
        // Indexing a WACZ pre-seeds its collection's finding aid (fill-gaps) from
        // the datapackage. a.wacz declares created 2026-…, so `dates` is seeded.
        let tmp = TempDir::new().unwrap();
        index_fixture("a.wacz", tmp.path(), None); // collection "test"
        let m = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(
            m.collection_by_id("test").unwrap().dates.as_deref(),
            Some("2026"),
            "collection dates seeded from the WACZ datapackage `created` year"
        );
    }

    #[test]
    fn index_does_not_clobber_two_different_wacz_with_the_same_basename() {
        // The `index a/report.wacz b/report.wacz --collection X` workflow: two
        // DISTINCT files sharing a basename must stay two crawls, not silently
        // collapse into one (regression guard).
        let home = TempDir::new().unwrap();
        let d1 = TempDir::new().unwrap();
        let d2 = TempDir::new().unwrap();
        std::fs::copy(fixture("a.wacz"), d1.path().join("report.wacz")).unwrap();
        std::fs::copy(fixture("simple.wacz"), d2.path().join("report.wacz")).unwrap();

        index_path(&d1.path().join("report.wacz"), home.path(), None, "Reports").unwrap();
        index_path(&d2.path().join("report.wacz"), home.path(), None, "Reports").unwrap();

        let m = Manifest::open(&home.path().join("index")).unwrap();
        assert_eq!(
            m.waczs.len(),
            2,
            "two distinct WACZs must remain two crawls"
        );
        // The second was disambiguated rather than overwriting the first.
        assert!(home.path().join("archive/reports/report.wacz").is_file());
        assert!(home.path().join("archive/reports/report-2.wacz").is_file());

        // Re-indexing the same external file is idempotent (byte-identical → reused).
        index_path(&d1.path().join("report.wacz"), home.path(), None, "Reports").unwrap();
        let m = Manifest::open(&home.path().join("index")).unwrap();
        assert_eq!(
            m.waczs.len(),
            2,
            "re-indexing an identical file must not duplicate"
        );
    }

    #[test]
    fn index_refuses_to_recollect_a_registered_crawl() {
        // Indexing a WACZ that's already filed in one collection into a different
        // one is refused (moving it would change its id and orphan its assets).
        let home = TempDir::new().unwrap();
        index_fixture("simple.wacz", home.path(), None); // collection "test"
        let filed = home.path().join("archive/test/simple.wacz");
        assert!(filed.is_file());

        let err = index_path(&filed, home.path(), None, "Other")
            .expect_err("re-collecting a filed crawl should be refused");
        assert!(
            format!("{err:#}").contains("already in collection"),
            "error should explain the crawl is already collected: {err:#}"
        );
    }

    #[test]
    fn index_rejects_a_directory() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();

        let err = index_path(&archive, tmp.path(), None, "test")
            .expect_err("indexing a directory should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("directory"),
            "error should say it is a directory: {msg}"
        );
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

        reindex(tmp.path(), None, None, None).unwrap();

        // The manifest (custom name + collection membership) is preserved...
        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(manifest.waczs.len(), 1);
        assert_eq!(manifest.waczs[0].name, "keepname");
        assert_eq!(
            manifest.waczs[0].collection, "test",
            "collection membership must survive a reindex"
        );

        // ...and the content is searchable again.
        let idx = crate::search::SearchIndex::open(full_text.as_path()).unwrap();
        assert!(
            !idx.search("example", 10).unwrap().is_empty(),
            "reindexed content should be searchable"
        );
    }

    #[test]
    fn reindex_with_no_collections_is_ok() {
        let tmp = TempDir::new().unwrap();
        // No collections.json yet: reindex should be a no-op, not an error.
        reindex(tmp.path(), None, None, None).unwrap();
    }

    #[test]
    fn reindex_skips_a_failing_source_and_keeps_going() {
        // A resilient reindex: one good WACZ plus one that exists but isn't a
        // valid WACZ. The bad source is skipped (warned) rather than aborting the
        // whole rebuild, so the good source is still indexed and searchable, and
        // the skipped source's manifest entry is preserved for a later re-run.
        let tmp = TempDir::new().unwrap();
        index_fixture("simple.wacz", tmp.path(), None);

        // Plant a corrupt WACZ and register it as a member alongside the good one.
        std::fs::write(tmp.path().join("archive/bad.wacz"), b"not a zip file").unwrap();
        let waczs_path = tmp.path().join("index/waczs.json");
        let mut entries: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&waczs_path).unwrap()).unwrap();
        entries.push(serde_json::json!({
            "id": "deadbeef",
            "collection": "deadbeef",
            "source": "archive/bad.wacz",
            "name": "BadOne",
            "date_indexed": "2026-01-01T00:00:00Z",
            "file_size": 14,
            "sha256": "00"
        }));
        std::fs::write(&waczs_path, serde_json::to_string(&entries).unwrap()).unwrap();

        // Rebuild from the manifest: the run completes over the good source but
        // reports a non-zero exit (an error) because one source was skipped.
        let err = reindex(tmp.path(), None, None, None)
            .expect_err("a skipped source should surface as a non-zero exit, not abort mid-run");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("skipped") && msg.contains("reindex"),
            "error should summarize the skipped source(s) and suggest re-running: {msg}"
        );

        // ...yet the good source is still fully indexed and searchable.
        let idx =
            crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path())
                .unwrap();
        assert!(
            !idx.search("example", 10).unwrap().is_empty(),
            "the good source should still be indexed after skipping the bad one"
        );

        // ...and the skipped source's manifest entry is preserved (not dropped),
        // so `rustyweb reindex` can pick it up again once the cause is fixed.
        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        assert_eq!(
            manifest.waczs.len(),
            2,
            "skipped source's manifest entry should be preserved"
        );
    }

    #[test]
    fn pages_jsonl_text_is_indexed_when_absent_from_html() {
        use std::io::Write;
        // A crawl whose rendered text lives ONLY in pages.jsonl (older
        // Browsertrix/SUCHO WACZs write it there, not as urn:text: records): the
        // HTML body lacks the term, but the pages.jsonl `text` field has it. It
        // must still be searchable, via the default CDX-guided path.
        let term = "zqxsentinel"; // unique token, present only in pages.jsonl text
        let url = "https://ex.com/";

        // One WARC response record whose HTML does NOT contain the term.
        let http = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
             <html><head><title>Home Page</title></head><body>nothing useful</body></html>";
        let mut warc = format!(
            "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: {url}\r\n\
             WARC-Date: 2022-01-01T00:00:00Z\r\n\
             Content-Type: application/http; msgtype=response\r\nContent-Length: {}\r\n\r\n",
            http.len()
        )
        .into_bytes();
        warc.extend_from_slice(http);
        warc.extend_from_slice(b"\r\n\r\n");
        let gz = |b: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b).unwrap();
            e.finish().unwrap()
        };
        let warc_gz = gz(&warc); // single gzip member => offset 0 in data.warc.gz

        // CDX pointing at that record (whole member).
        let cdx_line = format!(
            "com,ex)/ 20220101000000 {{\"url\":\"{url}\",\"mime\":\"text/html\",\
             \"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":0,\"length\":{}}}\n",
            warc_gz.len()
        );
        let cdx_gz = gz(cdx_line.as_bytes());

        // pages.jsonl: header + a page whose `text` carries the term (+Cyrillic).
        let pages = format!(
            "{{\"format\":\"json-pages-1.0\",\"id\":\"pages\",\"title\":\"All Pages\"}}\n\
             {{\"id\":\"p1\",\"url\":\"{url}\",\"title\":\"Home Page\",\
             \"ts\":\"2022-01-01T00:00:00Z\",\"text\":\"Петиція {term}\"}}\n"
        );

        // Assemble the WACZ. The WARC must be Stored so it takes the CDX-guided path.
        let mut wacz = Vec::new();
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let opt = zip::write::SimpleFileOptions::default();
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut wacz));
            zw.start_file("archive/data.warc.gz", stored).unwrap();
            zw.write_all(&warc_gz).unwrap();
            zw.start_file("indexes/index.cdx.gz", opt).unwrap();
            zw.write_all(&cdx_gz).unwrap();
            zw.start_file("pages/pages.jsonl", opt).unwrap();
            zw.write_all(pages.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let path = archive.join("crawl.wacz");
        std::fs::write(&path, &wacz).unwrap();
        assert!(
            local_warcs_streamable(&path).unwrap(),
            "stored WARC should take the CDX-guided path"
        );
        index_path(&path, tmp.path(), None, "test").unwrap();

        let idx =
            crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path())
                .unwrap();
        assert!(
            idx.search(term, 10)
                .unwrap()
                .iter()
                .any(|r| r.doc_type == "page" && r.url == url),
            "text from pages.jsonl must be searchable (term absent from the HTML)"
        );
        assert!(
            !idx.search("Петиція", 10).unwrap().is_empty(),
            "Cyrillic rendered text from pages.jsonl must be searchable"
        );
        assert!(
            !idx.search("Home Page", 10).unwrap().is_empty(),
            "the HTML <title> is still indexed"
        );
    }

    #[test]
    fn og_image_thumbnail_is_cached() {
        use std::io::Write;
        // End-to-end: a crawl whose main page declares an og:image pointing at a
        // captured PNG should get a downscaled JPEG thumbnail cached under
        // <home>/index/thumbs.
        let page_url = "https://ex.com/";
        let img_url = "https://ex.com/preview.png";

        // The captured og:image (a small PNG).
        let mut png_buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            24,
            16,
            image::Rgb([210, 90, 70]),
        ))
        .write_to(&mut png_buf, image::ImageFormat::Png)
        .unwrap();
        let png = png_buf.into_inner();

        let html = format!(
            "<html><head><title>Home</title>\
             <meta property=\"og:image\" content=\"{img_url}\"></head><body>hi</body></html>"
        );

        // One WARC response record, gzipped as a single member.
        let gz_record = |url: &str, ctype: &str, body: &[u8]| {
            let mut http = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n\r\n").into_bytes();
            http.extend_from_slice(body);
            let mut warc = format!(
                "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: {url}\r\n\
                 WARC-Date: 2022-01-01T00:00:00Z\r\n\
                 Content-Type: application/http; msgtype=response\r\nContent-Length: {}\r\n\r\n",
                http.len()
            )
            .into_bytes();
            warc.extend_from_slice(&http);
            warc.extend_from_slice(b"\r\n\r\n");
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&warc).unwrap();
            e.finish().unwrap()
        };
        let html_member = gz_record(page_url, "text/html", html.as_bytes());
        let png_member = gz_record(img_url, "image/png", &png);
        let mut warc_gz = html_member.clone();
        warc_gz.extend_from_slice(&png_member);

        let gz = |b: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b).unwrap();
            e.finish().unwrap()
        };
        // CDX: HTML at offset 0, the PNG right after it.
        let cdx = format!(
            "com,ex)/ 20220101000000 {{\"url\":\"{page_url}\",\"mime\":\"text/html\",\
             \"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":0,\"length\":{}}}\n\
             com,ex)/preview.png 20220101000000 {{\"url\":\"{img_url}\",\"mime\":\"image/png\",\
             \"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":{},\"length\":{}}}\n",
            html_member.len(),
            html_member.len(),
            png_member.len()
        );
        let cdx_gz = gz(cdx.as_bytes());
        let datapackage = format!("{{\"mainPageUrl\":\"{page_url}\"}}");

        let mut wacz = Vec::new();
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let opt = zip::write::SimpleFileOptions::default();
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut wacz));
            zw.start_file("archive/data.warc.gz", stored).unwrap();
            zw.write_all(&warc_gz).unwrap();
            zw.start_file("indexes/index.cdx.gz", opt).unwrap();
            zw.write_all(&cdx_gz).unwrap();
            zw.start_file("datapackage.json", opt).unwrap();
            zw.write_all(datapackage.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let path = archive.join("crawl.wacz");
        std::fs::write(&path, &wacz).unwrap();
        index_path(&path, tmp.path(), None, "test").unwrap();

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        let id = &manifest.waczs[0].id;
        let thumb = tmp
            .path()
            .join("index")
            .join("thumbs")
            .join(format!("{id}.jpg"));
        assert!(
            thumb.exists(),
            "a crawl with an og:image should get a cached thumbnail"
        );
        let decoded = image::load_from_memory(&std::fs::read(&thumb).unwrap()).unwrap();
        assert!(
            decoded.width() <= 400 && decoded.height() <= 400,
            "thumbnail should be downscaled"
        );
    }

    #[test]
    fn browsertrix_screenshot_is_preferred_over_og_image() {
        use std::io::Write;
        // A crawl that has BOTH an og:image and a Browsertrix screenshot
        // (urn:thumbnail:<page>) should thumbnail from the *screenshot* — it's an
        // actual picture of the page. We tell them apart by aspect ratio:
        // thumbnail() scales to fit 400px preserving aspect, so the 40x30 (4:3)
        // screenshot yields 400x300, whereas the 24x16 (3:2) og:image would yield
        // 400x266.
        let page_url = "https://ex.com/";
        let og_url = "https://ex.com/preview.png";

        let png = |w: u32, h: u32| {
            let mut b = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                w,
                h,
                image::Rgb([9, 9, 9]),
            ))
            .write_to(&mut b, image::ImageFormat::Png)
            .unwrap();
            b.into_inner()
        };
        let jpeg = |w: u32, h: u32| {
            let mut b = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                w,
                h,
                image::Rgb([9, 9, 9]),
            ))
            .write_to(&mut b, image::ImageFormat::Jpeg)
            .unwrap();
            b.into_inner()
        };
        let og_png = png(24, 16);
        let shot = jpeg(40, 30);

        let html = format!(
            "<html><head><meta property=\"og:image\" content=\"{og_url}\"></head>\
             <body>hi</body></html>"
        );

        let gz_record = |url: &str, ctype: &str, body: &[u8]| {
            let mut http = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n\r\n").into_bytes();
            http.extend_from_slice(body);
            let mut warc = format!(
                "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: {url}\r\n\
                 WARC-Date: 2022-01-01T00:00:00Z\r\n\
                 Content-Type: application/http; msgtype=response\r\nContent-Length: {}\r\n\r\n",
                http.len()
            )
            .into_bytes();
            warc.extend_from_slice(&http);
            warc.extend_from_slice(b"\r\n\r\n");
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&warc).unwrap();
            e.finish().unwrap()
        };
        let html_m = gz_record(page_url, "text/html", html.as_bytes());
        let og_m = gz_record(og_url, "image/png", &og_png);
        let shot_m = gz_record(&format!("urn:thumbnail:{page_url}"), "image/jpeg", &shot);
        let mut warc_gz = html_m.clone();
        warc_gz.extend_from_slice(&og_m);
        warc_gz.extend_from_slice(&shot_m);

        let gz = |b: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b).unwrap();
            e.finish().unwrap()
        };
        let cdx = format!(
            "com,ex)/ 20220101000000 {{\"url\":\"{page_url}\",\"mime\":\"text/html\",\
             \"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":0,\"length\":{}}}\n\
             com,ex)/preview.png 20220101000000 {{\"url\":\"{og_url}\",\"mime\":\"image/png\",\
             \"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":{},\"length\":{}}}\n\
             urn:thumbnail:{page_url} 20220101000000 {{\"url\":\"urn:thumbnail:{page_url}\",\
             \"mime\":\"image/jpeg\",\"status\":\"200\",\"filename\":\"data.warc.gz\",\
             \"offset\":{},\"length\":{}}}\n",
            html_m.len(),
            html_m.len(),
            og_m.len(),
            html_m.len() + og_m.len(),
            shot_m.len(),
        );
        let cdx_gz = gz(cdx.as_bytes());
        let datapackage = format!("{{\"mainPageUrl\":\"{page_url}\"}}");

        let mut wacz = Vec::new();
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let opt = zip::write::SimpleFileOptions::default();
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut wacz));
            zw.start_file("archive/data.warc.gz", stored).unwrap();
            zw.write_all(&warc_gz).unwrap();
            zw.start_file("indexes/index.cdx.gz", opt).unwrap();
            zw.write_all(&cdx_gz).unwrap();
            zw.start_file("datapackage.json", opt).unwrap();
            zw.write_all(datapackage.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let path = archive.join("crawl.wacz");
        std::fs::write(&path, &wacz).unwrap();
        index_path(&path, tmp.path(), None, "test").unwrap();

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        let id = &manifest.waczs[0].id;
        let thumb = tmp
            .path()
            .join("index")
            .join("thumbs")
            .join(format!("{id}.jpg"));
        assert!(thumb.exists(), "a screenshot should produce a thumbnail");
        let decoded = image::load_from_memory(&std::fs::read(&thumb).unwrap()).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (400, 300),
            "the thumbnail should come from the 4:3 screenshot, not the 3:2 og:image"
        );
    }

    #[test]
    fn thumbnail_falls_back_to_largest_page_image() {
        use std::io::Write;
        // The main page has NO og:image but embeds two images (a tiny icon and a
        // larger hero). The thumbnail should fall back to the largest captured
        // content image, skipping the sub-threshold icon.
        let page_url = "https://ex.com/";

        // A "noisy" hero PNG (poor compression → well over the 5 KB floor) and a
        // tiny icon (under it, so it's skipped).
        let png = |w: u32, h: u32, noisy: bool| {
            let mut im = image::RgbImage::new(w, h);
            // Pseudo-random (incompressible) pixels so the PNG stays large; a solid
            // fill for the tiny icon so it compresses well below the floor.
            let mut st: u32 = 0x1234_5678;
            for (_, _, p) in im.enumerate_pixels_mut() {
                if noisy {
                    st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let b = st.to_le_bytes();
                    *p = image::Rgb([b[0], b[1], b[2]]);
                } else {
                    *p = image::Rgb([200, 200, 200]);
                }
            }
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(im)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };
        let hero = png(160, 160, true);
        let icon = png(8, 8, false);
        assert!(
            hero.len() >= 5000 && icon.len() < 5000,
            "test image sizes bracket the floor"
        );

        let html = "<html><head><title>Home</title></head><body>\
             <img src=\"icon.png\"><img src=\"hero.png\"></body></html>";

        let gz_record = |url: &str, ctype: &str, body: &[u8]| {
            let mut http = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n\r\n").into_bytes();
            http.extend_from_slice(body);
            let mut warc = format!(
                "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: {url}\r\n\
                 WARC-Date: 2023-01-01T00:00:00Z\r\n\
                 Content-Type: application/http; msgtype=response\r\nContent-Length: {}\r\n\r\n",
                http.len()
            )
            .into_bytes();
            warc.extend_from_slice(&http);
            warc.extend_from_slice(b"\r\n\r\n");
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&warc).unwrap();
            e.finish().unwrap()
        };
        let m_html = gz_record(page_url, "text/html", html.as_bytes());
        let m_icon = gz_record("https://ex.com/icon.png", "image/png", &icon);
        let m_hero = gz_record("https://ex.com/hero.png", "image/png", &hero);
        let mut warc_gz = m_html.clone();
        warc_gz.extend_from_slice(&m_icon);
        warc_gz.extend_from_slice(&m_hero);

        let gz = |b: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b).unwrap();
            e.finish().unwrap()
        };
        let (o_html, o_icon) = (0, m_html.len());
        let o_hero = m_html.len() + m_icon.len();
        let cdx = format!(
            "com,ex)/ 20230101000000 {{\"url\":\"{page_url}\",\"mime\":\"text/html\",\"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":{o_html},\"length\":{}}}\n\
             com,ex)/icon.png 20230101000000 {{\"url\":\"https://ex.com/icon.png\",\"mime\":\"image/png\",\"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":{o_icon},\"length\":{}}}\n\
             com,ex)/hero.png 20230101000000 {{\"url\":\"https://ex.com/hero.png\",\"mime\":\"image/png\",\"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":{o_hero},\"length\":{}}}\n",
            m_html.len(), m_icon.len(), m_hero.len()
        );
        let cdx_gz = gz(cdx.as_bytes());
        let datapackage = format!("{{\"mainPageUrl\":\"{page_url}\"}}");

        let mut wacz = Vec::new();
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let opt = zip::write::SimpleFileOptions::default();
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut wacz));
            zw.start_file("archive/data.warc.gz", stored).unwrap();
            zw.write_all(&warc_gz).unwrap();
            zw.start_file("indexes/index.cdx.gz", opt).unwrap();
            zw.write_all(&cdx_gz).unwrap();
            zw.start_file("datapackage.json", opt).unwrap();
            zw.write_all(datapackage.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let path = archive.join("crawl.wacz");
        std::fs::write(&path, &wacz).unwrap();
        index_path(&path, tmp.path(), None, "test").unwrap();

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        let id = &manifest.waczs[0].id;
        let thumb = tmp
            .path()
            .join("index")
            .join("thumbs")
            .join(format!("{id}.jpg"));
        assert!(
            thumb.exists(),
            "with no og:image, a thumbnail should be generated from the largest embedded image"
        );
    }

    #[test]
    fn thumbnail_falls_back_to_largest_on_site_captured_image() {
        use std::io::Write;
        // A JS-rendered site: the saved HTML has NO og:image and NO <img>, but the
        // crawl captured images. The thumbnail should come from the largest
        // in-window raster image ON THE CRAWL'S OWN DOMAIN — a bigger off-domain
        // (CDN/ad) image must be ignored.
        let page_url = "https://ex.com/";

        // Distinct aspect ratios so we can tell which image was chosen: the on-site
        // one is landscape, the (larger, must-ignore) off-site one is portrait.
        let png = |w: u32, h: u32| {
            let mut im = image::RgbImage::new(w, h);
            let mut st: u32 = 0x9e37_79b9;
            for (_, _, p) in im.enumerate_pixels_mut() {
                st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let b = st.to_le_bytes();
                *p = image::Rgb([b[0], b[1], b[2]]);
            }
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(im)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };
        let onsite = png(240, 120); // landscape (w > h)
        let offsite = png(160, 320); // portrait, larger byte size
        assert!(offsite.len() > onsite.len() && onsite.len() >= 5000);

        let html = "<html><head><title>Home</title></head><body>no images here</body></html>";

        let gz_record = |url: &str, ctype: &str, body: &[u8]| {
            let mut http = format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n\r\n").into_bytes();
            http.extend_from_slice(body);
            let mut warc = format!(
                "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: {url}\r\n\
                 WARC-Date: 2023-01-01T00:00:00Z\r\n\
                 Content-Type: application/http; msgtype=response\r\nContent-Length: {}\r\n\r\n",
                http.len()
            )
            .into_bytes();
            warc.extend_from_slice(&http);
            warc.extend_from_slice(b"\r\n\r\n");
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&warc).unwrap();
            e.finish().unwrap()
        };
        let m_html = gz_record(page_url, "text/html", html.as_bytes());
        let m_on = gz_record("https://ex.com/photo.jpg", "image/jpeg", &onsite);
        let m_off = gz_record("https://cdn.other.com/ad.jpg", "image/jpeg", &offsite);
        let mut warc_gz = m_html.clone();
        warc_gz.extend_from_slice(&m_on);
        warc_gz.extend_from_slice(&m_off);

        let gz = |b: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b).unwrap();
            e.finish().unwrap()
        };
        let o_on = m_html.len();
        let o_off = m_html.len() + m_on.len();
        let cdx = format!(
            "com,ex)/ 20230101000000 {{\"url\":\"{page_url}\",\"mime\":\"text/html\",\"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":0,\"length\":{}}}\n\
             com,ex)/photo.jpg 20230101000000 {{\"url\":\"https://ex.com/photo.jpg\",\"mime\":\"image/jpeg\",\"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":{o_on},\"length\":{}}}\n\
             com,other,cdn)/ad.jpg 20230101000000 {{\"url\":\"https://cdn.other.com/ad.jpg\",\"mime\":\"image/jpeg\",\"status\":\"200\",\"filename\":\"data.warc.gz\",\"offset\":{o_off},\"length\":{}}}\n",
            m_html.len(), m_on.len(), m_off.len()
        );
        let cdx_gz = gz(cdx.as_bytes());
        let datapackage = format!("{{\"mainPageUrl\":\"{page_url}\"}}");

        let mut wacz = Vec::new();
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let opt = zip::write::SimpleFileOptions::default();
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut wacz));
            zw.start_file("archive/data.warc.gz", stored).unwrap();
            zw.write_all(&warc_gz).unwrap();
            zw.start_file("indexes/index.cdx.gz", opt).unwrap();
            zw.write_all(&cdx_gz).unwrap();
            zw.start_file("datapackage.json", opt).unwrap();
            zw.write_all(datapackage.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let path = archive.join("crawl.wacz");
        std::fs::write(&path, &wacz).unwrap();
        index_path(&path, tmp.path(), None, "test").unwrap();

        let manifest = Manifest::open(&tmp.path().join("index")).unwrap();
        let id = &manifest.waczs[0].id;
        let thumb = tmp
            .path()
            .join("index")
            .join("thumbs")
            .join(format!("{id}.jpg"));
        assert!(
            thumb.exists(),
            "a JS-rendered crawl should still get a thumbnail from a captured on-site image"
        );
        let decoded = image::load_from_memory(&std::fs::read(&thumb).unwrap()).unwrap();
        assert!(
            decoded.width() > decoded.height(),
            "the on-site landscape image should be chosen, not the larger off-site portrait one"
        );
    }

    #[test]
    fn pdf_pages_are_filterable_by_type() {
        // End-to-end: a PDF response in the WACZ should be tagged type:pdf so
        // it can be filtered from the search box.
        let tmp = TempDir::new().unwrap();
        index_fixture("pdf-doc.wacz", tmp.path(), None);

        let idx =
            crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path())
                .unwrap();
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

        let idx =
            crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path())
                .unwrap();
        let results = idx.search("example", 10).unwrap();
        assert!(!results.is_empty(), "should find HTML content from WACZ");
        assert_eq!(results[0].crawl_name, "simple");
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

        let idx =
            crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path())
                .unwrap();
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

        let idx =
            crate::search::SearchIndex::open(tmp.path().join("index").join("full_text").as_path())
                .unwrap();
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
