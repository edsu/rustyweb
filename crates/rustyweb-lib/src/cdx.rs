use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

// ── CdxRecord ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct CdxRecord {
    pub original_url: String,
    pub timestamp: String,
    pub mimetype: String,
    pub status: u16,
    pub digest: String,
    pub length: u64,
    pub warc_path: String,
    pub warc_offset: u64,
    pub warc_record_length: u64,
}

// ── MatchType ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum MatchType {
    /// Exact URL match.
    Exact,
    /// All captures whose URL starts with the given prefix (e.g. `example.com/*`).
    Prefix,
    /// All captures under a domain and all its subdomains.
    Domain,
}

// ── CdxStore ─────────────────────────────────────────────────────────────────

pub struct CdxStore {
    _db: fjall::Database,
    partition: fjall::Keyspace,
}

impl CdxStore {
    pub fn open(index_dir: &std::path::Path) -> anyhow::Result<Self> {
        let db = fjall::Database::builder(index_dir).open()?;
        let partition = db.keyspace("cdx", fjall::KeyspaceCreateOptions::default)?;
        Ok(Self { _db: db, partition })
    }

    pub fn insert(&self, record: &CdxRecord) -> anyhow::Result<()> {
        let key = cdx_key(&record.original_url, &record.timestamp);
        let value = bincode::encode_to_vec(record, bincode::config::standard())?;
        self.partition.insert(key, value)?;
        Ok(())
    }

    pub fn query(
        &self,
        url: &str,
        match_type: MatchType,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<CdxRecord>> {
        let mut records = Vec::new();

        // Domain matching needs a range scan: all keys from "com,example)" through
        // "com,example-" (where '-' = 0x2D > ',' = 0x2C) to include both the exact
        // domain and all subdomains without overmatching "com,example2)..." etc.
        let iter: Box<dyn Iterator<Item = fjall::Guard>> = match match_type {
            MatchType::Domain => {
                let start = surt_host_bytes(url);
                // Upper bound: replace the trailing ')' with '-' (0x2D) so the range
                // covers ',' (0x2C, for subdomains) but stops before any non-subdomain.
                let mut end = start.clone();
                if let Some(last) = end.last_mut() {
                    *last = b'-';
                }
                Box::new(self.partition.range(start..end))
            }
            _ => {
                let prefix = scan_prefix(url, match_type);
                Box::new(self.partition.prefix(prefix))
            }
        };

        for guard in iter {
            let (key, value) = guard.into_inner()?;

            if let Some(ts) = timestamp_from_key(key.as_ref()) {
                if from.is_some_and(|f| ts < f) || to.is_some_and(|t| ts > t) {
                    continue;
                }
            }

            let (record, _): (CdxRecord, _) =
                bincode::decode_from_slice(value.as_ref(), bincode::config::standard())?;
            records.push(record);

            if records.len() >= limit {
                break;
            }
        }

        Ok(records)
    }
}

fn scan_prefix(url: &str, match_type: MatchType) -> Vec<u8> {
    match match_type {
        MatchType::Exact => {
            let mut p = to_surt(url).into_bytes();
            p.push(0x00);
            p
        }
        MatchType::Prefix => to_surt(url).into_bytes(),
        MatchType::Domain => unreachable!("domain uses range scan"),
    }
}

/// Returns the reversed host bytes including the closing `)`, e.g. `b"com,example)"`.
/// Used as the start of a domain range scan.
fn surt_host_bytes(url: &str) -> Vec<u8> {
    // Strip scheme + path; we only want the host portion reversed.
    let stripped = url.trim_end_matches('*').trim_end_matches('/');
    let rest = if let Some(pos) = stripped.find("://") {
        &stripped[pos + 3..]
    } else {
        stripped
    };
    let host_part = rest.split('/').next().unwrap_or(rest);
    let host = host_part
        .rfind(':')
        .filter(|&p| host_part[p + 1..].chars().all(|c| c.is_ascii_digit()))
        .map_or(host_part, |p| &host_part[..p]);
    let mut result: Vec<u8> = host.split('.').rev().collect::<Vec<_>>().join(",").into_bytes();
    result.push(b')');
    result
}

fn timestamp_from_key(key: &[u8]) -> Option<&str> {
    let null_pos = key.iter().position(|&b| b == 0x00)?;
    std::str::from_utf8(&key[null_pos + 1..]).ok()
}

// ── SURT + CDX key encoding ───────────────────────────────────────────────────

pub fn to_surt(url: &str) -> String {
    // Strip trailing wildcard (e.g. "example.com/*" → "example.com/")
    let url = url.trim_end_matches('*');

    // Strip scheme
    let rest = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };

