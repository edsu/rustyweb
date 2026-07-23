//! Representative-image thumbnails.
//!
//! Where a crawl's representative image comes from, in priority order: a
//! Browsertrix page **screenshot** of the home page (crawls with screenshots
//! enabled store one per page); failing that the home page's `og:image`; failing
//! that the largest content image the page embeds. We pull the chosen image out
//! of the WACZ (it's one of the resources the crawl captured), downscale it, and
//! cache a small JPEG the UI shows on collection cards and detail pages.
//! Everything here is **best-effort**: a crawl with no usable image produces no
//! thumbnail and the UI falls back to a CSS placeholder.
//!
//! A curator can also pin a thumbnail explicitly (`rustyweb crawl set <id>
//! --image <file>`); a pinned thumbnail is never overwritten by (re)indexing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::http_range::{RangeFetch, RangeReader};
use crate::wacz::{self, CdxjRecord};

/// Longest edge of a cached thumbnail, in pixels. Small: these are card images.
const THUMB_MAX_EDGE: u32 = 400;
/// Ignore images smaller than this (icons, sprites, tracking pixels) when
/// auto-selecting.
const MIN_IMAGE_BYTES: u64 = 5_000;
/// And larger than this: full-res originals are wasteful to fetch + decode for a
/// 400px thumbnail, and plenty of usable images sit below it. A curator can still
/// pin a bigger one with `crawl set --image`.
const MAX_IMAGE_BYTES: u64 = 3_000_000;

/// Path of a crawl's cached (auto-selected) thumbnail.
fn thumb_file(thumbs_dir: &Path, crawl_id: &str) -> PathBuf {
    thumbs_dir.join(format!("{crawl_id}.jpg"))
}

