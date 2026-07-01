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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
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
}

impl Collection {
    /// Whether the WACZ is available. Local files must exist on disk; remote
    /// URLs are assumed present (we do not probe the network here).
    pub fn is_present(&self) -> bool {
        match &self.source {
            Source::File(p) => p.exists(),
            Source::Url(_) => true,
        }
    }
}

pub struct CollectionManifest {
    manifest_path: PathBuf,
    pub collections: Vec<Collection>,
}

impl CollectionManifest {
    pub fn open(index_dir: &Path) -> Result<Self> {
        let manifest_path = index_dir.join("collections.json");
        let collections = if manifest_path.exists() {
            let data = std::fs::read_to_string(&manifest_path)?;
            serde_json::from_str(&data)?
        } else {
            Vec::new()
        };
        Ok(Self {
            manifest_path,
            collections,
        })
    }

    pub fn upsert(&mut self, collection: Collection) {
        if let Some(pos) = self.collections.iter().position(|c| c.id == collection.id) {
            self.collections[pos] = collection;
        } else {
            self.collections.push(collection);
        }
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.collections)?;
        std::fs::write(&self.manifest_path, json)?;
        Ok(())
    }

    pub fn find_by_id(&self, id: &str) -> Option<&Collection> {
        self.collections.iter().find(|c| c.id == id)
    }
}

/// Stable short ID for a collection: first 8 hex chars of SHA-256 of the source
/// location string (an absolute file path or a URL).
pub fn collection_id(source: &Source) -> String {
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
    fn collection_id_is_stable() {
        let s = Source::File(PathBuf::from("/data/archive.wacz"));
        let id1 = collection_id(&s);
        let id2 = collection_id(&s);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 8);
    }

    #[test]
    fn different_sources_different_ids() {
        let id1 = collection_id(&Source::File(PathBuf::from("/data/a.wacz")));
        let id2 = collection_id(&Source::File(PathBuf::from("/data/b.wacz")));
        let id3 = collection_id(&Source::Url("https://ex.org/a.wacz".to_string()));
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
    fn manifest_reads_legacy_path_key() {
        // Older manifests used "path" instead of "source".
        let tmp = TempDir::new().unwrap();
        let legacy = r#"[{"id":"abc12345","path":"/data/old.wacz","name":"old","date_indexed":"2026-07-01T00:00:00Z","file_size":10,"sha256":"deadbeef"}]"#;
        std::fs::write(tmp.path().join("collections.json"), legacy).unwrap();
        let m = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(m.collections.len(), 1);
        assert_eq!(m.collections[0].source, Source::File(PathBuf::from("/data/old.wacz")));
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

    #[test]
    fn manifest_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut m = CollectionManifest::open(tmp.path()).unwrap();
        assert!(m.collections.is_empty());

        let col = Collection {
            id: "abc12345".to_string(),
            source: Source::File(PathBuf::from("/data/test.wacz")),
            name: "test".to_string(),
            date_indexed: "2026-07-01T00:00:00Z".to_string(),
            file_size: 1024,
            sha256: "deadbeef".to_string(),
            description: Some("A test collection".to_string()),
            crawl_date: None,
            seed_pages: vec![],
        };
        m.upsert(col);
        m.save().unwrap();

        let m2 = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(m2.collections.len(), 1);
        assert_eq!(m2.collections[0].id, "abc12345");
        assert_eq!(m2.collections[0].description.as_deref(), Some("A test collection"));
    }

    #[test]
    fn manifest_upsert_updates_existing() {
        let tmp = TempDir::new().unwrap();
        let mut m = CollectionManifest::open(tmp.path()).unwrap();

        let col = Collection {
            id: "abc12345".to_string(),
            source: Source::File(PathBuf::from("/data/test.wacz")),
            name: "test".to_string(),
            date_indexed: "2026-07-01T00:00:00Z".to_string(),
            file_size: 1024,
            sha256: "deadbeef".to_string(),
            description: None,
            crawl_date: None,
            seed_pages: vec![],
        };
        m.upsert(col);
        m.upsert(Collection {
            id: "abc12345".to_string(),
            source: Source::File(PathBuf::from("/data/test.wacz")),
            name: "test-updated".to_string(),
            date_indexed: "2026-07-02T00:00:00Z".to_string(),
            file_size: 2048,
            sha256: "cafebabe".to_string(),
            description: Some("updated".to_string()),
            crawl_date: None,
            seed_pages: vec![],
        });
        assert_eq!(m.collections.len(), 1);
        assert_eq!(m.collections[0].name, "test-updated");
    }
}
