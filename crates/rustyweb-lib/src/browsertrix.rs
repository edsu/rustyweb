//! A small client for the Browsertrix REST API — Webrecorder's hosted crawling
//! service, which produces the WACZ files rustyweb serves. It authenticates with
//! a username/password to get a JWT, lists a user's archived items across their
//! orgs, and resolves each item's WACZ resources.
//!
//! There are two ways to get a WACZ's bytes, both surfaced here:
//!   * **streaming** (preferred): [`Client::resources`] returns the per-file
//!     *presigned* S3 URLs from `replay.json`. Those URLs are range-capable, so
//!     they can be stream-indexed via [`crate::http_range`] without downloading —
//!     but they expire (~48 h), so they're for ingest, not durable replay.
//!   * **download**: [`Client::download`] streams the authenticated `/download`
//!     endpoint, for landing a durable copy under `<home>/archive`.
//!
//! HTTP is abstracted behind the [`Transport`] trait (mirroring
//! [`crate::http_range::RangeFetch`]) so the auth / pagination / JSON-parsing
//! logic is unit-tested against canned responses, with no live server or
//! mock-HTTP dependency. End-to-end coverage against a mock Browsertrix API is a
//! later task (rustyweb-15z.7).

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;

/// Default Browsertrix host (Webrecorder's hosted service). Override for
/// self-hosted instances.
pub const DEFAULT_HOST: &str = "https://app.browsertrix.com";

/// Items requested per page when listing. The API paginates; this is the page
/// size the client asks for.
const PAGE_SIZE: usize = 100;

/// Safety valve for the pagination loop: far above any real listing, but bounds
/// the loop if a misbehaving server never signals the last page.
const MAX_PAGES: usize = 100_000;

// ── Transport ────────────────────────────────────────────────────────────────

/// The subset of HTTP the client performs, behind a trait so the client's
/// auth / pagination / parsing logic can be tested against canned responses
/// (see the tests) rather than a live server.
pub trait Transport {
    /// GET `url`, optionally with a bearer token. Returns `(status, body)`.
    fn get(&self, url: &str, bearer: Option<&str>) -> Result<(u16, Vec<u8>)>;
    /// POST `fields` as `application/x-www-form-urlencoded`. Returns
    /// `(status, body)`.
    fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<(u16, Vec<u8>)>;
    /// GET `url` as a streaming reader — for large downloads that must not be
    /// buffered into memory.
    fn get_stream(&self, url: &str, bearer: Option<&str>) -> Result<Box<dyn Read + Send>>;
}

/// A [`Transport`] backed by `ureq`. The agent returns 4xx/5xx as ordinary
/// responses (not transport errors) so the client can read the API's status +
/// message, matching [`crate::http_range`]'s agent.
#[derive(Clone)]
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .new_agent(),
        }
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &str, bearer: Option<&str>) -> Result<(u16, Vec<u8>)> {
        let mut req = self.agent.get(url);
        if let Some(token) = bearer {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        let mut body = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut body)?;
        Ok((status, body))
    }

    fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<(u16, Vec<u8>)> {
        // Build the urlencoded body ourselves (via the `url` crate) rather than
        // depend on a particular ureq form helper, and send it with the matching
        // content type.
        let body = encode_form(fields);
        let resp = self
            .agent
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body.as_bytes())
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status().as_u16();
        let mut out = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut out)?;
        Ok((status, out))
    }

    fn get_stream(&self, url: &str, bearer: Option<&str>) -> Result<Box<dyn Read + Send>> {
        let mut req = self.agent.get(url);
        if let Some(token) = bearer {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("GET {url} failed: HTTP {status}");
        }
        Ok(Box::new(resp.into_body().into_reader()))
    }
}

/// URL-encode form fields as `k=v&k2=v2` (percent-encoding keys and values).
fn encode_form(fields: &[(&str, &str)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in fields {
        ser.append_pair(k, v);
    }
    ser.finish()
}

// ── API types ────────────────────────────────────────────────────────────────

/// The JWT returned by the login endpoint (other fields ignored).
#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
}

/// The signed-in user (`GET /api/users/me`).
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
}

/// An organization the user belongs to. `id` is the `oid` used in item URLs.
#[derive(Debug, Clone, Deserialize)]
pub struct Org {
    pub id: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
}