    // Split host from path+query at the first '/'
    let (host_part, path_and_query) = if let Some(pos) = rest.find('/') {
        (&rest[..pos], &rest[pos..])
    } else {
        (rest, "")
    };

    // Strip port (e.g. "example.com:8080" → "example.com")
    let host = if let Some(pos) = host_part.rfind(':') {
        let after = &host_part[pos + 1..];
        if after.chars().all(|c| c.is_ascii_digit()) {
            &host_part[..pos]
        } else {
            host_part
        }
    } else {
        host_part
    };

    // Reverse host labels: "www.example.com" → "com,example,www"
    let reversed = host.split('.').rev().collect::<Vec<_>>().join(",");

    format!("{}{}{}", reversed, ")", path_and_query)
}

/// Returns the Fjall key for a CDX record: `<surt_url>\x00<timestamp>`
pub fn cdx_key(url: &str, timestamp: &str) -> Vec<u8> {
    let mut key = to_surt(url).into_bytes();
    key.push(0x00);
    key.extend_from_slice(timestamp.as_bytes());
    key
}

/// Returns the Fjall key for a POST request, encoding method + body into the URL
/// per the IIPC CDX non-GET spec.
pub fn cdx_key_post(url: &str, content_type: &str, body: &[u8], timestamp: &str) -> Vec<u8> {
    let encoded = encode_post_url(url, content_type, body);
    let mut key = to_surt(&encoded).into_bytes();
    key.push(0x00);
    key.extend_from_slice(timestamp.as_bytes());
    key
}

pub fn encode_post_url(url: &str, content_type: &str, body: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

    let ct = content_type.split(';').next().unwrap_or("").trim();

    let extra = if ct == "application/x-www-form-urlencoded" {
        std::str::from_utf8(body).unwrap_or("").to_string()
    } else if ct == "application/json" {
        flatten_json(body)
    } else {
        format!("__wb_post_data={}", BASE64.encode(body))
    };

    let sep = if url.contains('?') { "&" } else { "?" };
    if extra.is_empty() {
        format!("{}{}__wb_method=POST", url, sep)
    } else {
        format!("{}{}__wb_method=POST&{}", url, sep, extra)
    }
}

fn flatten_json(body: &[u8]) -> String {
    let s = std::str::from_utf8(body).unwrap_or("");
    let Ok(val) = serde_json::from_str::<serde_json::Value>(s) else {
        return String::new();
    };
    match val {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{}={}", k, json_scalar(v)))
            .collect::<Vec<_>>()
            .join("&"),
        _ => String::new(),
    }
}

fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() { format!("{}.0", i) } else { n.to_string() }
        }
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

const STRIP_PARAMS: &[&str] = &[
    "fbclid", "gclid", "msclkid", "dclid", "yclid",
    "_", "cb", "_cb", "_ts", "nocache", "bust",
    "_callback", "_jsonp", "callback",
    "sessionid", "session_id", "jsessionid", "phpsessid",
];

