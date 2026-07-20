//! Representative-image thumbnails.
//!
//! A crawl's home page usually points at a representative image — via
//! `<meta property="og:image">`, or failing that the largest content image the
//! page embeds. We pull that image out of the WACZ (it's one of the resources the
//! crawl captured), downscale it, and cache a small JPEG the UI shows on
//! collection cards and detail pages. Everything here is **best-effort**: a crawl
//! with no usable image produces no thumbnail and the UI falls back to a CSS
//! placeholder.
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

/// Path of a crawl's cached thumbnail.
fn thumb_file(thumbs_dir: &Path, crawl_id: &str) -> PathBuf {
    thumbs_dir.join(format!("{crawl_id}.jpg"))
}

/// Marker next to a thumbnail: present when a curator pinned it, so (re)indexing
/// won't overwrite their choice.
fn pin_file(thumbs_dir: &Path, crawl_id: &str) -> PathBuf {
    thumbs_dir.join(format!("{crawl_id}.pinned"))
}

/// Whether a crawl's thumbnail was pinned by a curator.
pub(crate) fn is_pinned(thumbs_dir: &Path, crawl_id: &str) -> bool {
    pin_file(thumbs_dir, crawl_id).exists()
}

/// Decode `bytes`, downscale to fit `THUMB_MAX_EDGE` (aspect preserved), and
/// write the JPEG to `<thumbs_dir>/<crawl_id>.jpg`.
fn write_thumbnail(thumbs_dir: &Path, crawl_id: &str, bytes: &[u8]) -> Result<()> {
    let img = image::load_from_memory(bytes).context("decoding image")?;
    let rgb = img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE).into_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rgb)
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .context("encoding thumbnail")?;
    std::fs::create_dir_all(thumbs_dir)
        .with_context(|| format!("creating {}", thumbs_dir.display()))?;
    let path = thumb_file(thumbs_dir, crawl_id);
    std::fs::write(&path, out.into_inner())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Pin a curator-supplied local image as a crawl's thumbnail. Downscales it like
/// any other thumbnail and marks it pinned so indexing won't replace it.
pub(crate) fn set_manual(thumbs_dir: &Path, crawl_id: &str, image_file: &Path) -> Result<()> {
    let bytes =
        std::fs::read(image_file).with_context(|| format!("reading {}", image_file.display()))?;
    write_thumbnail(thumbs_dir, crawl_id, &bytes)?;
    std::fs::write(pin_file(thumbs_dir, crawl_id), b"").context("writing pin marker")?;
    Ok(())
}

/// Generate and cache a crawl's representative thumbnail from its WACZ: prefer the
/// main page's `og:image`, else the largest content image the page embeds.
/// Returns whether a thumbnail was written. Skips pinned thumbnails. Best-effort.
///
/// Only works for CDX-streamable WACZs: it locates the HTML and image records
/// through the CDX, the same way indexing does.
pub(crate) fn generate<F>(
    fetch: F,
    thumbs_dir: &Path,
    crawl_id: &str,
    main_page_url: &str,
) -> Result<bool>
where
    F: RangeFetch + Clone + Send + Sync,
{
    if is_pinned(thumbs_dir, crawl_id) {
        return Ok(false); // a curator's pinned image stands
    }

    let mut zip = zip::ZipArchive::new(RangeReader::new(fetch.clone()))
        .context("opening WACZ for thumbnail")?;
    let cdx = wacz::cdx_records(&mut zip)?;
    let starts = wacz::warc_data_starts(&mut zip)?;
    drop(zip);

    // The main page's HTML → image candidates.
    let Some(html) = fetch_payload(&fetch, &cdx, &starts, main_page_url, "html")? else {
        return Ok(false);
    };
    let (og_image, embedded) = candidate_images(&html);

    // 1. Preferred: the declared og:image / twitter:image, if it was captured.
    if let Some(src) = og_image {
        if let Some(abs) = resolve(main_page_url, &src) {
            if let Some(bytes) = fetch_payload(&fetch, &cdx, &starts, &abs, "image")? {
                write_thumbnail(thumbs_dir, crawl_id, &bytes)?;
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
            write_thumbnail(thumbs_dir, crawl_id, &bytes)?;
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
    fn set_manual_writes_and_pins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let thumbs = tmp.path().join("thumbs");
        let src = tmp.path().join("pic.png");
        std::fs::write(&src, png_bytes(30, 20)).unwrap();

        assert!(!is_pinned(&thumbs, "abc123"));
        set_manual(&thumbs, "abc123", &src).unwrap();
        assert!(thumb_file(&thumbs, "abc123").exists(), "thumbnail written");
        assert!(is_pinned(&thumbs, "abc123"), "marked pinned");

        // it's a valid, downscaled JPEG
        let out = image::load_from_memory(&std::fs::read(thumb_file(&thumbs, "abc123")).unwrap())
            .unwrap();
        assert!(out.width() <= THUMB_MAX_EDGE && out.height() <= THUMB_MAX_EDGE);
    }
}
