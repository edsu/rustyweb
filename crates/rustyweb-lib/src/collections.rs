use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    Wacz,
    Warc,
}

impl CollectionKind {
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "wacz" => CollectionKind::Wacz,
            _ => CollectionKind::Warc,
        }
    }

    pub fn content_type(&self) -> &'static str {
        match self {
            CollectionKind::Wacz => "application/zip",
            CollectionKind::Warc => "application/warc",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
    pub path: PathBuf,
    pub name: String,
    pub kind: CollectionKind,
    pub date_indexed: String,
    pub record_count: u64,
    pub file_size: u64,
    pub sha256: String,
}

impl Collection {
    /// Whether the file still exists at its registered path.
    pub fn is_present(&self) -> bool {
        self.path.exists()
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

    pub fn find_by_path(&self, path: &Path) -> Option<&Collection> {
        self.collections.iter().find(|c| c.path == path)
    }
}

/// Stable short ID for a collection: first 8 hex chars of SHA-256 of the absolute path string.
pub fn collection_id(path: &Path) -> String {
    let hash = sha256_of_bytes(path.to_string_lossy().as_bytes());
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
        let p = Path::new("/data/archive.wacz");
        let id1 = collection_id(p);
        let id2 = collection_id(p);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 8);
    }

    #[test]
    fn different_paths_different_ids() {
        let id1 = collection_id(Path::new("/data/a.wacz"));
        let id2 = collection_id(Path::new("/data/b.wacz"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn manifest_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut m = CollectionManifest::open(tmp.path()).unwrap();
        assert!(m.collections.is_empty());

        let col = Collection {
            id: "abc12345".to_string(),
            path: PathBuf::from("/data/test.wacz"),
            name: "test".to_string(),
            kind: CollectionKind::Wacz,
            date_indexed: "2026-07-01T00:00:00Z".to_string(),
            record_count: 10,
            file_size: 1024,
            sha256: "deadbeef".to_string(),
        };
        m.upsert(col.clone());
        m.save().unwrap();

        let m2 = CollectionManifest::open(tmp.path()).unwrap();
        assert_eq!(m2.collections.len(), 1);
        assert_eq!(m2.collections[0].id, "abc12345");
        assert_eq!(m2.collections[0].record_count, 10);
    }

    #[test]
    fn manifest_upsert_updates_existing() {
        let tmp = TempDir::new().unwrap();
        let mut m = CollectionManifest::open(tmp.path()).unwrap();

        let col = Collection {
            id: "abc12345".to_string(),
            path: PathBuf::from("/data/test.wacz"),
            name: "test".to_string(),
            kind: CollectionKind::Wacz,
            date_indexed: "2026-07-01T00:00:00Z".to_string(),
            record_count: 10,
            file_size: 1024,
            sha256: "deadbeef".to_string(),
        };
        m.upsert(col);
        m.upsert(Collection {
            id: "abc12345".to_string(),
            path: PathBuf::from("/data/test.wacz"),
            name: "test".to_string(),
            kind: CollectionKind::Wacz,
            date_indexed: "2026-07-02T00:00:00Z".to_string(),
            record_count: 20,
            file_size: 1024,
            sha256: "cafebabe".to_string(),
        });
        assert_eq!(m.collections.len(), 1);
        assert_eq!(m.collections[0].record_count, 20);
    }
}
