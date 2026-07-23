use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeedPage {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub ts: String,
}

/// Where a collection's WACZ lives: a local file, a remote http(s) URL, or a
/// resource in a Browsertrix instance that must be re-resolved to a fresh
/// presigned URL on demand (see [`Source::Browsertrix`]).
///
/// Serializes as a plain string (the path/URL, or `browsertrix|…`) for a
/// readable manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Source {
    File(PathBuf),
    Url(String),
    /// A WACZ resource inside a Browsertrix instance, identified stably by
    /// `host` + `org` + archived-item `item` + `resource` filename. Browsertrix
    /// serves WACZs via **presigned URLs that expire (~48 h)**, so we store the
    /// identity, not a URL, and re-resolve a fresh presigned URL each time we
    /// index or replay it (via a [`crate::index::SourceResolver`], which holds
    /// the credentials). Stored as `browsertrix|host|org|item|resource`.
    Browsertrix {
        host: String,
        org: String,
        item: String,
        resource: String,
    },
}

impl Source {
    /// Parse a location string: `browsertrix|…` is a Browsertrix resource,
    /// `http://`/`https://` a URL, else a file path.
    pub fn parse(s: &str) -> Self {
        if let Some(rest) = s.strip_prefix("browsertrix|") {
            // host|org|item|resource — split into exactly 4; the resource is the
            // remainder, so a `|` in a filename (unheard of from Browsertrix)
            // lands harmlessly in the last field.
            let p: Vec<&str> = rest.splitn(4, '|').collect();
            if p.len() == 4 {
                return Source::Browsertrix {
                    host: p[0].to_string(),
                    org: p[1].to_string(),
                    item: p[2].to_string(),
                    resource: p[3].to_string(),
                };
            }
        }
        if s.starts_with("http://") || s.starts_with("https://") {
            Source::Url(s.to_string())
        } else {
            Source::File(PathBuf::from(s))
        }
    }

    pub fn is_url(&self) -> bool {
        matches!(self, Source::Url(_))
    }

    /// Whether replaying/verifying this source needs a live fetch rather than a
    /// local file (a URL or a Browsertrix resource).
    pub fn is_remote(&self) -> bool {
        !matches!(self, Source::File(_))
    }

    /// The local file path, if this is a file source.
    pub fn as_file(&self) -> Option<&Path> {
        match self {
            Source::File(p) => Some(p.as_path()),
            Source::Url(_) | Source::Browsertrix { .. } => None,
        }
    }

    /// Stable string form: the file path, the URL, or `browsertrix|…`.
    pub fn location(&self) -> String {
        match self {
            Source::File(p) => p.to_string_lossy().into_owned(),
            Source::Url(u) => u.clone(),
            Source::Browsertrix {
                host,
                org,
                item,
                resource,
            } => format!("browsertrix|{host}|{org}|{item}|{resource}"),
        }
    }

    /// Build a File source for an absolute path, stored relative to `home` when
    /// the path is under it (so the home folder is portable), else absolute.
    pub fn for_file(abs: &Path, home: &Path) -> Source {
        let home_abs = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        match abs.strip_prefix(&home_abs) {
            Ok(rel) => Source::File(rel.to_path_buf()),
            Err(_) => Source::File(abs.to_path_buf()),
        }
    }

    /// Resolve a File source to a concrete path against `home`: relative paths
    /// are joined to `home`, absolute paths are returned as-is. `None` for URLs.
    pub fn resolve(&self, home: &Path) -> Option<PathBuf> {
        match self {
            Source::File(p) if p.is_absolute() => Some(p.clone()),
            Source::File(p) => Some(home.join(p)),
            Source::Url(_) | Source::Browsertrix { .. } => None,
        }
    }
}

impl From<String> for Source {
    fn from(s: String) -> Self {
        Source::parse(&s)
    }
}

impl From<Source> for String {
    fn from(s: Source) -> Self {
        s.location()
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.location())
    }
}

