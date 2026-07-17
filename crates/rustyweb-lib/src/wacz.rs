use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::collections::SeedPage;

/// Metadata extracted from a WACZ file's `datapackage.json` and `pages/pages.jsonl`.
#[derive(Debug, Default)]
pub struct WaczMetadata {
    pub title: Option<String>,
    pub description: Option<String>,
    pub created: Option<String>,
    /// Last-modified time of the WACZ, if recorded (WACZ 1.1.1 `modified`).
    pub modified: Option<String>,
    /// Tool that produced the WACZ (WACZ 1.1.1 `software`).
    pub software: Option<String>,
    /// The collection's main page URL, if declared (WACZ 1.1.1 `mainPageUrl`).
    pub main_page_url: Option<String>,
    pub seed_pages: Vec<SeedPage>,
}

/// A single record from a WACZ CDX index (`indexes/index.cdx[.gz]`).
#[derive(Debug)]
pub struct CdxjRecord {
    pub url: String,
    pub timestamp: String,
    pub mime: String,
    pub status: u16,
    pub filename: String,
    pub offset: u64,
    pub length: u64,
}

/// Read `datapackage.json` and `pages/pages.jsonl` from a WACZ file and return
/// collected metadata.  Missing or unrecognised fields are silently ignored so
/// that the function works with minimal / non-standard WACZ files.
pub fn read_datapackage(wacz_path: &Path) -> Result<WaczMetadata> {
    let file = std::fs::File::open(wacz_path)
        .with_context(|| format!("opening WACZ {}", wacz_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP {}", wacz_path.display()))?;

    // --- datapackage.json -------------------------------------------------
    // Descriptive fields live at the top level in some WACZs and under a
    // nested `metadata` object in others (e.g. Browsertrix). Read both, with
    // the top level taking precedence.
    #[derive(Deserialize, Default)]
    struct Metadata {
        title: Option<String>,
        description: Option<String>,
        created: Option<String>,
        modified: Option<String>,
        software: Option<String>,
        #[serde(rename = "mainPageUrl")]
        main_page_url: Option<String>,
        /// Epoch-millisecond modification time; a fallback for `created`.
        mtime: Option<i64>,
    }
    #[derive(Deserialize, Default)]
    struct DataPackage {
        title: Option<String>,
        description: Option<String>,
        created: Option<String>,
        modified: Option<String>,
        software: Option<String>,
        #[serde(rename = "mainPageUrl")]
        main_page_url: Option<String>,
        #[serde(default)]
        metadata: Option<Metadata>,
    }

    let mut meta = WaczMetadata::default();

    if let Ok(mut entry) = zip.by_name("datapackage.json") {
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;
        if let Ok(dp) = serde_json::from_str::<DataPackage>(&buf) {
            let nested = dp.metadata.unwrap_or_default();
            meta.title = clean(dp.title.or(nested.title));
            meta.description = clean(dp.description.or(nested.description));
            meta.created = dp
                .created
                .or(nested.created)
                .or_else(|| nested.mtime.and_then(millis_to_rfc3339));
            meta.modified = clean(dp.modified.or(nested.modified));
            meta.software = clean(dp.software.or(nested.software));
            meta.main_page_url = clean(dp.main_page_url.or(nested.main_page_url));
        }
    }

    // --- pages/pages.jsonl ------------------------------------------------
    // First line is a header object; subsequent lines are page entries.
    if let Ok(mut entry) = zip.by_name("pages/pages.jsonl") {
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;

        for line in buf.lines().skip(1) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            #[derive(Deserialize)]
            struct PageEntry {
                url: Option<String>,
                title: Option<String>,
                ts: Option<String>,
            }
            if let Ok(p) = serde_json::from_str::<PageEntry>(line) {
                if let Some(url) = p.url {
                    meta.seed_pages.push(SeedPage {
                        url,
                        title: p.title,
                        ts: p.ts.unwrap_or_default(),
                    });
                }
            }
        }
    }

    Ok(meta)
}

/// Trim a metadata string and drop it if empty.
fn clean(s: Option<String>) -> Option<String> {
    s.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// Convert epoch milliseconds to an RFC 3339 timestamp string.
fn millis_to_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339())
}

