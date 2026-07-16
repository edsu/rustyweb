use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeedPage {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub ts: String,
}

/// Where a collection's WACZ lives: a local file or a remote http(s) URL.
/// Serializes as a plain string (the path or the URL) for a readable manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Source {
    File(PathBuf),
    Url(String),
}

impl Source {
    /// Parse a location string: `http://`/`https://` is a URL, else a file path.
    pub fn parse(s: &str) -> Self {
        if s.starts_with("http://") || s.starts_with("https://") {
            Source::Url(s.to_string())
        } else {
            Source::File(PathBuf::from(s))
        }
    }

    pub fn is_url(&self) -> bool {
        matches!(self, Source::Url(_))
    }

    /// The local file path, if this is a file source.
    pub fn as_file(&self) -> Option<&Path> {
        match self {
            Source::File(p) => Some(p.as_path()),
            Source::Url(_) => None,
        }
    }

    /// Stable string form: the file path or the URL.
    pub fn location(&self) -> String {
        match self {
            Source::File(p) => p.to_string_lossy().into_owned(),
            Source::Url(u) => u.clone(),
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
            Source::Url(_) => None,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "string_or_seq")]
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

/// A curated collection: a named group of [`Wacz`] members with its own
/// (curatorial) metadata. Aggregates (member count, size, capture range,
/// software) are derived from members at read time, not stored here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// When the collection was first created (RFC 3339).
    pub created: String,
    /// Optional curator / owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curator: Option<String>,
}

/// The on-disk manifest: curated collections (`collections.json`) plus their
/// WACZ members (`waczs.json`), both under `<home>/index`.
pub struct Manifest {
    index_dir: PathBuf,
    pub collections: Vec<Collection>,
    pub waczs: Vec<Wacz>,
}

impl Manifest {
    pub fn open(index_dir: &Path) -> Result<Self> {
        let collections_path = index_dir.join("collections.json");
        let waczs_path = index_dir.join("waczs.json");

        // New two-file layout.
        if waczs_path.exists() {
            return Ok(Self {
                index_dir: index_dir.to_path_buf(),
                collections: read_json(&collections_path)?.unwrap_or_default(),
                waczs: read_json(&waczs_path)?.unwrap_or_default(),
            });
        }

        // Migrate an older single-file `collections.json` that held WACZ
        // records: each becomes a member of its own singleton collection.
        if collections_path.exists() {
            let data = std::fs::read_to_string(&collections_path)?;
            let value: serde_json::Value = serde_json::from_str(&data)?;
            let looks_like_waczs = value
                .as_array()
                .and_then(|a| a.first())
                .map(|e| e.get("source").is_some() || e.get("path").is_some())
                .unwrap_or(false);
            if looks_like_waczs {
                let mut waczs: Vec<Wacz> = serde_json::from_value(value)?;
                let mut collections = Vec::new();
                for w in &mut waczs {
                    if w.collection.is_empty() {
                        w.collection = w.id.clone();
                    }
                    collections.push(Collection {
                        id: w.id.clone(),
                        name: w.name.clone(),
                        description: w.description.clone(),
                        created: w.date_indexed.clone(),
                        curator: None,
                    });
                }
                return Ok(Self { index_dir: index_dir.to_path_buf(), collections, waczs });
            }
            // Otherwise collections.json already holds groups (waczs.json just missing).
            return Ok(Self {
                index_dir: index_dir.to_path_buf(),
                collections: serde_json::from_value(value).unwrap_or_default(),
                waczs: Vec::new(),
            });
        }

        Ok(Self {
            index_dir: index_dir.to_path_buf(),
            collections: Vec::new(),
            waczs: Vec::new(),
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

    /// Insert or replace a collection by id.
    pub fn upsert_collection(&mut self, collection: Collection) {
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
                description: None,
                created: created.to_string(),
                curator: None,
            });
        }
        id.to_string()
    }

    /// Create or update a collection's metadata by name (its id is the slug of
    /// the name). Only provided fields are changed; `created` is set on first
    /// creation. Returns the collection id.
    pub fn set_collection(
        &mut self,
        name: &str,
        description: Option<String>,
        curator: Option<String>,
        created: &str,
    ) -> String {
        let id = slugify(name);
        if let Some(c) = self.collections.iter_mut().find(|c| c.id == id) {
            c.name = name.to_string();
            if description.is_some() {
                c.description = description;
            }
            if curator.is_some() {
                c.curator = curator;
            }
        } else {
            self.collections.push(Collection {
                id: id.clone(),
                name: name.to_string(),
                description,
                created: created.to_string(),
                curator,
            });
        }
        id
    }

    pub fn save(&self) -> Result<()> {
        std::fs::write(
            self.index_dir.join("collections.json"),
            serde_json::to_string_pretty(&self.collections)?,
        )?;
        std::fs::write(
            self.index_dir.join("waczs.json"),
            serde_json::to_string_pretty(&self.waczs)?,
        )?;
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
        self.waczs.iter().filter(move |w| w.collection == collection_id)
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
        assert_eq!(serde_json::to_string(&url).unwrap(), "\"https://ex.org/a.wacz\"");
        // Round-trips back to the right variant.
        let back: Source = serde_json::from_str("\"https://ex.org/a.wacz\"").unwrap();
        assert_eq!(back, url);
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
        assert_eq!(listy.software, vec!["Heritrix/3.4.0".to_string(), "py-wacz 0.4.6".to_string()]);

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
        assert_eq!(m.waczs[0].source, Source::File(PathBuf::from("/data/old.wacz")));
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
        }
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
        assert_eq!(m2.waczs[0].description.as_deref(), Some("A test collection"));
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
    fn set_collection_creates_then_updates_preserving_created() {
        let tmp = TempDir::new().unwrap();
        let mut m = Manifest::open(tmp.path()).unwrap();

        let id = m.set_collection("Bay Area Transit", Some("desc".into()), None, "2026-01-01T00:00:00Z");
        assert_eq!(id, "bay-area-transit");
        assert_eq!(m.collections.len(), 1);
        assert_eq!(m.collections[0].description.as_deref(), Some("desc"));

        // Re-setting updates fields but keeps the original created timestamp.
        m.set_collection("Bay Area Transit", Some("new".into()), Some("Ed".into()), "2026-02-02T00:00:00Z");
        assert_eq!(m.collections.len(), 1);
        assert_eq!(m.collections[0].description.as_deref(), Some("new"));
        assert_eq!(m.collections[0].curator.as_deref(), Some("Ed"));
        assert_eq!(m.collections[0].created, "2026-01-01T00:00:00Z");
    }
}