/// A single WACZ file in the archive - one member of a [`Collection`]. (This was
/// previously the top-level `Collection`; a curated `Collection` now groups many
/// of these.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wacz {
    pub id: String,
    /// Id of the [`Collection`] this WACZ belongs to. Defaults to the WACZ's own
    /// id (a singleton collection) when not explicitly grouped.
    #[serde(default)]
    pub collection: String,
    /// The WACZ location. Older manifests used the key `path`.
    #[serde(alias = "path")]
    pub source: Source,
    pub name: String,
    pub date_indexed: String,
    pub file_size: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawl_date: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seed_pages: Vec<SeedPage>,

    // ── Provenance (from datapackage.json + the WARC warcinfo record) ──
    /// Software that produced this archive, as reported by the WACZ
    /// `datapackage.json` and/or the WARC `warcinfo` record (e.g.
    /// `Browsertrix-Crawler 1.13.0`, `py-wacz 0.4.6`). We do not try to label
    /// which entry crawled vs packaged - the formats don't distinguish - so this
    /// is just the set of tools involved, joined for display at the UI level.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "string_or_seq"
    )]
    pub software: Vec<String>,
    /// Contact for the operator who ran the crawl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    /// User-Agent the crawler sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// How the crawler handled robots.txt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub robots: Option<String>,
    /// Number of pages indexed from this WACZ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u64>,
    /// Earliest capture timestamp seen (14-digit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_start: Option<String>,
    /// Latest capture timestamp seen (14-digit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_end: Option<String>,

    /// Where this WACZ was imported from, when it came via `rustyweb
    /// browsertrix`. Drives incremental re-sync (skip already-imported items)
    /// and attributes provenance. Absent for hand-indexed WACZs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browsertrix: Option<BrowsertrixRef>,
    /// For a nested multi-WACZ (a WACZ bundling other WACZs), the number of inner
    /// WACZs flattened into this crawl. `None` for an ordinary (flat) WACZ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_waczs: Option<u64>,

    // ── Provenance fields previously parsed-but-dropped, or newly read (populate
    //    on reindex; all conditional so un-reindexed crawls just show less) ──
    /// WACZ last-modified time (datapackage `modified`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// Collection/crawl this WARC declares membership in (warcinfo `isPartOf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_part_of: Option<String>,
    /// Host the crawl ran on (warcinfo `hostname`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// WARC spec the crawl conforms to (warcinfo `conformsTo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conforms_to: Option<String>,
    /// Topical keywords (datapackage `keywords`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// License labels (datapackage `licenses`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub licenses: Vec<String>,
    /// HTTP status-code histogram across all captures (from the CDX) — the
    /// derived "capture quality" / DACS Appraisal signal. Empty until reindex.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub status_counts: BTreeMap<u16, u64>,
}

impl Wacz {
    /// Whether the WACZ is available, resolving file paths against `home`.
    /// Local files must exist on disk; remote URLs are assumed present.
    pub fn is_present(&self, home: &Path) -> bool {
        match self.source.resolve(home) {
            Some(path) => path.exists(),
            None => true, // URL source
        }
    }
}

/// Provenance for a WACZ imported from a Browsertrix instance (`rustyweb
/// browsertrix`). The `(host, item_id, resource_hash)` triple lets a re-run skip
/// an item that's already indexed without re-downloading it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowsertrixRef {
    /// The Browsertrix host the item came from.
    pub host: String,
    /// The Browsertrix archived-item id (a crawl or an upload).
    pub item_id: String,
    /// The WACZ resource content hash from `replay.json` (e.g. `sha256:…`), when
    /// present — the strongest signal that content is unchanged.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resource_hash: String,
    /// Browsertrix QA review rating (1–5, Excellent→Bad), if a human reviewed the
    /// crawl. A DACS Appraisal signal surfaced on the crawl page. `None` if
    /// unreviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_status: Option<u8>,
}

/// A curated collection: a named group of [`Wacz`] members with its own
/// curatorial (finding-aid) metadata. Aggregates (member count, size, capture
/// range, software) are derived from members at read time, not stored here.
///
/// The descriptive metadata is the *source of truth* in a git-committable
/// Markdown finding aid at `<home>/collections/<id>.md` — YAML front-matter for
/// the short structured fields, and a Markdown body for the `narrative` (Scope
/// & Content / Custodial history / Appraisal prose). See [`load_finding_aids`]
/// / [`write_finding_aid`]. Fields are framed against DACS / EAD.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Collection {
    pub id: String,
    pub name: String,
    /// A short abstract / caption (EAD `<abstract>`), distinct from the longer
    /// `narrative`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// When the collection was first created (RFC 3339).
    pub created: String,
    /// Who runs this rustyweb instance / holds the collection (EAD
    /// `<repository>`), distinct from `creator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curator: Option<String>,

    // ── Finding-aid front-matter (DACS / EAD) ──
    /// Collecting org/person responsible for the records (EAD `<origination>`,
    /// DACS Name of Creator) — distinct from `curator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    /// Curatorial coverage statement (EAD `<unitdate>`, DACS Date), distinct
    /// from the auto-derived capture range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dates: Option<String>,
    /// Conditions governing access and use (EAD `<accessrestrict>` +
    /// `<userestrict>`, DACS 4.1/4.4) — one field labelled to cover both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rights: Option<String>,
    /// Topical access points (EAD `<controlaccess>`, DACS Subject).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<String>,

    // ── Finding-aid body ──
    /// The Markdown narrative: Scope & Content plus Custodial history /
    /// Appraisal, written as the curator sees fit (EAD `<scopecontent>` /
    /// `<custodhist>` / `<appraisal>`). Stored as the Markdown body of the
    /// finding-aid file, not in front-matter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrative: Option<String>,
}