/// Search a WACZ file's CDX index for records matching `url` (exact URL match).
///
/// Handles both:
/// - CDXJ format: `{surt} {timestamp} {json}`
/// - NDJSON format: `{json}` where the JSON object has a `"url"` field
///
/// The index file may be compressed (`.cdx.gz`) or plain (`.cdx`).
pub fn search_cdx(wacz_path: &Path, url: &str) -> Result<Vec<CdxjRecord>> {
    let file = std::fs::File::open(wacz_path)
        .with_context(|| format!("opening WACZ {}", wacz_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP {}", wacz_path.display()))?;

    // Find the CDX entry (may be .cdx or .cdx.gz, may be in a subdirectory).
    let cdx_name = find_cdx_entry(&mut zip)?;
    let Some(cdx_name) = cdx_name else {
        return Ok(Vec::new());
    };

    let mut entry = zip.by_name(&cdx_name)?;
    let mut raw = Vec::new();
    entry.read_to_end(&mut raw)?;

    // Decompress if gzipped.
    let text = if cdx_name.ends_with(".gz") {
        let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
        let mut out = String::new();
        decoder.read_to_string(&mut out)?;
        out
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };

    let mut results = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rec) = parse_cdx_line(line) {
            if rec.url == url {
                results.push(rec);
            }
        }
    }

    Ok(results)
}

fn find_cdx_entry<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<Option<String>> {
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if name.starts_with("indexes/")
            && (name.ends_with(".cdx.gz") || name.ends_with(".cdx"))
        {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// The basename of a slash-separated path (`archive/x.warc.gz` -> `x.warc.gz`).
fn basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Read and parse **all** CDX records from a WACZ ZIP (any `Read + Seek`), for
/// CDX-guided/streaming indexing. Unlike [`search_cdx`], no URL filter.
pub(crate) fn cdx_records<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<Vec<CdxjRecord>> {
    let Some(cdx_name) = find_cdx_entry(zip)? else {
        return Ok(Vec::new());
    };
    let mut raw = Vec::new();
    zip.by_name(&cdx_name)?.read_to_end(&mut raw)?;
    let text = if cdx_name.ends_with(".gz") {
        let mut out = String::new();
        flate2::read::GzDecoder::new(raw.as_slice()).read_to_string(&mut out)?;
        out
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    Ok(text
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            (!l.is_empty()).then(|| parse_cdx_line(l)).flatten()
        })
        .collect())
}

/// Error unless every embedded WARC is `Stored` (uncompressed). CDX-guided
/// streaming needs this: a CDX byte offset maps to an absolute WACZ position
/// only when the WARC isn't ZIP-compressed. Browsertrix / py-wacz store WARCs
/// uncompressed (they're already gzipped); some tools deflate them, and those
/// can't be streamed (fall back to scan / `--download`).
pub(crate) fn ensure_warcs_stored<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<()> {
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let name = entry.name();
        let is_warc =
            name.starts_with("archive/") && (name.ends_with(".warc.gz") || name.ends_with(".warc"));
        if is_warc && entry.compression() != zip::CompressionMethod::Stored {
            anyhow::bail!(
                "WACZ stores its WARC entries compressed (deflate); CDX-guided \
                 streaming needs uncompressed (stored) WARCs. Index without --stream \
                 (scan mode), or use --download."
            );
        }
    }
    Ok(())
}

/// Map each embedded WARC's basename to the absolute byte offset of its data
/// within the WACZ (`ZipFile::data_start`). Since WARCs are stored uncompressed,
/// a CDX record at `offset` lives at `data_start + offset` in the WACZ.
pub(crate) fn warc_data_starts<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<std::collections::HashMap<String, u64>> {
    let mut map = std::collections::HashMap::new();
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if name.starts_with("archive/") && (name.ends_with(".warc.gz") || name.ends_with(".warc")) {
            map.insert(basename(&name).to_string(), entry.data_start());
        }
    }
    Ok(map)
}

