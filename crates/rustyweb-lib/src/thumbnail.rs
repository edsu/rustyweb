//! Representative-image thumbnails.
//!
//! A crawl's homepage usually declares a social-preview image via
//! `<meta property="og:image">`. We pull that image out of the WACZ (it's one of
//! the resources the crawl captured), downscale it, and cache a small JPEG the UI
//! shows on collection cards and detail pages. Everything here is **best-effort**:
//! a crawl with no og:image, or an image we can't fetch/decode, simply produces no
//! thumbnail and the UI falls back to a CSS placeholder.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::http_range::{RangeFetch, RangeReader};
use crate::wacz;

/// Longest edge of a cached thumbnail, in pixels. Small: these are card images.
const THUMB_MAX_EDGE: u32 = 400;

/// Find the crawl's main-page `og:image` inside the WACZ, fetch that captured
/// image, and write a downscaled JPEG to `<thumbs_dir>/<crawl_id>.jpg`. Returns
/// whether a thumbnail was written.
///
/// Only works for CDX-streamable WACZs (Browsertrix / py-wacz output): it locates
/// the HTML and image records through the CDX, the same way indexing does.
pub(crate) fn generate<F>(
    fetch: F,
    thumbs_dir: &Path,
    crawl_id: &str,
    main_page_url: &str,
) -> Result<bool>
where
    F: RangeFetch + Clone + Send + Sync,
{
    let mut zip = zip::ZipArchive::new(RangeReader::new(fetch.clone()))
        .context("opening WACZ for thumbnail")?;
    let cdx = wacz::cdx_records(&mut zip)?;
    let starts = wacz::warc_data_starts(&mut zip)?;
    drop(zip);

    // The main page's HTML → its og:image URL.
    let Some(html) = fetch_payload(&fetch, &cdx, &starts, main_page_url, "html")? else {
        return Ok(false);
    };
    let og_image = crate::search::extract_html_text(&html).og_image;
    if og_image.trim().is_empty() {
        return Ok(false);
    }

    // Resolve it (og:image is often a relative or protocol-relative URL) against
    // the page URL, then look that captured resource up in the CDX.
    let Some(image_url) = url::Url::parse(main_page_url)
        .ok()
        .and_then(|base| base.join(og_image.trim()).ok())
    else {
        return Ok(false);
    };
    let Some(bytes) = fetch_payload(&fetch, &cdx, &starts, image_url.as_str(), "image")? else {
        return Ok(false);
    };

    // Decode, downscale (aspect preserved), re-encode as JPEG.
    let img = image::load_from_memory(&bytes).context("decoding og:image")?;
    let rgb = img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE).into_rgb8();
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rgb)
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .context("encoding thumbnail")?;

    std::fs::create_dir_all(thumbs_dir)
        .with_context(|| format!("creating {}", thumbs_dir.display()))?;
    let path = thumbs_dir.join(format!("{crawl_id}.jpg"));
    std::fs::write(&path, out.into_inner())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Fetch and return the HTTP response body of the first CDX record whose URL is
/// `url` and whose mime contains `mime_hint`; `None` if there's no such record.
fn fetch_payload<F: RangeFetch>(
    fetch: &F,
    cdx: &[wacz::CdxjRecord],
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
    let base = rec.filename.rsplit('/').next().unwrap_or(&rec.filename);
    let Some(&start) = starts.get(base) else {
        return Ok(None);
    };
    let from = start + rec.offset;
    let bytes = fetch.fetch(from, from + rec.length)?;
    let records = wacz::records_from_slice(&bytes, from, rec.length)?;
    Ok(records.into_iter().next().map(|r| r.payload))
}