/// The on-disk manifest. WACZ members (membership + derived provenance) live in
/// the *derived* `<home>/index/waczs.json`; collection descriptive metadata is
/// the *source of truth* in git-committable Markdown finding aids at
/// `<home>/collections/<id>.md` (see [`load_finding_aids`]). A legacy
/// `index/collections.json` is read once and migrated to `.md` on the next save.
pub struct Manifest {
    index_dir: PathBuf,
    /// Rustyweb home (`index_dir`'s parent); holds `collections/` + `crawls/`.
    home: PathBuf,
    pub collections: Vec<Collection>,
    pub waczs: Vec<Wacz>,
    /// Collection ids whose finding aid needs (re)writing on `save` — the set
    /// created/modified this session, or migrated from legacy JSON. Untouched
    /// finding aids are never rewritten, so hand edits keep their formatting.
    dirty: HashSet<String>,
}

impl Manifest {
    pub fn open(index_dir: &Path) -> Result<Self> {
        let home = index_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| index_dir.to_path_buf());
        let collections_path = index_dir.join("collections.json");
        let waczs_path = index_dir.join("waczs.json");
        let collections_dir = home.join("collections");
        let mut dirty = HashSet::new();

        // ── WACZ members (derived index) ──
        let waczs: Vec<Wacz> = if waczs_path.exists() {
            read_json(&waczs_path)?.unwrap_or_default()
        } else if collections_path.exists() && legacy_json_holds_waczs(&collections_path)? {
            // Oldest layout: `collections.json` held the WACZ records directly.
            let value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&collections_path)?)?;
            let mut waczs: Vec<Wacz> = serde_json::from_value(value)?;
            for w in &mut waczs {
                if w.collection.is_empty() {
                    w.collection = w.id.clone();
                }
            }
            waczs
        } else {
            Vec::new()
        };

        // ── Collection descriptive metadata (finding aids, source of truth) ──
        let collections: Vec<Collection> = if dir_has_findingaid(&collections_dir) {
            load_finding_aids(&collections_dir)?
        } else if collections_path.exists() {
            // Migrate from legacy `collections.json`, then write `.md` on save.
            let value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&collections_path)?)?;
            let cols: Vec<Collection> = if legacy_json_holds_waczs(&collections_path)? {
                // Synthesize a singleton collection per legacy WACZ.
                waczs
                    .iter()
                    .map(|w| Collection {
                        id: w.id.clone(),
                        name: w.name.clone(),
                        description: w.description.clone(),
                        created: w.date_indexed.clone(),
                        ..Default::default()
                    })
                    .collect()
            } else {
                serde_json::from_value(value).unwrap_or_default()
            };
            for c in &cols {
                dirty.insert(c.id.clone());
            }
            cols
        } else {
            Vec::new()
        };

        Ok(Self {
            index_dir: index_dir.to_path_buf(),
            home,
            collections,
            waczs,
            dirty,
        })
    }

    /// Insert or replace a WACZ member by id.
    pub fn upsert_wacz(&mut self, wacz: Wacz) {
        if let Some(pos) = self.waczs.iter().position(|w| w.id == wacz.id) {
            self.waczs[pos] = wacz;
        } else {
            self.waczs.push(wacz);
        }
    }

    /// Insert or replace a collection by id (marks its finding aid for writing).
    pub fn upsert_collection(&mut self, collection: Collection) {
        self.dirty.insert(collection.id.clone());
        if let Some(pos) = self.collections.iter().position(|c| c.id == collection.id) {
            self.collections[pos] = collection;
        } else {
            self.collections.push(collection);
        }
    }

    /// Ensure a collection with `id` exists, creating a default one (named
    /// `name`) if it doesn't. Returns the collection id for convenience.
    pub fn ensure_collection(&mut self, id: &str, name: &str, created: &str) -> String {
        if !self.collections.iter().any(|c| c.id == id) {
            self.collections.push(Collection {
                id: id.to_string(),
                name: name.to_string(),
                created: created.to_string(),
                ..Default::default()
            });
            self.dirty.insert(id.to_string());
        }
        id.to_string()
    }

    /// Create or update a collection's curatorial metadata by name (its id is
    /// the slug of the name). Only fields set in `fields` change; `created` is
    /// set on first creation. Merge policy is "fill gaps, curator wins": the
    /// caller decides what to pass (the CLI passes what the curator typed; an
    /// importer passes only fields that are still empty). Returns the id.
    pub fn apply_fields(&mut self, name: &str, fields: &CollectionFields, created: &str) -> String {
        let id = slugify(name);
        self.dirty.insert(id.clone());
        if let Some(c) = self.collections.iter_mut().find(|c| c.id == id) {
            c.name = name.to_string();
            fields.apply_to(c);
        } else {
            let mut c = Collection {
                id: id.clone(),
                name: name.to_string(),
                created: created.to_string(),
                ..Default::default()
            };
            fields.apply_to(&mut c);
            self.collections.push(c);
        }
        id
    }

    /// Persist the manifest: the derived `waczs.json`, plus a Markdown finding
    /// aid for every collection created/modified this session. Untouched finding
    /// aids are left on disk as-is (so hand edits keep their formatting).
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.index_dir)?;
        std::fs::write(
            self.index_dir.join("waczs.json"),
            serde_json::to_string_pretty(&self.waczs)?,
        )?;
        for c in &self.collections {
            if self.dirty.contains(&c.id) {
                write_finding_aid(&self.home, c)?;
            }
        }
        Ok(())
    }

    pub fn wacz_by_id(&self, id: &str) -> Option<&Wacz> {
        self.waczs.iter().find(|w| w.id == id)
    }

    pub fn collection_by_id(&self, id: &str) -> Option<&Collection> {
        self.collections.iter().find(|c| c.id == id)
    }

    /// The WACZ members of a collection.
    pub fn members_of<'a>(&'a self, collection_id: &'a str) -> impl Iterator<Item = &'a Wacz> {
        self.waczs
            .iter()
            .filter(move |w| w.collection == collection_id)
    }
}

