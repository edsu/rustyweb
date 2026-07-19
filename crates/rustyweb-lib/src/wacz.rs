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
    read_datapackage_from(file)
}

/// Read `datapackage.json` + `pages/pages.jsonl` metadata from any `Read + Seek`
/// WACZ — a local file, or an HTTP range reader for streaming indexing.
pub(crate) fn read_datapackage_from<R: std::io::Read + std::io::Seek>(
    reader: R,
) -> Result<WaczMetadata> {
    let mut zip = zip::ZipArchive::new(reader).context("reading WACZ ZIP")?;

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

/// One page's extracted text from a WACZ's `pages/*.jsonl` files.
///
/// Browsertrix records the fully rendered (post-JS) page text here - in
/// `pages/pages.jsonl` (seed pages) and `pages/extraPages.jsonl` (pages found
/// while crawling) - as a `text` field. Older crawls store the rendered text
/// *only* here (not as `urn:text:` WARC records), so indexing must read it or
/// JS-rendered content is unsearchable even though it shows up in replay.
#[derive(Debug)]
pub(crate) struct PageText {
    pub url: String,
    pub ts: String,
    pub title: Option<String>,
    pub text: String,
}

/// Read extracted page text from a WACZ's `pages/pages.jsonl` and
/// `pages/extraPages.jsonl`. Each file's header line ({format,id,title}) carries
/// no url/text and is skipped naturally, so we don't depend on a fixed header
/// row. Entries without a URL or with empty text are dropped; missing files are
/// not an error.
pub(crate) fn read_page_texts<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Vec<PageText> {
    #[derive(Deserialize)]
    struct Entry {
        url: Option<String>,
        title: Option<String>,
        ts: Option<String>,
        text: Option<String>,
    }
    let mut out = Vec::new();
    for name in ["pages/pages.jsonl", "pages/extraPages.jsonl"] {
        let Ok(mut entry) = zip.by_name(name) else {
            continue;
        };
        let mut buf = String::new();
        if entry.read_to_string(&mut buf).is_err() {
            continue;
        }
        for line in buf.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_str::<Entry>(line) {
                let (Some(url), Some(text)) = (e.url, e.text) else {
                    continue;
                };
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                out.push(PageText {
                    url,
                    ts: e.ts.unwrap_or_default(),
                    title: clean(e.title),
                    text,
                });
            }
        }
    }
    out
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

    // Decompress if gzipped. The CDX is often a multi-member gzip (ZipNum
    // blocks), so use MultiGzDecoder to read every block, not just the first.
    let text = if cdx_name.ends_with(".gz") {
        let mut decoder = flate2::read::MultiGzDecoder::new(raw.as_slice());
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
        // The index holds CDXJ data; the extension varies by tool: compressed
        // `.cdx.gz`/`.cdxj.gz` or plain `.cdx`/`.cdxj`. Match any of them.
        if name.starts_with("indexes/")
            && (name.ends_with(".cdx.gz")
                || name.ends_with(".cdxj.gz")
                || name.ends_with(".cdx")
                || name.ends_with(".cdxj"))
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

/// Whether a ZIP entry name is an embedded WARC (`archive/*.warc` or `.warc.gz`).
fn is_warc_entry(name: &str) -> bool {
    name.starts_with("archive/") && (name.ends_with(".warc.gz") || name.ends_with(".warc"))
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
        // The CDX is often a multi-member gzip (ZipNum-clustered blocks); use
        // MultiGzDecoder so every block is read, not just the first.
        let mut out = String::new();
        flate2::read::MultiGzDecoder::new(raw.as_slice()).read_to_string(&mut out)?;
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
    if warcs_stored(zip)? {
        Ok(())
    } else {
        anyhow::bail!(
            "WACZ stores its WARC entries compressed (deflate); CDX-guided \
             extraction needs uncompressed (stored) WARCs. rustyweb falls back to \
             a full WARC scan for such files automatically."
        )
    }
}

/// Whether every embedded WARC is `Stored` (streamable). `false` if any is
/// compressed — used to decide whether a remote WACZ can be streamed or must be
/// downloaded.
pub(crate) fn warcs_stored<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<bool> {
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        if is_warc_entry(entry.name()) && entry.compression() != zip::CompressionMethod::Stored {
            return Ok(false);
        }
    }
    Ok(true)
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
        // `data_start()` is `Option<u64>` (zip 8+): `Some` once the local header
        // has been read, which `by_index` does. A WARC without a known data start
        // can't be seeked into, so skip it (its CDX records are then not fetched).
        if let (true, Some(start)) = (is_warc_entry(&name), entry.data_start()) {
            map.insert(basename(&name).to_string(), start);
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
        .filter(|n| is_warc_entry(n))
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
        for rec in crate::warc::parse_warc_records(&decoded, 0, 0)
            .into_iter()
            .flatten()
        {
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

    let suffix = if entry_name.ends_with(".warc.gz") {
        ".warc.gz"
    } else {
        ".warc"
    };
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
/// Gunzip a single WARC record slice (one gzip member of `length` bytes, located
/// at `offset` — both informational) and parse it. The CDX-guided extractor
/// fetches a record's byte range (a `data_start + offset` slice, possibly
/// concurrently via [`crate::http_range::RangeFetch`]) and hands the bytes here
/// to gunzip + parse without a `Read + Seek`. Returns the record(s) in the member
/// (usually one).
pub(crate) fn records_from_slice(
    buf: &[u8],
    offset: u64,
    length: u64,
) -> Result<Vec<crate::warc::WarcRecord>> {
    use std::io::Read;
    let mut decompressed = Vec::new();
    flate2::read::GzDecoder::new(buf)
        .read_to_end(&mut decompressed)
        .context("decompressing WARC record slice")?;
    Ok(
        crate::warc::parse_warc_records(&decompressed, offset, length)
            .into_iter()
            .filter_map(Result::ok)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdx_records_reads_all_members_of_a_multimember_gzip() {
        use std::io::Write;
        // Two CDX lines, each in its OWN gzip member (ZipNum-style clustering).
        let line = |u: &str| {
            format!("com,example)/ 20200101000000 {{\"url\":\"{u}\",\"mime\":\"text/html\",\"filename\":\"x.warc.gz\",\"offset\":0,\"length\":10,\"status\":200}}\n")
        };
        let gz = |s: String| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(s.as_bytes()).unwrap();
            e.finish().unwrap()
        };
        let mut cdx_gz = gz(line("https://example.com/a"));
        cdx_gz.extend_from_slice(&gz(line("https://example.com/b"))); // 2nd member appended

        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file(
                "indexes/index.cdx.gz",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zw.write_all(&cdx_gz).unwrap();
            zw.finish().unwrap();
        }
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(buf)).unwrap();
        let recs = cdx_records(&mut zip).unwrap();
        let urls: Vec<&str> = recs.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            recs.len(),
            2,
            "must read every gzip member of the CDX, not just the first"
        );
        assert!(urls.contains(&"https://example.com/a") && urls.contains(&"https://example.com/b"));
    }

    #[test]
    fn cdx_records_recognizes_a_plain_cdxj_index() {
        use std::io::Write;
        // Some WACZs name the (uncompressed) index `indexes/index.cdxj`.
        let line = "com,example)/ 20200101000000 {\"url\":\"https://example.com/\",\"mime\":\"text/html\",\"filename\":\"x.warc.gz\",\"offset\":0,\"length\":10,\"status\":200}\n";
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file(
                "indexes/index.cdxj",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zw.write_all(line.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(buf)).unwrap();
        let recs = cdx_records(&mut zip).unwrap();
        assert_eq!(recs.len(), 1, "a plain .cdxj index must be recognized");
        assert_eq!(recs[0].url, "https://example.com/");
    }

    #[test]
    fn read_page_texts_reads_pages_and_extra_pages() {
        use std::io::Write;
        // pages.jsonl (seed pages) + extraPages.jsonl (discovered pages). Header
        // lines, entries without text, and whitespace-only text are all skipped.
        let pages = "{\"format\":\"json-pages-1.0\",\"id\":\"pages\",\"title\":\"All Pages\"}\n\
             {\"id\":\"p1\",\"url\":\"https://ex.com/\",\"title\":\"Home\",\"ts\":\"2022-01-01T00:00:00Z\",\"text\":\"seed body Петиція\"}\n\
             {\"id\":\"p2\",\"url\":\"https://ex.com/no-text\",\"title\":\"NoText\"}\n";
        let extra = "{\"format\":\"json-pages-1.0\",\"id\":\"extraPages\",\"title\":\"Extra\"}\n\
             {\"id\":\"e1\",\"url\":\"https://ex.com/two\",\"ts\":\"2022-01-02T00:00:00Z\",\"text\":\"  discovered text  \"}\n\
             {\"id\":\"e2\",\"url\":\"https://ex.com/blank\",\"text\":\"   \"}\n";
        let mut buf = Vec::new();
        {
            let opt = zip::write::SimpleFileOptions::default();
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file("pages/pages.jsonl", opt).unwrap();
            zw.write_all(pages.as_bytes()).unwrap();
            zw.start_file("pages/extraPages.jsonl", opt).unwrap();
            zw.write_all(extra.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(buf)).unwrap();
        let texts = read_page_texts(&mut zip);
        assert_eq!(
            texts.len(),
            2,
            "only entries with non-empty text: {texts:?}"
        );
        let home = texts.iter().find(|p| p.url == "https://ex.com/").unwrap();
        assert!(home.text.contains("Петиція"), "reads text incl. Cyrillic");
        assert_eq!(home.title.as_deref(), Some("Home"));
        let two = texts
            .iter()
            .find(|p| p.url == "https://ex.com/two")
            .unwrap();
        assert_eq!(two.text, "discovered text", "text is trimmed");
        assert_eq!(two.ts, "2022-01-02T00:00:00Z");
        assert!(two.title.is_none(), "absent title stays None");
    }

    #[test]
    fn records_from_slice_reads_one_gzipped_warc_record() {
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

        // The extractor fetches exactly the record's byte slice, then gunzips +
        // parses it (offset/length are informational).
        let len = member.len() as u64;
        let recs = records_from_slice(&member, 64, len).unwrap();
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
        assert!(
            created.starts_with("2021-04-17"),
            "created from mtime: {created}"
        );
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
        assert!(
            redirect.is_some(),
            "arcg.is shortener should be a 301 redirect"
        );
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
        assert!(
            software.contains("Browsertrix-Crawler"),
            "unexpected software: {software}"
        );
    }

    #[test]
    fn read_warcinfo_extracts_software_from_real_wacz() {
        // a.wacz was produced by Browsertrix-Crawler, which writes a warcinfo
        // record with a `software` field.
        let info = read_warcinfo(&fixture("a.wacz"))
            .unwrap()
            .expect("a.wacz has a warcinfo record");
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