/// A Browsertrix collection (a named group of crawls). The API's item filters
/// take the `id` (a UUID), not the slug.
#[derive(Debug, Clone, Deserialize)]
pub struct Collection {
    pub id: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
}

/// An archived item — a crawl or an upload — from `all-crawls`.
#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `"crawl"` or `"upload"`; determines the `replay.json` path.
    #[serde(rename = "type", default)]
    pub item_type: String,
    #[serde(rename = "fileSize", default)]
    pub file_size: u64,
    /// QA review rating (1–5) set by a reviewer in Browsertrix. `None` when the
    /// item has never been QA'd.
    #[serde(rename = "reviewStatus", default)]
    pub review_status: Option<u8>,
}

impl Item {
    /// Uploaded items resolve their resources under `/uploads/`, crawls under
    /// `/crawls/`.
    pub fn is_upload(&self) -> bool {
        self.item_type == "upload"
    }

    /// Whether a reviewer has QA'd this item (its `reviewStatus` is set).
    pub fn is_reviewed(&self) -> bool {
        self.review_status.is_some()
    }
}

/// Server-side selection filters for [`Client::items`]. Defaults to no filter
/// (the whole org).
#[derive(Default)]
pub struct ItemQuery<'a> {
    /// Limit to a Browsertrix collection by its id (UUID).
    pub collection_id: Option<&'a str>,
    /// Limit to a single archived item (crawl or upload) by id.
    pub item_id: Option<&'a str>,
}

/// One WACZ file backing an item, from its `replay.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Resource {
    /// Original WACZ filename.
    #[serde(default)]
    pub name: String,
    /// A **presigned**, range-capable URL to the WACZ. Short-lived (~48 h): use
    /// it to stream-index now, not as a durable replay source.
    pub path: String,
    /// `sha256:...` digest of the file, when present.
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub size: u64,
    #[serde(rename = "crawlId", default)]
    pub crawl_id: String,
}

/// Browsertrix's paginated list envelope.
#[derive(Debug, Deserialize)]
struct Paginated<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
    #[serde(default)]
    total: usize,
}

/// The subset of `replay.json` the client needs.
#[derive(Debug, Deserialize)]
struct ReplayJson {
    #[serde(default = "Vec::new")]
    resources: Vec<Resource>,
}

// ── Client ───────────────────────────────────────────────────────────────────

/// An authenticated Browsertrix API client. Generic over the [`Transport`] so
/// tests can drive it with canned responses; the default is the `ureq`-backed
/// [`UreqTransport`].
pub struct Client<T: Transport = UreqTransport> {
    transport: T,
    host: String,
    token: String,
    page_size: usize,
}

impl Client<UreqTransport> {
    /// Log in to `host` with a username/password and return a ready client.
    pub fn login(host: &str, username: &str, password: &str) -> Result<Self> {
        Self::login_with(UreqTransport::default(), host, username, password)
    }

    /// Build a client from an existing bearer token, skipping the login
    /// round-trip. For callers that already hold a JWT (e.g. from the
    /// environment).
    pub fn with_token(host: &str, token: &str) -> Self {
        Self {
            transport: UreqTransport::default(),
            host: host.trim_end_matches('/').to_string(),
            token: token.to_string(),
            page_size: PAGE_SIZE,
        }
    }
}

impl<T: Transport> Client<T> {
    /// Log in using a caller-provided transport. `Client::login` is the usual
    /// entry point; this exists so tests can inject a fake transport.
    pub fn login_with(transport: T, host: &str, username: &str, password: &str) -> Result<Self> {
        let host = host.trim_end_matches('/').to_string();
        let url = format!("{host}/api/auth/jwt/login");
        // fastapi-users' password flow expects form fields `username`/`password`.
        let (status, body) =
            transport.post_form(&url, &[("username", username), ("password", password)])?;
        if !(200..300).contains(&status) {
            bail!(
                "Browsertrix login failed (HTTP {status}): {}",
                body_snippet(&body)
            );
        }
        let login: LoginResponse = parse(&body).context("parsing login response")?;
        Ok(Self {
            transport,
            host,
            token: login.access_token,
            page_size: PAGE_SIZE,
        })
    }

    /// Override the pagination page size (mainly for tests).
    pub fn with_page_size(mut self, n: usize) -> Self {
        self.page_size = n.max(1);
        self
    }

