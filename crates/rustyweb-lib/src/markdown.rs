//! Render curator-authored Markdown (collection narratives, crawl notes) to a
//! **safe HTML subset** for splicing into the server-rendered pages.
//!
//! Curator- and importer-supplied content is untrusted, so we do not pass raw
//! HTML through: [`render`] treats HTML events as escaped text (no `<script>`
//! injection), validates link/image destinations to `http`/`https`/`mailto`
//! (dropping `javascript:`, `data:`, etc.), and neutralizes images to their alt
//! text. This uses only `pulldown-cmark` (pure Rust) — no `ammonia`/`html5ever`
//! sanitizer chain.

use maud::PreEscaped;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// Render a Markdown string to a sanitized HTML fragment.
pub fn render(md: &str) -> PreEscaped<String> {
    // `drop_link` tracks a link whose destination we rejected: we skip its
    // Start/End tags but keep the inner text (so the words survive, unlinked).
    // Links cannot nest in CommonMark, so a single bool would do; a counter is
    // simply robust.
    let mut drop_link = 0usize;
    let events: Vec<Event> = Parser::new(md)
        .filter_map(|ev| match ev {
            // Never pass raw HTML through — emit it as escaped text instead.
            Event::Html(s) | Event::InlineHtml(s) => Some(Event::Text(s)),

            // Drop images entirely; their alt text (inner Text events) remains.
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,

            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => match safe_dest(&dest_url) {
                Some(dest) => Some(Event::Start(Tag::Link {
                    link_type,
                    dest_url: dest.into(),
                    title,
                    id,
                })),
                None => {
                    drop_link += 1;
                    None
                }
            },
            Event::End(TagEnd::Link) if drop_link > 0 => {
                drop_link -= 1;
                None
            }

            other => Some(other),
        })
        .collect();

    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    PreEscaped(html)
}

/// Validate a link/image destination. Relative paths and `#anchors` are allowed;
/// an absolute URL is allowed only for `http`/`https`/`mailto`. Anything with
/// another scheme (`javascript:`, `data:`, …) is rejected (`None`).
fn safe_dest(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    // A "scheme" is text up to the first ':' that appears before any '/'.
    let colon = u.find(':');
    let slash = u.find('/');
    let has_scheme = match (colon, slash) {
        (Some(c), Some(s)) => c < s,
        (Some(_), None) => true,
        _ => false,
    };
    if has_scheme {
        let scheme = u[..colon.unwrap()].to_ascii_lowercase();
        matches!(scheme.as_str(), "http" | "https" | "mailto").then(|| u.to_string())
    } else {
        Some(u.to_string()) // relative path or #anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(md: &str) -> String {
        render(md).0
    }

    #[test]
    fn allowed_subset_renders() {
        let out =
            html("# Heading\n\nA **bold** and *italic* and `code`.\n\n- one\n- two\n\n> quote\n");
        for frag in [
            "<h1>",
            "<strong>",
            "<em>",
            "<code>",
            "<ul>",
            "<li>",
            "<p>",
            "<blockquote>",
        ] {
            assert!(out.contains(frag), "expected {frag} in: {out}");
        }
    }

    #[test]
    fn raw_html_is_escaped_not_passed_through() {
        let out = html("hi <script>alert(1)</script> <b>x</b>");
        assert!(
            !out.contains("<script>"),
            "raw <script> must not pass through: {out}"
        );
        assert!(out.contains("&lt;script&gt;"), "should be escaped: {out}");
        assert!(
            !out.contains("<b>"),
            "raw inline HTML must not pass through: {out}"
        );
    }

    #[test]
    fn javascript_link_is_dropped_but_text_kept() {
        let out = html("click [here](javascript:alert(1)) now");
        assert!(
            !out.to_lowercase().contains("javascript"),
            "js scheme dropped: {out}"
        );
        assert!(!out.contains("<a "), "no anchor for a rejected dest: {out}");
        assert!(out.contains("here"), "link text preserved: {out}");
    }

    #[test]
    fn safe_link_is_kept() {
        let out = html("[site](https://example.org/x) and [mail](mailto:a@b.org)");
        assert!(out.contains("href=\"https://example.org/x\""), "{out}");
        assert!(out.contains("href=\"mailto:a@b.org\""), "{out}");
    }

    #[test]
    fn image_neutralized_to_alt_text() {
        let out = html("![a diagram](https://ex.org/x.png) and ![evil](javascript:x)");
        assert!(!out.contains("<img"), "images dropped: {out}");
        assert!(out.contains("a diagram"), "alt text kept: {out}");
        assert!(!out.to_lowercase().contains("javascript"), "{out}");
    }

    #[test]
    fn relative_and_anchor_links_allowed() {
        let out = html("[a](page.html) [b](#sec) [c](/root)");
        assert!(out.contains("href=\"page.html\""), "{out}");
        assert!(out.contains("href=\"#sec\""), "{out}");
        assert!(out.contains("href=\"/root\""), "{out}");
    }
}