/// Read and parse a JSON file if it exists (`None` when absent).
fn read_json<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&data)?))
}

// ── Finding-aid files (git-committable curator source of truth) ────────────────

/// A partial update to a collection's curatorial metadata. Every field is
/// optional so callers change only what they mean to; `subjects` is
/// `Option<Vec>` so "not provided" (`None`) differs from "clear to empty"
/// (`Some(vec![])`). Shared by the `collection set` CLI and the importer, with a
/// "fill gaps, curator wins" policy applied by the *caller* (see
/// [`Manifest::apply_fields`]).
#[derive(Debug, Clone, Default)]
pub struct CollectionFields {
    pub description: Option<String>,
    pub curator: Option<String>,
    pub creator: Option<String>,
    pub dates: Option<String>,
    pub rights: Option<String>,
    pub subjects: Option<Vec<String>>,
    pub narrative: Option<String>,
}

impl CollectionFields {
    /// Whether nothing is set (nothing to apply).
    pub fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.curator.is_none()
            && self.creator.is_none()
            && self.dates.is_none()
            && self.rights.is_none()
            && self.subjects.is_none()
            && self.narrative.is_none()
    }

    /// Overwrite `c`'s fields for each value that is `Some` (leave the rest).
    fn apply_to(&self, c: &mut Collection) {
        if self.description.is_some() {
            c.description = self.description.clone();
        }
        if self.curator.is_some() {
            c.curator = self.curator.clone();
        }
        if self.creator.is_some() {
            c.creator = self.creator.clone();
        }
        if self.dates.is_some() {
            c.dates = self.dates.clone();
        }
        if self.rights.is_some() {
            c.rights = self.rights.clone();
        }
        if let Some(s) = &self.subjects {
            c.subjects = s.clone();
        }
        if self.narrative.is_some() {
            c.narrative = self.narrative.clone();
        }
    }
}

/// The YAML front-matter of a `collections/<id>.md` finding aid. The `id` is the
/// filename stem (not stored here); the `narrative` is the Markdown body.
#[derive(Debug, Serialize, Deserialize, Default)]
struct FrontMatter {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    curator: Option<String>,
    created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    creator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dates: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rights: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    subjects: Vec<String>,
}

/// Whether `dir` holds at least one `*.md` finding aid.
fn dir_has_findingaid(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "md"))
        })
        .unwrap_or(false)
}

/// Whether a legacy `collections.json` array's first element looks like a WACZ
/// record (has `source`/`path`) rather than a collection group.
fn legacy_json_holds_waczs(path: &Path) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    Ok(value
        .as_array()
        .and_then(|a| a.first())
        .map(|e| e.get("source").is_some() || e.get("path").is_some())
        .unwrap_or(false))
}

/// Load every `collections/*.md` finding aid (the descriptive source of truth),
/// sorted by id for a stable order.
pub fn load_finding_aids(dir: &Path) -> Result<Vec<Collection>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading finding aid {}", path.display()))?;
        out.push(
            parse_finding_aid(&id, &text)
                .with_context(|| format!("parsing finding aid {}", path.display()))?,
        );
    }
    Ok(out)
}