/// Decode `bytes`, downscale to fit `THUMB_MAX_EDGE` (aspect preserved), and
/// write the JPEG to `dest`, creating its parent directory.
fn write_thumbnail(dest: &Path, bytes: &[u8]) -> Result<()> {
    let img = image::load_from_memory(bytes).context("decoding image")?;
    let rgb = img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE).into_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rgb)
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .context("encoding thumbnail")?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(dest, out.into_inner())
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Pin a curator-supplied local image at `dest` (a committable path under the
/// collection). Downscales it like any other thumbnail; its presence is the pin
/// marker, so (re)indexing won't replace it. Shared by crawl and collection
/// thumbnails.
pub(crate) fn set_manual(dest: &Path, image_file: &Path) -> Result<()> {
    let bytes =
        std::fs::read(image_file).with_context(|| format!("reading {}", image_file.display()))?;
    write_thumbnail(dest, &bytes)
}

/// Generate and cache a crawl's representative thumbnail from its WACZ: prefer a
/// Browsertrix page screenshot of the main page, else its `og:image`, else the
/// largest content image the page embeds. Returns whether a thumbnail was
/// written. Skips pinned thumbnails. Best-effort.
///
/// Only works for CDX-streamable WACZs: it locates the screenshot, HTML, and
/// image records through the CDX, the same way indexing does.
pub(crate) fn generate<F>(
    fetch: F,
    thumbs_dir: &Path,
    crawl_id: &str,
    main_page_url: &str,
    pinned_dest: &Path,
) -> Result<bool>
where
    F: RangeFetch + Clone + Send + Sync,
{
    if pinned_dest.exists() {
        return Ok(false); // a curator's committed pinned image stands
    }

    let mut zip = zip::ZipArchive::new(RangeReader::new(fetch.clone()))
        .context("opening WACZ for thumbnail")?;
    let cdx = wacz::cdx_records(&mut zip)?;
    let starts = wacz::warc_data_starts(&mut zip)?;
    drop(zip);

    // 0. Best by far: a Browsertrix screenshot of the main page. It's an actual
    //    rendered picture of the page, so it beats every heuristic below and works
    //    for JS-rendered sites where the saved HTML lists no usable image. Checked
    //    first, before the HTML is even fetched.
    if let Some(bytes) = screenshot_bytes(&fetch, &cdx, &starts, main_page_url)? {
        write_thumbnail(&thumb_file(thumbs_dir, crawl_id), &bytes)?;
        return Ok(true);
    }

    // The main page's HTML → image candidates.
    let Some(html) = fetch_payload(&fetch, &cdx, &starts, main_page_url, "html")? else {
        return Ok(false);
    };
    let (og_image, embedded) = candidate_images(&html);

    // 1. The declared og:image / twitter:image, if it was captured.
    if let Some(src) = og_image {
        if let Some(abs) = resolve(main_page_url, &src) {
            if let Some(bytes) = fetch_payload(&fetch, &cdx, &starts, &abs, "image")? {
                write_thumbnail(&thumb_file(thumbs_dir, crawl_id), &bytes)?;
                return Ok(true);
            }
        }
    }

    // Useful size window (byte size is a cheap proxy for "real content image"):
    // above icons/sprites, below full-res originals.
    let in_window = |rec: &CdxjRecord| (MIN_IMAGE_BYTES..=MAX_IMAGE_BYTES).contains(&rec.length);

    // 2. The largest in-window image the page embeds (works when the site isn't
    //    JS-rendered, so the saved HTML actually lists its images).
    let mut best: Option<&CdxjRecord> = None;
    for src in embedded.iter().take(120) {
        let Some(abs) = resolve(main_page_url, src) else {
            continue;
        };
        if let Some(rec) = find_image_record(&cdx, &abs) {
            if in_window(rec) && best.is_none_or(|b| rec.length > b.length) {
                best = Some(rec);
            }
        }
    }

    // 3. Still nothing? Many sites are JS-rendered, so the *saved* HTML has no
    //    og:image and no <img> — but the crawl still captured the images. Pick the
    //    largest in-window raster image on the crawl's own registrable domain
    //    (via `site_of`), ignoring third-party/CDN/ad images on other domains.
    if best.is_none() {
        let site = crate::search::site_of(main_page_url);
        if !site.is_empty() {
            for rec in &cdx {
                if is_raster_image(rec)
                    && in_window(rec)
                    && crate::search::site_of(&rec.url) == site
                    && best.is_none_or(|b| rec.length > b.length)
                {
                    best = Some(rec);
                }
            }
        }
    }

    if let Some(rec) = best {
        if let Some(bytes) = fetch_record_bytes(&fetch, &starts, rec)? {
            write_thumbnail(&thumb_file(thumbs_dir, crawl_id), &bytes)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Resolve a possibly-relative image reference against the page URL.
fn resolve(base: &str, href: &str) -> Option<String> {
    url::Url::parse(base)
        .ok()
        .and_then(|b| b.join(href.trim()).ok())
        .map(|u| u.to_string())
}

/// The og:image (or twitter:image) plus the `<img>` sources on a page.
fn candidate_images(html: &[u8]) -> (Option<String>, Vec<String>) {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(&String::from_utf8_lossy(html));

    let meta = |sel: &str| -> Option<String> {
        let s = Selector::parse(sel).ok()?;
        doc.select(&s)
            .next()
            .and_then(|e| e.value().attr("content"))
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
    };
    let og = meta("meta[property=\"og:image\"]").or_else(|| meta("meta[name=\"twitter:image\"]"));

    let mut imgs = Vec::new();
    if let Ok(sel) = Selector::parse("img") {
        for el in doc.select(&sel) {
            if let Some(src) = el.value().attr("src") {
                imgs.push(src.to_string());
            } else if let Some(srcset) = el.value().attr("srcset") {
                // Take the first candidate URL from a srcset (before its descriptor).
                if let Some(first) = srcset
                    .split(',')
                    .next()
                    .and_then(|c| c.split_whitespace().next())
                {
                    imgs.push(first.to_string());
                }
            }
        }
    }
    (og, imgs)
}

/// A captured, decodable (raster) image record — the `image` crate can't decode
/// SVG, so those are excluded.
fn is_raster_image(rec: &CdxjRecord) -> bool {
    rec.length != 0 && rec.mime.contains("image") && !rec.mime.contains("svg")
}

/// First CDX record for `url` that is a raster image.
fn find_image_record<'a>(cdx: &'a [CdxjRecord], url: &str) -> Option<&'a CdxjRecord> {
    cdx.iter().find(|c| c.url == url && is_raster_image(c))
}

/// The page URL as-is plus its trailing-slash-toggled variant, so a screenshot
/// keyed on `…/` still matches a `main_page_url` without the slash (or vice
/// versa).
fn page_url_variants(url: &str) -> Vec<String> {
    let url = url.trim();
    match url.strip_suffix('/') {
        Some(stripped) => vec![url.to_string(), stripped.to_string()],
        None => vec![url.to_string(), format!("{url}/")],
    }
}

/// A Browsertrix page screenshot for `page_url`, if the crawl captured one.
/// Browsertrix stores per-page screenshots as WARC records keyed by a `urn:` URL:
/// `urn:thumbnail:<page>` (a small, ready-made JPEG — preferred) and
/// `urn:view:<page>` (the full-page PNG). Matched on the exact page URL,
/// tolerating a trailing-slash difference.
fn screenshot_bytes<F: RangeFetch>(
    fetch: &F,
    cdx: &[CdxjRecord],
    starts: &HashMap<String, u64>,
    page_url: &str,
) -> Result<Option<Vec<u8>>> {
    for scheme in ["urn:thumbnail:", "urn:view:"] {
        for variant in page_url_variants(page_url) {
            if let Some(rec) = find_image_record(cdx, &format!("{scheme}{variant}")) {
                if let Some(bytes) = fetch_record_bytes(fetch, starts, rec)? {
                    return Ok(Some(bytes));
                }
            }
        }
    }
    Ok(None)
}

/// Fetch and return the HTTP body of a CDX record's capture.
fn fetch_record_bytes<F: RangeFetch>(
    fetch: &F,
    starts: &HashMap<String, u64>,
    rec: &CdxjRecord,
) -> Result<Option<Vec<u8>>> {
    let base = rec.filename.rsplit('/').next().unwrap_or(&rec.filename);
    let Some(&start) = starts.get(base) else {
        return Ok(None);
    };
    let from = start + rec.offset;
    let bytes = fetch.fetch(from, from + rec.length)?;
    let records = wacz::records_from_slice(&bytes, from, rec.length)?;
    Ok(records.into_iter().next().map(|r| r.payload))
}

/// Fetch the HTTP body of the first CDX record for `url` whose mime contains
/// `mime_hint`; `None` if there's no such record.
fn fetch_payload<F: RangeFetch>(
    fetch: &F,
    cdx: &[CdxjRecord],
    starts: &HashMap<String, u64>,
    url: &str,
    mime_hint: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(rec) = cdx
        .iter()
        .find(|c| c.url == url && c.length != 0 && c.mime.contains(mime_hint))
    else {
        return Ok(None);
    };
    fetch_record_bytes(fetch, starts, rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            w,
            h,
            image::Rgb([12, 120, 200]),
        ))
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
        buf.into_inner()
    }

    #[test]
    fn set_manual_writes_downscaled_jpeg() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A committable pinned destination under a collection dir.
        let dest = tmp.path().join("collections/c/crawls/abc123.jpg");
        let src = tmp.path().join("pic.png");
        std::fs::write(&src, png_bytes(30, 20)).unwrap();

        set_manual(&dest, &src).unwrap();
        assert!(
            dest.exists(),
            "pinned thumbnail written (parent dir created)"
        );

        // it's a valid, downscaled JPEG
        let out = image::load_from_memory(&std::fs::read(&dest).unwrap()).unwrap();
        assert!(out.width() <= THUMB_MAX_EDGE && out.height() <= THUMB_MAX_EDGE);
    }

    #[test]
    fn page_url_variants_toggles_trailing_slash() {
        assert_eq!(
            page_url_variants("https://ex.com/"),
            vec!["https://ex.com/".to_string(), "https://ex.com".to_string()]
        );
        assert_eq!(
            page_url_variants("https://ex.com/p"),
            vec![
                "https://ex.com/p".to_string(),
                "https://ex.com/p/".to_string()
            ]
        );
    }

    #[test]
    fn generate_skips_a_pinned_thumbnail() {
        // A pinned thumbnail must survive (re)indexing: generate() returns early
        // without regenerating. The pinned check runs before the WACZ is opened,
        // so the fetch source is never read (a dummy file stands in for it).
        let tmp = tempfile::TempDir::new().unwrap();
        let thumbs = tmp.path().join("thumbs");
        let pinned = tmp.path().join("collections/c/crawls/pinned1.jpg");
        let src = tmp.path().join("pic.png");
        std::fs::write(&src, png_bytes(24, 16)).unwrap();
        set_manual(&pinned, &src).unwrap();
        let before = std::fs::read(&pinned).unwrap();

        let dummy = tmp.path().join("dummy.bin");
        std::fs::write(&dummy, b"not a wacz").unwrap();
        let fetch = crate::http_range::FileFetch::open(&dummy).unwrap();
        let wrote = generate(fetch, &thumbs, "pinned1", "https://ex.com/", &pinned).unwrap();

        assert!(
            !wrote,
            "generate() should skip a crawl with a committed pin"
        );
        assert!(
            !thumb_file(&thumbs, "pinned1").exists(),
            "no auto cache written for a pinned crawl"
        );
        let after = std::fs::read(&pinned).unwrap();
        assert_eq!(before, after, "the pinned thumbnail must be left untouched");
    }
}