/// Strip ephemeral query params and sort the remainder for consistent CDX matching.
pub fn normalize_url_fuzzy(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };

    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.as_ref();
            !k.starts_with("utm_") && !STRIP_PARAMS.contains(&k)
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if pairs.is_empty() {
        parsed.set_query(None);
    } else {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let query = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");
        parsed.set_query(Some(&query));
    }

    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // SURT canonicalization
    #[test]
    fn surt_simple_url() {
        assert_eq!(to_surt("https://example.com/path"), "com,example)/path");
    }

    #[test]
    fn surt_strips_scheme() {
        assert_eq!(to_surt("http://example.com/path"), to_surt("https://example.com/path"));
    }

    #[test]
    fn surt_subdomain() {
        assert_eq!(to_surt("https://www.example.com/"), "com,example,www)/");
    }

    #[test]
    fn surt_preserves_path() {
        assert_eq!(to_surt("https://example.com/path?q=1"), "com,example)/path?q=1");
    }

    #[test]
    fn surt_wildcard_prefix() {
        // "example.com/*" → prefix "com,example)/"
        assert_eq!(to_surt("http://example.com/*"), "com,example)/");
    }

    #[test]
    fn surt_domain_matchtype() {
        // domain match for "example.com" → prefix "com,example,"
        assert_eq!(to_surt("http://example.com"), "com,example)");
    }

    // CDX key encoding
    #[test]
    fn cdx_key_encodes_get() {
        let key = cdx_key("https://example.com/page?q=1", "20240115120000");
        let expected = b"com,example)/page?q=1\x0020240115120000";
        assert_eq!(key, expected);
    }

    #[test]
    fn cdx_key_post_form() {
        let key = cdx_key_post(
            "http://example.com/api",
            "application/x-www-form-urlencoded",
            b"a=1&b=2",
            "20240115120000",
        );
        assert_eq!(key, b"com,example)/api?__wb_method=POST&a=1&b=2\x0020240115120000");
    }

    #[test]
    fn cdx_key_post_json() {
        let key = cdx_key_post(
            "http://example.com/api",
            "application/json",
            br#"{"id":42}"#,
            "20240115120000",
        );
        assert_eq!(key, b"com,example)/api?__wb_method=POST&id=42.0\x0020240115120000");
    }

    #[test]
    fn cdx_key_post_binary() {
        let key = cdx_key_post(
            "http://example.com/api",
            "application/octet-stream",
            b"\x00\x01\x02",
            "20240115120000",
        );
        // base64("\x00\x01\x02") = "AAEC"
        assert_eq!(key, b"com,example)/api?__wb_method=POST&__wb_post_data=AAEC\x0020240115120000");
    }

    // Fuzzy URL normalization
    #[test]
    fn fuzzy_strips_utm() {
        let normalized = normalize_url_fuzzy("https://example.com/?utm_source=x&utm_campaign=y&keep=1");
        assert_eq!(normalized, "https://example.com/?keep=1");
    }

    #[test]
    fn fuzzy_strips_fbclid() {
        let normalized = normalize_url_fuzzy("https://example.com/?fbclid=abc&keep=1");
        assert_eq!(normalized, "https://example.com/?keep=1");
    }

    #[test]
    fn fuzzy_normalizes_params() {
        let normalized = normalize_url_fuzzy("https://example.com/?b=2&a=1");
        assert_eq!(normalized, "https://example.com/?a=1&b=2");
    }

    #[test]
    fn fuzzy_unchanged_url() {
        let url = "https://example.com/?q=hello";
        assert_eq!(normalize_url_fuzzy(url), url);
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(url: &str, ts: &str) -> CdxRecord {
        CdxRecord {
            original_url: url.to_string(),
            timestamp: ts.to_string(),
            mimetype: "text/html".to_string(),
            status: 200,
            digest: "sha1:AAAA".to_string(),
            length: 100,
            warc_path: "test.warc.gz".to_string(),
            warc_offset: 0,
            warc_record_length: 100,
        }
    }

    #[test]
    fn store_insert_and_exact_query() {
        let dir = TempDir::new().unwrap();
        let store = CdxStore::open(dir.path()).unwrap();
        store.insert(&sample("http://example.com/", "20240115120000")).unwrap();

        let results = store.query("http://example.com/", MatchType::Exact, None, None, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].original_url, "http://example.com/");
    }

    #[test]
    fn store_exact_query_no_match() {
        let dir = TempDir::new().unwrap();
        let store = CdxStore::open(dir.path()).unwrap();
        store.insert(&sample("http://example.com/page", "20240115120000")).unwrap();

        let results = store.query("http://example.com/other", MatchType::Exact, None, None, 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn store_prefix_scan() {
        let dir = TempDir::new().unwrap();
        let store = CdxStore::open(dir.path()).unwrap();
        store.insert(&sample("http://example.com/a", "20240115120000")).unwrap();
        store.insert(&sample("http://example.com/b", "20240115120001")).unwrap();
        store.insert(&sample("http://other.com/", "20240115120000")).unwrap();

        let results = store.query("http://example.com/*", MatchType::Prefix, None, None, 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn store_time_range_filter() {
        let dir = TempDir::new().unwrap();
        let store = CdxStore::open(dir.path()).unwrap();
        store.insert(&sample("http://example.com/", "20230101000000")).unwrap();
        store.insert(&sample("http://example.com/", "20240101000000")).unwrap();
        store.insert(&sample("http://example.com/", "20250101000000")).unwrap();

        let results = store.query(
            "http://example.com/",
            MatchType::Exact,
            Some("20240101000000"),
            Some("20240101000000"),
            10,
        ).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].timestamp, "20240101000000");
    }

    #[test]
    fn store_limit() {
        let dir = TempDir::new().unwrap();
        let store = CdxStore::open(dir.path()).unwrap();
        for i in 0..5u64 {
            let mut r = sample("http://example.com/", &format!("2024010100000{}", i));
            r.warc_offset = i;
            store.insert(&r).unwrap();
        }

        let results = store.query("http://example.com/", MatchType::Exact, None, None, 3).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn store_domain_scan() {
        let dir = TempDir::new().unwrap();
        let store = CdxStore::open(dir.path()).unwrap();
        store.insert(&sample("http://example.com/", "20240115120000")).unwrap();
        store.insert(&sample("http://sub.example.com/", "20240115120001")).unwrap();
        store.insert(&sample("http://other.com/", "20240115120000")).unwrap();

        let results = store.query("http://example.com", MatchType::Domain, None, None, 10).unwrap();
        assert_eq!(results.len(), 2);
    }
}