/// Find the crawl's `warcinfo` by decoding the first gzip member of each
/// embedded WARC (warcinfo is the first record) and parsing it — so streaming
/// indexing gets provenance without reading whole WARCs. Returns the first
/// non-empty warcinfo found.
pub(crate) fn find_warcinfo_streaming<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<Option<crate::warc::Warcinfo>> {
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|e| e.name().to_string()))
        .filter(|n| n.starts_with("archive/") && (n.ends_with(".warc.gz") || n.ends_with(".warc")))
        .collect();
    for name in names {
        let mut decoded = Vec::new();
        // read::GzDecoder decodes only the first member, which is enough.
        if flate2::read::GzDecoder::new(zip.by_name(&name)?)
            .read_to_end(&mut decoded)
            .is_err()
            || decoded.is_empty()
        {
            continue;
        }
        for rec in crate::warc::parse_warc_records(&decoded, 0, 0).into_iter().flatten() {
            if let Some(info) = crate::warc::Warcinfo::from_record(&rec) {
                if !info.is_empty() {
                    return Ok(Some(info));
                }
            }
        }
    }
    Ok(None)
}

// WACZ CDX JSON quotes some numeric fields as strings (e.g. `"length":"715"`)
// and leaves others as numbers (`"length":715`), depending on the tool that
// wrote it. Deserialize the varying fields as `Value` and coerce below so a
// single quoted number doesn't cause the whole record to be dropped.
#[derive(Deserialize)]
struct CdxJson {
    url: Option<String>,
    ts: Option<String>,
    mime: Option<String>,
    status: Option<serde_json::Value>,
    filename: Option<String>,
    offset: Option<serde_json::Value>,
    length: Option<serde_json::Value>,
}

fn coerce_u64(v: &Option<serde_json::Value>) -> u64 {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn parse_cdx_line(line: &str) -> Option<CdxjRecord> {
    let (ts, json_str) = if line.starts_with('{') {
        // NDJSON format: the whole line is the JSON object.
        ("", line)
    } else {
        // CDXJ format: "{surt} {timestamp} {json}"
        let mut parts = line.splitn(3, ' ');
        let _surt = parts.next()?;
        let ts = parts.next()?;
        let json = parts.next()?;
        (ts, json)
    };

    let obj: CdxJson = serde_json::from_str(json_str).ok()?;
    let url = obj.url?;
    let timestamp = if !ts.is_empty() {
        ts.to_string()
    } else {
        obj.ts.unwrap_or_default()
    };
    let status = match &obj.status {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0) as u16,
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    };

    Some(CdxjRecord {
        url,
        timestamp,
        mime: obj.mime.unwrap_or_default(),
        status,
        filename: obj.filename.unwrap_or_default(),
        offset: coerce_u64(&obj.offset),
        length: coerce_u64(&obj.length),
    })
}