/// Parse a finding aid's text (YAML front-matter + Markdown body) into a
/// [`Collection`] with the given `id` (the filename stem).
fn parse_finding_aid(id: &str, text: &str) -> Result<Collection> {
    let (fm_src, body) = split_front_matter(text);
    let fm: FrontMatter = if fm_src.trim().is_empty() {
        FrontMatter::default()
    } else {
        serde_yaml_ng::from_str(fm_src).context("parsing YAML front-matter")?
    };
    let narrative = {
        let b = body.trim();
        (!b.is_empty()).then(|| b.to_string())
    };
    Ok(Collection {
        id: id.to_string(),
        name: if fm.name.is_empty() {
            id.to_string()
        } else {
            fm.name
        },
        description: fm.description,
        created: fm.created,
        curator: fm.curator,
        creator: fm.creator,
        dates: fm.dates,
        rights: fm.rights,
        subjects: fm.subjects,
        narrative,
    })
}

/// Split leading `---`-delimited YAML front-matter from the Markdown body,
/// returning `(front_matter_yaml, body)`. Front matter is `""` when absent (or
/// when the opening fence has no matching close).
fn split_front_matter(text: &str) -> (&str, &str) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text); // tolerate a BOM
    let after_open = match t
        .strip_prefix("---\n")
        .or_else(|| t.strip_prefix("---\r\n"))
    {
        Some(r) => r,
        None => return ("", t),
    };
    let mut idx = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return (&after_open[..idx], &after_open[idx + line.len()..]);
        }
        idx += line.len();
    }
    ("", t) // unterminated front matter → treat the whole file as body
}

/// Write a collection's finding aid to `<home>/collections/<id>.md`: YAML
/// front-matter for the structured fields, then the Markdown `narrative` body.
pub fn write_finding_aid(home: &Path, c: &Collection) -> Result<()> {
    let dir = home.join("collections");
    std::fs::create_dir_all(&dir)?;
    let fm = FrontMatter {
        name: c.name.clone(),
        description: c.description.clone(),
        curator: c.curator.clone(),
        created: c.created.clone(),
        creator: c.creator.clone(),
        dates: c.dates.clone(),
        rights: c.rights.clone(),
        subjects: c.subjects.clone(),
    };
    let yaml = serde_yaml_ng::to_string(&fm).context("serializing YAML front-matter")?;
    let mut out = String::from("---\n");
    out.push_str(&yaml);
    if !yaml.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("---\n");
    if let Some(body) = c
        .narrative
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        out.push('\n');
        out.push_str(body);
        out.push('\n');
    }
    let path = dir.join(format!("{}.md", c.id));
    std::fs::write(&path, out)
        .with_context(|| format!("writing finding aid {}", path.display()))?;
    Ok(())
}

/// Path to a crawl's committable Markdown note, `<home>/crawls/<id>.md`.
pub fn crawl_note_path(home: &Path, id: &str) -> PathBuf {
    home.join("crawls").join(format!("{id}.md"))
}

/// Read a crawl's Markdown note, if present and non-empty.
pub fn read_crawl_note(home: &Path, id: &str) -> Option<String> {
    let text = std::fs::read_to_string(crawl_note_path(home, id)).ok()?;
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Write a crawl's Markdown note to `<home>/crawls/<id>.md`.
pub fn write_crawl_note(home: &Path, id: &str, note: &str) -> Result<()> {
    let path = crawl_note_path(home, id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{}\n", note.trim()))
        .with_context(|| format!("writing crawl note {}", path.display()))?;
    Ok(())
}

/// Deserialize `software` as either a single string (older manifests wrote one)
/// or a list of strings, always yielding a `Vec<String>`.
fn string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// A URL/id-friendly slug for a collection name: lowercase ASCII alphanumerics,
/// with runs of anything else collapsed to a single hyphen and trimmed
/// (`"Bay Area Transit"` -> `bay-area-transit`). Falls back to a short hash when
/// the name has no sluggable characters.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash {
                slug.push('-');
                pending_dash = false;
            }
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        bytes_to_hex(&sha256_of_bytes(name.as_bytes())[..4])
    } else {
        slug
    }
}

/// Stable short ID for a WACZ: first 8 hex chars of SHA-256 of the source
/// location string (an absolute file path or a URL).
pub fn wacz_id(source: &Source) -> String {
    let hash = sha256_of_bytes(source.location().as_bytes());
    bytes_to_hex(&hash[..4])
}

/// Compute SHA-256 of a file's contents, reading in 64 KiB chunks.
pub fn file_sha256(path: &Path) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(bytes_to_hex(hasher.finalize().as_slice()))
}