    /// The host this client is bound to (no trailing slash).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The bearer token (for callers that stream presigned/download URLs
    /// themselves).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The signed-in user.
    pub fn me(&self) -> Result<User> {
        self.get_json("/api/users/me")
    }

    /// The user's organizations.
    pub fn orgs(&self) -> Result<Vec<Org>> {
        self.list("/api/orgs", &[])
    }

    /// The collections in an org (id + slug + name), across every page. Used to
    /// resolve a user-supplied slug/name to the UUID the item filters require.
    pub fn collections(&self, oid: &str) -> Result<Vec<Collection>> {
        self.list(&format!("/api/orgs/{oid}/collections"), &[])
    }

    /// Archived items (crawls + uploads) in an org, across every page, narrowed
    /// by `query` (a collection or a single item; the default is the whole org).
    pub fn items(&self, oid: &str, query: &ItemQuery) -> Result<Vec<Item>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(c) = query.collection_id {
            params.push(("collectionId", c.to_string()));
        }
        if let Some(i) = query.item_id {
            params.push(("ids", i.to_string()));
        }
        self.list(&format!("/api/orgs/{oid}/all-crawls"), &params)
    }

    /// The WACZ resources backing an item, from its `replay.json`. Each carries
    /// a presigned `path` suitable for range-streaming, plus hash + size.
    pub fn resources(&self, oid: &str, item: &Item) -> Result<Vec<Resource>> {
        let kind = if item.is_upload() {
            "uploads"
        } else {
            "crawls"
        };
        let replay: ReplayJson =
            self.get_json(&format!("/api/orgs/{oid}/{kind}/{}/replay.json", item.id))?;
        Ok(replay.resources)
    }

    /// The WACZ resources for an item by its id (rather than an [`Item`]),
    /// trying the crawl then the upload `replay.json` path — an archived item is
    /// one or the other. Used to re-resolve a fresh presigned URL for a stored
    /// Browsertrix source at index/replay time.
    pub fn item_resources(&self, oid: &str, item_id: &str) -> Result<Vec<Resource>> {
        let mut last_err = None;
        for kind in ["crawls", "uploads"] {
            match self
                .get_json::<ReplayJson>(&format!("/api/orgs/{oid}/{kind}/{item_id}/replay.json"))
            {
                Ok(r) if !r.resources.is_empty() => return Ok(r.resources),
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("no WACZ resources for Browsertrix item {item_id}")))
    }

    /// The authenticated URL that streams an item's WACZ as a durable download.
    ///
    /// Note: the importer does *not* use this — it downloads each flat,
    /// single-WACZ resource from [`Self::resources`] (which indexes cleanly),
    /// because the combined per-item `/download` can be a nested multi-WACZ. This
    /// is here for callers that want that combined download.
    pub fn download_url(&self, oid: &str, item_id: &str) -> String {
        format!("{}/api/orgs/{oid}/all-crawls/{item_id}/download", self.host)
    }

    /// Open an item's combined WACZ download as a streaming reader. See
    /// [`Self::download_url`] on why the importer uses [`Self::resources`]
    /// instead.
    pub fn download(&self, oid: &str, item_id: &str) -> Result<Box<dyn Read + Send>> {
        let url = self.download_url(oid, item_id);
        self.transport.get_stream(&url, Some(&self.token))
    }

    /// GET a JSON endpoint (path begins with `/`), authenticated, and parse it.
    fn get_json<D: DeserializeOwned>(&self, path: &str) -> Result<D> {
        let url = format!("{}{path}", self.host);
        let (status, body) = self.transport.get(&url, Some(&self.token))?;
        if !(200..300).contains(&status) {
            bail!("GET {url} failed (HTTP {status}): {}", body_snippet(&body));
        }
        parse(&body).with_context(|| format!("parsing response from {url}"))
    }

    /// Fetch every page of a paginated list endpoint and concatenate the items.
    /// `params` are extra query pairs (e.g. filters) appended to each page
    /// request; values are percent-encoded.
    fn list<D: DeserializeOwned>(&self, path: &str, params: &[(&str, String)]) -> Result<Vec<D>> {
        let mut out = Vec::new();
        let mut page = 1usize;
        loop {
            let mut query = format!("page={page}&pageSize={}", self.page_size);
            for (k, v) in params {
                query.push('&');
                query.push_str(k);
                query.push('=');
                query.extend(url::form_urlencoded::byte_serialize(v.as_bytes()));
            }
            let envelope: Paginated<D> = self.get_json(&format!("{path}?{query}"))?;
            let got = envelope.items.len();
            out.extend(envelope.items);
            // Stop at a short/empty page (the last one), or once we've collected
            // everything `total` promised. A full page with more expected means
            // there's another page to fetch.
            let reached_total = envelope.total > 0 && out.len() >= envelope.total;
            if got == 0 || got < self.page_size || reached_total {
                break;
            }
            page += 1;
            if page > MAX_PAGES {
                bail!("{path}: pagination did not terminate after {MAX_PAGES} pages");
            }
        }
        Ok(out)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn parse<D: DeserializeOwned>(body: &[u8]) -> Result<D> {
    Ok(serde_json::from_slice(body)?)
}

/// A short, printable slice of a response body, for error context. Truncated on
/// a char boundary so it can't panic on multi-byte UTF-8.
fn body_snippet(body: &[u8]) -> String {
    const MAX: usize = 300;
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.chars().count() > MAX {
        let head: String = text.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A [`Transport`] that returns canned `(status, body)` for exact URLs.
    #[derive(Default)]
    struct FakeTransport {
        responses: HashMap<String, (u16, String)>,
    }

    impl FakeTransport {
        fn with(mut self, url: &str, status: u16, body: &str) -> Self {
            self.responses
                .insert(url.to_string(), (status, body.to_string()));
            self
        }

        fn lookup(&self, url: &str) -> Result<(u16, Vec<u8>)> {
            self.responses
                .get(url)
                .map(|(s, b)| (*s, b.as_bytes().to_vec()))
                .ok_or_else(|| anyhow::anyhow!("unexpected request: {url}"))
        }
    }

    impl Transport for FakeTransport {
        fn get(&self, url: &str, _bearer: Option<&str>) -> Result<(u16, Vec<u8>)> {
            self.lookup(url)
        }
        fn post_form(&self, url: &str, _fields: &[(&str, &str)]) -> Result<(u16, Vec<u8>)> {
            self.lookup(url)
        }
        fn get_stream(&self, url: &str, _bearer: Option<&str>) -> Result<Box<dyn Read + Send>> {
            let (_status, body) = self.lookup(url)?;
            Ok(Box::new(std::io::Cursor::new(body)))
        }
    }

    const HOST: &str = "https://bt.example";

    fn logged_in(t: FakeTransport) -> Client<FakeTransport> {
        let t = t.with(
            "https://bt.example/api/auth/jwt/login",
            200,
            r#"{"access_token":"tok-123","token_type":"bearer"}"#,
        );
        Client::login_with(t, HOST, "u", "p").expect("login")
    }

    #[test]
    fn login_extracts_the_bearer_token() {
        let c = logged_in(FakeTransport::default());
        assert_eq!(c.token(), "tok-123");
        assert_eq!(c.host(), "https://bt.example");
    }

    #[test]
    fn trailing_slash_in_host_is_trimmed() {
        let t = FakeTransport::default().with(
            "https://bt.example/api/auth/jwt/login",
            200,
            r#"{"access_token":"t"}"#,
        );
        let c = Client::login_with(t, "https://bt.example/", "u", "p").expect("login");
        assert_eq!(c.host(), "https://bt.example");
    }

    #[test]
    fn login_failure_surfaces_status_and_body() {
        let t = FakeTransport::default().with(
            "https://bt.example/api/auth/jwt/login",
            400,
            r#"{"detail":"LOGIN_BAD_CREDENTIALS"}"#,
        );
        let err = Client::login_with(t, HOST, "u", "bad")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("400"), "{err}");
        assert!(err.contains("LOGIN_BAD_CREDENTIALS"), "{err}");
    }

    #[test]
    fn orgs_are_parsed() {
        let c = logged_in(FakeTransport::default().with(
            "https://bt.example/api/orgs?page=1&pageSize=100",
            200,
            r#"{"items":[{"id":"o1","slug":"gov","name":"US Gov"}],"total":1}"#,
        ));
        let orgs = c.orgs().unwrap();
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0].id, "o1");
        assert_eq!(orgs[0].slug, "gov");
    }

    #[test]
    fn collections_are_parsed() {
        let c = logged_in(FakeTransport::default().with(
            "https://bt.example/api/orgs/o1/collections?page=1&pageSize=100",
            200,
            r#"{"items":[{"id":"uuid-1","slug":"gov-arc","name":"US Gov"}],"total":1}"#,
        ));
        let colls = c.collections("o1").unwrap();
        assert_eq!(colls.len(), 1);
        assert_eq!(colls[0].id, "uuid-1");
        assert_eq!(colls[0].slug, "gov-arc");
        assert_eq!(colls[0].name, "US Gov");
    }

    #[test]
    fn items_are_fetched_across_pages() {
        let c = logged_in(
            FakeTransport::default()
                .with(
                    "https://bt.example/api/orgs/o1/all-crawls?page=1&pageSize=2",
                    200,
                    r#"{"items":[{"id":"a","name":"A","type":"crawl"},
                                {"id":"b","name":"B","type":"upload","fileSize":42}],
                        "total":3}"#,
                )
                .with(
                    "https://bt.example/api/orgs/o1/all-crawls?page=2&pageSize=2",
                    200,
                    r#"{"items":[{"id":"c","name":"C","type":"crawl"}],"total":3}"#,
                ),
        )
        .with_page_size(2);

        let items = c.items("o1", &ItemQuery::default()).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "a");
        assert!(!items[0].is_upload());
        assert!(items[1].is_upload());
        assert_eq!(items[1].file_size, 42);
        assert_eq!(items[2].id, "c");
    }

    #[test]
    fn items_query_appends_selection_filters_and_parses_review_status() {
        let c = logged_in(FakeTransport::default().with(
            "https://bt.example/api/orgs/o1/all-crawls?page=1&pageSize=100&collectionId=col-9",
            200,
            r#"{"items":[{"id":"a","name":"A","type":"crawl","reviewStatus":4},
                        {"id":"b","name":"B","type":"crawl"}],"total":2}"#,
        ));
        let query = ItemQuery {
            collection_id: Some("col-9"),
            ..Default::default()
        };
        let items = c.items("o1", &query).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].review_status, Some(4));
        assert!(items[0].is_reviewed());
        assert!(!items[1].is_reviewed());
    }

    #[test]
    fn resources_use_the_crawl_or_upload_path() {
        let c = logged_in(
            FakeTransport::default()
                .with(
                    "https://bt.example/api/orgs/o1/crawls/a/replay.json",
                    200,
                    r#"{"resources":[{"name":"a.wacz","path":"https://files/a.wacz?sig=1",
                                     "hash":"sha256:aa","size":10,"crawlId":"a"}]}"#,
                )
                .with(
                    "https://bt.example/api/orgs/o1/uploads/b/replay.json",
                    200,
                    r#"{"resources":[{"name":"b.wacz","path":"https://files/b.wacz?sig=2"}]}"#,
                ),
        );

        let crawl = Item {
            id: "a".into(),
            name: "A".into(),
            item_type: "crawl".into(),
            file_size: 0,
            review_status: None,
        };
        let res = c.resources("o1", &crawl).unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].path, "https://files/a.wacz?sig=1");
        assert_eq!(res[0].hash, "sha256:aa");
        assert_eq!(res[0].size, 10);

        let upload = Item {
            id: "b".into(),
            name: "B".into(),
            item_type: "upload".into(),
            file_size: 0,
            review_status: None,
        };
        let res = c.resources("o1", &upload).unwrap();
        assert_eq!(res[0].path, "https://files/b.wacz?sig=2");
    }

    #[test]
    fn download_streams_the_authenticated_endpoint() {
        let c = logged_in(FakeTransport::default().with(
            "https://bt.example/api/orgs/o1/all-crawls/a/download",
            200,
            "WACZ-BYTES",
        ));
        let mut r = c.download("o1", "a").unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "WACZ-BYTES");
    }

    #[test]
    fn api_error_includes_status() {
        let c = logged_in(FakeTransport::default().with(
            "https://bt.example/api/orgs?page=1&pageSize=100",
            403,
            r#"{"detail":"Not Allowed"}"#,
        ));
        let err = c.orgs().unwrap_err().to_string();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("Not Allowed"), "{err}");
    }
}