/// Iterate over the paths of WARC archives embedded inside a WACZ file.
pub fn iter_warc_paths(wacz_path: &Path) -> Result<impl Iterator<Item = Result<String>>> {
    let file = std::fs::File::open(wacz_path)
        .with_context(|| format!("opening WACZ {}", wacz_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP in {}", wacz_path.display()))?;

    let mut names: Vec<String> = Vec::new();
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if name.starts_with("archive/") && (name.ends_with(".warc.gz") || name.ends_with(".warc")) {
            names.push(name);
        }
    }

    Ok(names.into_iter().map(Ok))
}

/// Read the first `warcinfo` record found in a WACZ's WARC(s) and return its
/// parsed provenance ([`crate::warc::Warcinfo`]). `warcinfo` normally leads each
/// WARC, so the first hit describes how the capture was produced. Returns
/// `Ok(None)` when the WACZ has no warcinfo (or none with recognized fields).
pub fn read_warcinfo(wacz_path: &Path) -> Result<Option<crate::warc::Warcinfo>> {
    use crate::warc::{iter_records, Warcinfo};

    let warc_paths: Vec<String> = iter_warc_paths(wacz_path)?.collect::<Result<Vec<_>>>()?;
    for entry_name in &warc_paths {
        let tmp = extract_warc_from_wacz(wacz_path, entry_name)
            .with_context(|| format!("extracting {} from {}", entry_name, wacz_path.display()))?;
        for record in iter_records(tmp.path())? {
            let record = record?;
            if let Some(info) = Warcinfo::from_record(&record) {
                if !info.is_empty() {
                    return Ok(Some(info));
                }
            }
        }
    }
    Ok(None)
}

/// Extract a single named WARC entry from a WACZ ZIP into a temp file.
pub fn extract_warc_from_wacz(
    wacz_path: &Path,
    entry_name: &str,
) -> Result<tempfile::NamedTempFile> {
    use std::io::copy;

    let file = std::fs::File::open(wacz_path)
        .with_context(|| format!("opening WACZ {}", wacz_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut entry = zip
        .by_name(entry_name)
        .with_context(|| format!("entry {} not found in {}", entry_name, wacz_path.display()))?;

    let suffix = if entry_name.ends_with(".warc.gz") { ".warc.gz" } else { ".warc" };
    let mut tmp = tempfile::Builder::new().suffix(suffix).tempfile()?;
    copy(&mut entry, &mut tmp)?;

    Ok(tmp)
}

/// Read a single WARC record located at absolute byte `offset` in a `Read + Seek`
/// WACZ, where the record is one gzip member of `length` bytes (both as given by
/// a CDX entry, translated to an absolute position via the WARC ZIP entry's
/// `data_start`). Gunzips just that slice and parses it, so CDX-guided/streaming
/// indexing can pull one record without reading the rest of the WARC. Returns
/// the record(s) in the member (usually one).
pub(crate) fn record_at<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    offset: u64,
    length: u64,
) -> Result<Vec<crate::warc::WarcRecord>> {
    use std::io::{Read, SeekFrom};
    reader
        .seek(SeekFrom::Start(offset))
        .with_context(|| format!("seeking to offset {offset}"))?;
    let mut buf = vec![0u8; length as usize];
    reader
        .read_exact(&mut buf)
        .with_context(|| format!("reading {length} bytes at offset {offset}"))?;
    let mut decompressed = Vec::new();
    flate2::read::GzDecoder::new(&buf[..])
        .read_to_end(&mut decompressed)
        .context("decompressing WARC record slice")?;
    Ok(crate::warc::parse_warc_records(&decompressed, offset, length)
        .into_iter()
        .filter_map(Result::ok)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_at_reads_one_gzipped_warc_record_slice() {
        use std::io::Write;
        // Build a WARC response record, gzip it as one member.
        let block = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>hi</html>";
        let mut warc = format!(
            "WARC/1.0\r\nWARC-Type: response\r\nWARC-Target-URI: https://ex.com/p\r\n\
             Content-Type: application/http; msgtype=response\r\nContent-Length: {}\r\n\r\n",
            block.len()
        )
        .into_bytes();
        warc.extend_from_slice(block);
        warc.extend_from_slice(b"\r\n\r\n");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&warc).unwrap();
        let member = enc.finish().unwrap();

        // Place the member after padding so the offset is exercised.
        let mut buf = vec![0xEEu8; 64];
        let offset = buf.len() as u64;
        let len = member.len() as u64;
        buf.extend_from_slice(&member);

        let mut cur = std::io::Cursor::new(buf);
        let recs = record_at(&mut cur, offset, len).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].target_uri, "https://ex.com/p");
        assert_eq!(recs[0].http_status, Some(200));
    }

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(FIXTURES).join(name)
    }

    #[test]
    fn list_warc_paths_in_wacz() {
        let paths: Vec<_> = iter_warc_paths(&fixture("simple.wacz"))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        assert!(!paths.is_empty(), "should find at least one WARC entry");
        assert!(
            paths.iter().any(|p| p.contains("archive/")),
            "entries should be under archive/: {paths:?}"
        );
    }

    #[test]
    fn extract_warc_from_wacz_succeeds() {
        let paths: Vec<_> = iter_warc_paths(&fixture("simple.wacz"))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        let first = &paths[0];
        let tmp = extract_warc_from_wacz(&fixture("simple.wacz"), first).unwrap();
        assert!(tmp.path().exists());
        assert!(tmp.path().metadata().unwrap().len() > 0);
    }

    #[test]
    fn read_datapackage_from_simple_wacz() {
        let meta = read_datapackage(&fixture("simple.wacz")).unwrap();
        // simple.wacz has a pages/pages.jsonl with one page entry
        assert!(
            !meta.seed_pages.is_empty(),
            "should have at least one seed page"
        );
        assert_eq!(meta.seed_pages[0].url, "http://example.com/");
        assert_eq!(meta.seed_pages[0].title.as_deref(), Some("Example"));
    }

    #[test]
    fn read_datapackage_reads_nested_metadata() {
        // github-bitcoin-mining.wacz has no top-level title/created; its title
        // lives under a nested `metadata` object, and the timestamp is mtime
        // (epoch ms). Both should be picked up.
        let meta = read_datapackage(&fixture("github-bitcoin-mining.wacz")).unwrap();
        assert_eq!(meta.title.as_deref(), Some("GitHub Bitcoin Mining"));
        let created = meta.created.expect("created should fall back to mtime");
        assert!(created.starts_with("2021-04-17"), "created from mtime: {created}");
    }

    #[test]
    fn search_cdx_finds_url_in_ndjson_wacz() {
        let records = search_cdx(&fixture("simple.wacz"), "http://example.com/").unwrap();
        assert!(!records.is_empty(), "should find CDX entry for example.com");
        assert_eq!(records[0].url, "http://example.com/");
        assert_eq!(records[0].status, 200);
    }

    #[test]
    fn search_cdx_finds_url_in_cdxj_wacz() {
        // github-bitcoin-mining.wacz uses CDXJ format
        let records = search_cdx(
            &fixture("github-bitcoin-mining.wacz"),
            "https://github.com/DocNow/hydrator/pull/78/files",
        )
        .unwrap();
        assert!(!records.is_empty(), "should find CDX entry");
    }

    #[test]
    fn search_cdx_handles_string_typed_numeric_fields() {
        // a.wacz stores offset/length/status as quoted strings in its CDX
        // (e.g. "length":"715"); make sure we still parse those records.
        let records = search_cdx(
            &fixture("a.wacz"),
            "https://storymaps.arcgis.com/stories/278e1b5c18a3474082e583e889705179",
        )
        .unwrap();
        assert!(!records.is_empty(), "should parse string-typed CDX fields");
        assert_eq!(records[0].status, 200);
        assert!(records[0].length > 0, "length should coerce from string");
    }

    #[test]
    fn search_cdx_finds_redirect_record() {
        let records = search_cdx(&fixture("a.wacz"), "https://arcg.is/1zLCSC4").unwrap();
        let redirect = records.iter().find(|r| r.status == 301);
        assert!(redirect.is_some(), "arcg.is shortener should be a 301 redirect");
    }

    #[test]
    fn search_cdx_no_match() {
        let records = search_cdx(&fixture("simple.wacz"), "http://notexist.example/").unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn read_datapackage_reads_software() {
        // a.wacz records the crawler in datapackage.json's `software` field.
        let meta = read_datapackage(&fixture("a.wacz")).unwrap();
        let software = meta.software.expect("datapackage should carry software");
        assert!(software.contains("Browsertrix-Crawler"), "unexpected software: {software}");
    }

    #[test]
    fn read_warcinfo_extracts_software_from_real_wacz() {
        // a.wacz was produced by Browsertrix-Crawler, which writes a warcinfo
        // record with a `software` field.
        let info = read_warcinfo(&fixture("a.wacz")).unwrap().expect("a.wacz has a warcinfo record");
        let software = info.software.expect("warcinfo should carry software");
        assert!(
            software.contains("Browsertrix-Crawler"),
            "unexpected software: {software}"
        );
    }

    #[test]
    fn read_warcinfo_returns_none_when_absent() {
        // simple.wacz has no warcinfo record.
        assert!(read_warcinfo(&fixture("simple.wacz")).unwrap().is_none());
    }
}