fn sha256_of_bytes(data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn wacz_id_is_stable() {
        let s = Source::File(PathBuf::from("/data/archive.wacz"));
        let id1 = wacz_id(&s);
        let id2 = wacz_id(&s);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 8);
    }

    #[test]
    fn different_sources_different_ids() {
        let id1 = wacz_id(&Source::File(PathBuf::from("/data/a.wacz")));
        let id2 = wacz_id(&Source::File(PathBuf::from("/data/b.wacz")));
        let id3 = wacz_id(&Source::Url("https://ex.org/a.wacz".to_string()));
        assert_ne!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn source_parse_distinguishes_url_from_path() {
        assert!(Source::parse("https://ex.org/a.wacz").is_url());
        assert!(Source::parse("http://ex.org/a.wacz").is_url());
        assert!(!Source::parse("/data/a.wacz").is_url());
        assert!(!Source::parse("relative/a.wacz").is_url());
    }

    #[test]
    fn source_serializes_as_plain_string() {
        let file = Source::File(PathBuf::from("/data/a.wacz"));
        assert_eq!(serde_json::to_string(&file).unwrap(), "\"/data/a.wacz\"");
        let url = Source::Url("https://ex.org/a.wacz".to_string());
        assert_eq!(
            serde_json::to_string(&url).unwrap(),
            "\"https://ex.org/a.wacz\""
        );
        // Round-trips back to the right variant.
        let back: Source = serde_json::from_str("\"https://ex.org/a.wacz\"").unwrap();
        assert_eq!(back, url);
    }

    #[test]
    fn browsertrix_source_roundtrips() {
        let s = Source::Browsertrix {
            host: "https://app.browsertrix.com".into(),
            org: "o1".into(),
            item: "item-1".into(),
            resource: "crawl-20250101-abc-0.wacz".into(),
        };
        let encoded = s.location();
        assert_eq!(
            encoded,
            "browsertrix|https://app.browsertrix.com|o1|item-1|crawl-20250101-abc-0.wacz"
        );
        assert_eq!(Source::parse(&encoded), s);
        assert!(s.is_remote());
        assert!(s.as_file().is_none());
        assert!(!s.is_url());
        // A stable id (independent of the presigned URL, which changes each time).
        assert_eq!(wacz_id(&s), wacz_id(&Source::parse(&encoded)));
    }

    #[test]
    fn software_accepts_string_or_list() {
        // Older manifests wrote `software` as a single string; newer ones a list.
        let legacy: Wacz = serde_json::from_str(
            r#"{"id":"a","source":"archive/x.wacz","name":"x","date_indexed":"t","file_size":1,"sha256":"h","software":"py-wacz 0.4.6"}"#,
        ).unwrap();
        assert_eq!(legacy.software, vec!["py-wacz 0.4.6".to_string()]);

        let listy: Wacz = serde_json::from_str(
            r#"{"id":"a","source":"archive/x.wacz","name":"x","date_indexed":"t","file_size":1,"sha256":"h","software":["Heritrix/3.4.0","py-wacz 0.4.6"]}"#,
        ).unwrap();
        assert_eq!(
            listy.software,
            vec!["Heritrix/3.4.0".to_string(), "py-wacz 0.4.6".to_string()]
        );

        // Absent -> empty, and empty is not serialized back out.
        let none: Wacz = serde_json::from_str(
            r#"{"id":"a","source":"archive/x.wacz","name":"x","date_indexed":"t","file_size":1,"sha256":"h"}"#,
        ).unwrap();
        assert!(none.software.is_empty());
        assert!(!serde_json::to_string(&none).unwrap().contains("software"));
    }

    #[test]
    fn manifest_reads_legacy_path_key() {
        // Older manifests used "path" instead of "source".
        let tmp = TempDir::new().unwrap();
        let legacy = r#"[{"id":"abc12345","path":"/data/old.wacz","name":"old","date_indexed":"2026-07-01T00:00:00Z","file_size":10,"sha256":"deadbeef"}]"#;
        std::fs::write(tmp.path().join("collections.json"), legacy).unwrap();
        let m = Manifest::open(tmp.path()).unwrap();
        assert_eq!(m.waczs.len(), 1);
        assert_eq!(
            m.waczs[0].source,
            Source::File(PathBuf::from("/data/old.wacz"))
        );
        // Migration synthesizes a singleton collection per legacy WACZ.
        assert_eq!(m.collections.len(), 1);
    }

    #[test]
    fn file_sha256_detects_content_change() {
        // The fixity primitive behind `rustyweb verify`: the same bytes hash to
        // the same digest, and a single changed byte changes the digest.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.bin");
        std::fs::write(&path, b"hello world").unwrap();

        let h1 = file_sha256(&path).unwrap();
        let h2 = file_sha256(&path).unwrap();
        assert_eq!(h1, h2, "unchanged file should hash identically");
        assert_eq!(h1.len(), 64, "sha-256 hex is 64 chars");

        std::fs::write(&path, b"hello worlx").unwrap();
        let h3 = file_sha256(&path).unwrap();
        assert_ne!(h1, h3, "a changed byte must change the digest");
    }

    /// A WACZ member with the given id/name and defaults elsewhere.
    fn wacz(id: &str, name: &str, description: Option<&str>) -> Wacz {
        Wacz {
            id: id.to_string(),
            collection: id.to_string(),
            source: Source::File(PathBuf::from("/data/test.wacz")),
            name: name.to_string(),
            date_indexed: "2026-07-01T00:00:00Z".to_string(),
            file_size: 1024,
            sha256: "deadbeef".to_string(),
            description: description.map(str::to_string),
            crawl_date: None,
            seed_pages: vec![],
            software: Vec::new(),
            operator: None,
            user_agent: None,
            robots: None,
            page_count: None,
            capture_start: None,
            capture_end: None,
            browsertrix: None,
            nested_waczs: None,
            modified: None,
            is_part_of: None,
            hostname: None,
            conforms_to: None,
            keywords: Vec::new(),
            licenses: Vec::new(),
            status_counts: BTreeMap::new(),
        }
    }

    #[test]
    fn wacz_without_browsertrix_field_deserializes_to_none() {
        // Backward compatibility: an older collections.json entry has no
        // `browsertrix` key; it must load with the field defaulting to None.
        let json = r#"{
            "id": "abc12345",
            "source": "/data/test.wacz",
            "name": "test",
            "date_indexed": "2026-07-01T00:00:00Z",
            "file_size": 1024,
            "sha256": "deadbeef"
        }"#;
        let w: Wacz = serde_json::from_str(json).unwrap();
        assert!(w.browsertrix.is_none());
    }

    #[test]
    fn browsertrix_ref_roundtrips() {
        let w = {
            let mut w = wacz("abc12345", "test", None);
            w.browsertrix = Some(BrowsertrixRef {
                host: "https://app.browsertrix.com".to_string(),
                item_id: "item-1".to_string(),
                resource_hash: "sha256:aa".to_string(),
                review_status: Some(4),
            });
            w
        };
        let json = serde_json::to_string(&w).unwrap();
        let back: Wacz = serde_json::from_str(&json).unwrap();
        assert_eq!(w.browsertrix, back.browsertrix);
    }

    #[test]
    fn manifest_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut m = Manifest::open(tmp.path()).unwrap();
        assert!(m.waczs.is_empty());

        m.upsert_wacz(wacz("abc12345", "test", Some("A test collection")));
        m.save().unwrap();

        let m2 = Manifest::open(tmp.path()).unwrap();
        assert_eq!(m2.waczs.len(), 1);
        assert_eq!(m2.waczs[0].id, "abc12345");
        assert_eq!(
            m2.waczs[0].description.as_deref(),
            Some("A test collection")
        );
    }

    #[test]
    fn manifest_upsert_updates_existing() {
        let tmp = TempDir::new().unwrap();
        let mut m = Manifest::open(tmp.path()).unwrap();

        m.upsert_wacz(wacz("abc12345", "test", None));
        let mut updated = wacz("abc12345", "test-updated", Some("updated"));
        updated.sha256 = "cafebabe".to_string();
        m.upsert_wacz(updated);

        assert_eq!(m.waczs.len(), 1);
        assert_eq!(m.waczs[0].name, "test-updated");
    }

    #[test]
    fn slugify_makes_readable_ids() {
        assert_eq!(slugify("Bay Area Transit"), "bay-area-transit");
        assert_eq!(slugify("  Hello, World!  "), "hello-world");
        assert_eq!(slugify("already-slug"), "already-slug");
        // No sluggable characters -> short hash fallback (8 hex chars).
        assert_eq!(slugify("!!!").len(), 8);
    }

    #[test]
    fn apply_fields_creates_then_updates_preserving_created() {
        let tmp = TempDir::new().unwrap();
        let mut m = Manifest::open(&tmp.path().join("index")).unwrap();

        let id = m.apply_fields(
            "Bay Area Transit",
            &CollectionFields {
                description: Some("desc".into()),
                ..Default::default()
            },
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(id, "bay-area-transit");
        assert_eq!(m.collections.len(), 1);
        assert_eq!(m.collections[0].description.as_deref(), Some("desc"));

        // Re-applying updates only the Some fields, keeps `created`, and — "fill
        // gaps, curator wins" — a None leaves the existing value untouched.
        m.apply_fields(
            "Bay Area Transit",
            &CollectionFields {
                description: None, // not provided → keep "desc"
                curator: Some("Ed".into()),
                creator: Some("BART".into()),
                subjects: Some(vec!["transit".into(), "bay-area".into()]),
                ..Default::default()
            },
            "2026-02-02T00:00:00Z",
        );
        assert_eq!(m.collections.len(), 1);
        assert_eq!(m.collections[0].description.as_deref(), Some("desc"));
        assert_eq!(m.collections[0].curator.as_deref(), Some("Ed"));
        assert_eq!(m.collections[0].creator.as_deref(), Some("BART"));
        assert_eq!(m.collections[0].subjects, vec!["transit", "bay-area"]);
        assert_eq!(m.collections[0].created, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn finding_aid_roundtrips_through_files() {
        let tmp = TempDir::new().unwrap();
        let index_dir = tmp.path().join("index");

        let mut m = Manifest::open(&index_dir).unwrap();
        m.apply_fields(
            "SUCHO",
            &CollectionFields {
                creator: Some("Saving Ukrainian Cultural Heritage Online".into()),
                dates: Some("2022–2023".into()),
                rights: Some("See individual sites; archived for research".into()),
                subjects: Some(vec!["ukraine".into(), "cultural heritage".into()]),
                narrative: Some("## Scope and Content\n\nWhy this was archived.".into()),
                ..Default::default()
            },
            "2026-01-01T00:00:00Z",
        );
        m.save().unwrap();

        // A finding-aid Markdown file is written under <home>/collections/.
        let md = tmp.path().join("collections").join("sucho.md");
        assert!(md.exists(), "finding aid should be written to {md:?}");
        let text = std::fs::read_to_string(&md).unwrap();
        assert!(text.starts_with("---\n"), "has YAML front-matter");
        assert!(text.contains("creator: Saving Ukrainian"));
        assert!(text.contains("## Scope and Content"));

        // Re-opening reads the file back as the source of truth.
        let m2 = Manifest::open(&index_dir).unwrap();
        let c = m2.collection_by_id("sucho").unwrap();
        assert_eq!(
            c.creator.as_deref(),
            Some("Saving Ukrainian Cultural Heritage Online")
        );
        assert_eq!(c.subjects, vec!["ukraine", "cultural heritage"]);
        assert_eq!(
            c.narrative.as_deref(),
            Some("## Scope and Content\n\nWhy this was archived.")
        );
        assert_eq!(c.created, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn hand_edited_finding_aid_loads() {
        // A curator can author the Markdown by hand; rustyweb reads it verbatim.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("collections");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("my-coll.md"),
            "---\nname: My Collection\ncreated: 2026-03-01T00:00:00Z\nsubjects:\n  - a\n  - b\n---\n\n# About\n\nHand-written prose.\n",
        )
        .unwrap();

        let m = Manifest::open(&tmp.path().join("index")).unwrap();
        let c = m.collection_by_id("my-coll").unwrap();
        assert_eq!(c.name, "My Collection");
        assert_eq!(c.subjects, vec!["a", "b"]);
        assert_eq!(
            c.narrative.as_deref(),
            Some("# About\n\nHand-written prose.")
        );
    }

    #[test]
    fn legacy_collections_json_migrates_to_markdown() {
        // A pre-existing index/collections.json (groups, no collections/ dir)
        // loads and is migrated to a Markdown finding aid on save.
        let tmp = TempDir::new().unwrap();
        let index_dir = tmp.path().join("index");
        std::fs::create_dir_all(&index_dir).unwrap();
        std::fs::write(
            index_dir.join("collections.json"),
            r#"[{"id":"old","name":"Old Coll","description":"legacy","created":"2025-01-01T00:00:00Z"}]"#,
        )
        .unwrap();
        // waczs.json present (so this isn't the waczs-in-collections layout).
        std::fs::write(index_dir.join("waczs.json"), "[]").unwrap();

        let m = Manifest::open(&index_dir).unwrap();
        assert_eq!(
            m.collection_by_id("old").unwrap().description.as_deref(),
            Some("legacy")
        );
        m.save().unwrap();
        assert!(tmp.path().join("collections").join("old.md").exists());
    }

    #[test]
    fn crawl_note_roundtrips() {
        let tmp = TempDir::new().unwrap();
        assert!(read_crawl_note(tmp.path(), "abc12345").is_none());
        write_crawl_note(tmp.path(), "abc12345", "  A note about absences.  ").unwrap();
        assert_eq!(
            read_crawl_note(tmp.path(), "abc12345").as_deref(),
            Some("A note about absences.")
        );
    }
}
