use std::path::Path;

use anyhow::{Context, Result};
use tantivy::aggregation::agg_req::Aggregations;
use tantivy::aggregation::{AggContextParams, AggregationCollector};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::QueryParser;
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED,
    STRING, TEXT,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

const FIELD_DOC_TYPE: &str = "doc_type";
const FIELD_CRAWL_ID: &str = "crawl_id";
const FIELD_CRAWL_NAME: &str = "crawl_name";
const FIELD_URL: &str = "url";
const FIELD_TS: &str = "timestamp";
const FIELD_TITLE: &str = "title";
const FIELD_BODY: &str = "body";
/// Exact host of a page URL (e.g. `www.example.com`), for `domain:` filtering.
const FIELD_DOMAIN: &str = "domain";
/// Registrable domain of a page URL (eTLD+1, e.g. `example.com` for any
/// `*.example.com` host), for the cross-subdomain `site:` filter and Site facet.
const FIELD_SITE: &str = "site";
/// Tokenized words from a page URL (host + path), so URL words are searchable.
const FIELD_URL_TOKENS: &str = "url_tokens";
/// Page description from `<meta name=description>` / `og:description`.
const FIELD_DESCRIPTION: &str = "description";
/// Concatenated `<h1>`/`<h2>` heading text.
const FIELD_HEADINGS: &str = "headings";
/// `<meta name=keywords>` content.
const FIELD_KEYWORDS: &str = "keywords";
/// Page author (`<meta name=author>` / `article:author`), for `author:` search.
const FIELD_AUTHOR: &str = "author";
/// Four-digit crawl year (from the page timestamp), for `year:` filtering.
const FIELD_YEAR: &str = "year";
/// Six-digit crawl month `YYYYMM` (from the page timestamp), for `month:`
/// filtering/range and the results timeline.
const FIELD_MONTH: &str = "month";
/// Coarse media type of the page: `html` or `pdf`, for `type:` filtering.
const FIELD_MEDIA_TYPE: &str = "type";
/// Primary language subtag from `<html lang>` (e.g. `en`), for `lang:` filtering.
const FIELD_LANG: &str = "lang";
/// Curated collection id (slug) this document belongs to, for `collection:` filtering.
const FIELD_COLLECTION: &str = "collection";
/// HTTP response status code of the capture, for `status:200` filtering.
const FIELD_STATUS: &str = "status";
/// Year from the HTTP `Last-Modified` header, for `modified:2015` filtering
/// (when the content was authored, vs `year:` = when it was crawled).
const FIELD_MODIFIED: &str = "modified";

/// How much more a title match counts than a body/url match when ranking.
const TITLE_BOOST: tantivy::Score = 3.0;
/// Upper bound on captures scanned per query for URL grouping (field collapsing
/// has no native Tantivy support, so we group over the top-scored window). When
/// a query matches more captures than this, grouping and `total_hits` cover only
/// the top `CANDIDATE_CAP` and [`SearchResponse::capped`] is set. Two cost
/// notes: every query reads this many stored docs (to read each candidate's URL
/// for grouping), and facet/timeline counts are unaffected by this bound — they
/// come from the aggregation, which is exact over the full match set.
const CANDIDATE_CAP: usize = 1000;
/// Headings rank above body text but below the title.
const HEADINGS_BOOST: tantivy::Score = 2.0;

pub struct SearchIndex {
    index: Index,
    /// Present only when opened for writing. The server opens read-only so it
    /// does not hold Tantivy's exclusive write lock, letting `index` run while
    /// the server is serving.
    writer: Option<IndexWriter>,
}

impl SearchIndex {
    /// Open the index for writing (indexing). Creates it if needed and acquires
    /// Tantivy's exclusive write lock for the lifetime of this value.
    pub fn open(index_dir: &Path) -> Result<Self> {
        let index = Self::open_index(index_dir)?;
        let writer = index.writer(50_000_000)?;
        Ok(Self {
            index,
            writer: Some(writer),
        })
    }

    /// Open the index read-only (searching). Does not create a writer, so it
    /// does not take the write lock; indexing can proceed concurrently.
    pub fn open_read_only(index_dir: &Path) -> Result<Self> {
        let index = Self::open_index(index_dir)?;
        Ok(Self {
            index,
            writer: None,
        })
    }

    fn open_index(index_dir: &Path) -> Result<Index> {
        std::fs::create_dir_all(index_dir)?;
        let schema = build_schema();
        if index_dir.join("meta.json").exists() {
            let index = Index::open_in_dir(index_dir)
                .with_context(|| format!("opening Tantivy index at {}", index_dir.display()))?;
            // A stored schema that differs from the current one (e.g. after new
            // fields were added) can't be written or queried correctly. Fail
            // with a clear message instead of panicking on a missing field.
            if index.schema() != schema {
                anyhow::bail!(
                    "the search index at {} was built with an older schema; \
                     run `rustyweb reindex` to rebuild it",
                    index_dir.display()
                );
            }
            Ok(index)
        } else {
            Index::create_in_dir(index_dir, schema)
                .with_context(|| format!("creating Tantivy index at {}", index_dir.display()))
        }
    }

    fn writer_mut(&mut self) -> &mut IndexWriter {
        self.writer
            .as_mut()
            .expect("SearchIndex opened read-only; no writer available")
    }

    /// Remove all documents (pages and the collection doc) belonging to a
    /// collection.  Call this before re-indexing a collection so that
    /// re-indexing is an upsert rather than an append.
    ///
    /// Tantivy applies a delete only to documents committed before it, so the
    /// caller should `delete_collection()` first, then `index_page()` /
    /// `index_collection()`, then `commit()` - the fresh documents survive.
    pub fn delete_collection(&mut self, crawl_id: &str) {
        let field = self.index.schema().get_field(FIELD_CRAWL_ID).unwrap();
        self.writer_mut()
            .delete_term(Term::from_field_text(field, crawl_id));
    }

    /// Index a single page from an archive. Fields not set on the [`Page`]
    /// default to empty, so callers only populate what they have.
    pub fn index_page(&mut self, page: &Page) -> Result<()> {
        let schema = self.index.schema();
        let mut doc = TantivyDocument::default();
        doc.add_text(schema.get_field(FIELD_DOC_TYPE).unwrap(), "page");
        doc.add_text(schema.get_field(FIELD_CRAWL_ID).unwrap(), page.crawl_id);
        doc.add_text(schema.get_field(FIELD_CRAWL_NAME).unwrap(), page.crawl_name);
        doc.add_text(schema.get_field(FIELD_COLLECTION).unwrap(), page.collection);
        doc.add_text(schema.get_field(FIELD_URL).unwrap(), page.url);
        doc.add_text(schema.get_field(FIELD_TS).unwrap(), page.timestamp);
        doc.add_text(schema.get_field(FIELD_TITLE).unwrap(), page.title);
        doc.add_text(schema.get_field(FIELD_BODY).unwrap(), page.body);
        doc.add_text(
            schema.get_field(FIELD_DESCRIPTION).unwrap(),
            page.description,
        );
        doc.add_text(schema.get_field(FIELD_HEADINGS).unwrap(), page.headings);
        doc.add_text(schema.get_field(FIELD_KEYWORDS).unwrap(), page.keywords);
        doc.add_text(schema.get_field(FIELD_AUTHOR).unwrap(), page.author);
        // Derived URL fields: an exact host for `domain:` filtering, and the
        // URL's words tokenized so they're searchable as ordinary terms.
        doc.add_text(schema.get_field(FIELD_DOMAIN).unwrap(), domain_of(page.url));
        doc.add_text(schema.get_field(FIELD_SITE).unwrap(), site_of(page.url));
        doc.add_text(
            schema.get_field(FIELD_URL_TOKENS).unwrap(),
            url_search_text(page.url),
        );
        // Numeric year/month for range filtering and the timeline; omitted when
        // there's no usable date.
        if let Some(year) = year_of(page.timestamp) {
            doc.add_u64(schema.get_field(FIELD_YEAR).unwrap(), year);
        }
        if let Some(month) = month_of(page.timestamp) {
            doc.add_u64(schema.get_field(FIELD_MONTH).unwrap(), month);
        }
        doc.add_text(schema.get_field(FIELD_MEDIA_TYPE).unwrap(), page.media_type);
        // Language: the declared `<html lang>` wins; if absent, fall back to
        // detecting it from the body text (empty when nothing is confident).
        let lang = if page.lang.trim().is_empty() {
            detect_lang(page.body).unwrap_or_default()
        } else {
            primary_lang(page.lang)
        };
        doc.add_text(schema.get_field(FIELD_LANG).unwrap(), &lang);
        if let Some(status) = page.status {
            doc.add_u64(schema.get_field(FIELD_STATUS).unwrap(), status as u64);
        }
        if let Some(year) = page.modified_year {
            doc.add_u64(schema.get_field(FIELD_MODIFIED).unwrap(), year);
        }
        self.writer_mut().add_document(doc)?;
        Ok(())
    }

    /// Index a collection-level document so the collection itself is searchable.
    /// `body` should be the concatenation of the description and seed page titles/URLs.
    pub fn index_collection(
        &mut self,
        crawl_id: &str,
        crawl_name: &str,
        collection: &str,
        body: &str,
    ) -> Result<()> {
        let schema = self.index.schema();
        let mut doc = TantivyDocument::default();
        doc.add_text(schema.get_field(FIELD_DOC_TYPE).unwrap(), "collection");
        doc.add_text(schema.get_field(FIELD_CRAWL_ID).unwrap(), crawl_id);
        doc.add_text(schema.get_field(FIELD_CRAWL_NAME).unwrap(), crawl_name);
        doc.add_text(schema.get_field(FIELD_COLLECTION).unwrap(), collection);
        doc.add_text(schema.get_field(FIELD_URL).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_TS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_TITLE).unwrap(), crawl_name);
        doc.add_text(schema.get_field(FIELD_BODY).unwrap(), body);
        // Collection docs have no page URL or HTML metadata; keep those empty.
        doc.add_text(schema.get_field(FIELD_DESCRIPTION).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_HEADINGS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_KEYWORDS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_AUTHOR).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_DOMAIN).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_SITE).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_URL_TOKENS).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_MEDIA_TYPE).unwrap(), "");
        doc.add_text(schema.get_field(FIELD_LANG).unwrap(), "");
        self.writer_mut().add_document(doc)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer_mut().commit()?;
        Ok(())
    }

    /// Number of searchable segments. A healthy index has a handful; hundreds
    /// means background merges haven't kept up (e.g. they failed on a full
    /// disk), which slows *every* query — a search fans out across all segments.
    pub fn segment_count(&self) -> Result<usize> {
        Ok(self.index.searchable_segment_ids()?.len())
    }

    /// Compact the index by merging segments down toward `target_segments`
    /// (clamped to ≥ 1), so queries fan out across far fewer segments. Merges
    /// smallest-first in bounded batches, waiting for each merge before starting
    /// the next, so peak transient disk stays ~one merged batch rather than a
    /// second copy of the whole index (a merge writes the new segment before the
    /// inputs are freed). A smaller `target` compacts more but raises that peak
    /// (~index_size / target). Needs a writer. Returns `(before, after)` counts.
    pub fn optimize(
        &mut self,
        target_segments: usize,
        progress: Option<&dyn crate::index::IndexProgress>,
    ) -> Result<(usize, usize)> {
        // Segments merged per round: bounds each merge's size (hence peak disk)
        // and gives the spinner something to tick.
        const BATCH: usize = 32;
        let target = target_segments.max(1);
        // Take exclusive control of merging. Otherwise, as soon as our first
        // explicit merge finishes, Tantivy's default LogMergePolicy (triggered
        // on merge completion) schedules *background* merges over the remaining
        // segments — which then race our next explicit merge and consume its
        // segments out from under it ("couldn't find segment in SegmentManager").
        self.writer_mut()
            .set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
        let before = self.index.searchable_segment_ids()?.len();
        if let Some(p) = progress {
            p.begin("optimize");
            p.phase(&format!("{before} segments"));
        }

        let mut prev = usize::MAX;
        loop {
            // Re-read each round: a merge replaces its inputs with one segment.
            let mut metas = self.index.searchable_segment_metas()?;
            let n = metas.len();
            // Stop at the target, or if a round made no progress (defensive:
            // never spin, even if a merge unexpectedly didn't reduce the count).
            if n <= target || n >= prev {
                break;
            }
            prev = n;
            // Smallest-first keeps early merges cheap; merge enough to move
            // toward `target`, capped by BATCH so one merge can't blow up disk.
            metas.sort_by_key(|m| m.num_docs());
            let take = (n - target + 1).clamp(2, BATCH);
            let ids: Vec<_> = metas.iter().take(take).map(|m| m.id()).collect();
            self.writer_mut()
                .merge(&ids)
                .wait()
                .context("merging segments while optimizing the index")?;
            if let Some(p) = progress {
                p.phase(&format!("{} segments", n - (take - 1)));
            }
        }

        // Delete the now-orphaned input segment files and persist the tidy meta.
        self.writer_mut()
            .garbage_collect_files()
            .wait()
            .context("garbage-collecting merged segment files")?;
        self.writer_mut().commit().context("committing optimize")?;
        let after = self.index.searchable_segment_ids()?.len();
        if let Some(p) = progress {
            p.finish();
        }
        Ok((before, after))
    }

    /// Disable Tantivy's automatic segment merging on this writer, so each
    /// `commit()` leaves its own segment. Tests use this to build a deliberately
    /// fragmented index to exercise [`Self::optimize`].
    #[cfg(test)]
    fn disable_auto_merge(&mut self) {
        self.writer_mut()
            .set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
    }

    /// Facet counts across the whole archive (a match-all query), for homepage
    /// browse entry points. Runs only the aggregation — no result fetching or
    /// URL grouping — so it is cheap.
    pub fn facet_overview(&self) -> Result<Vec<FacetGroup>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let agg_collector =
            AggregationCollector::from_aggs(facet_aggregations(), AggContextParams::default());
        let agg_results = searcher.search(&tantivy::query::AllQuery, &agg_collector)?;
        let agg_json = serde_json::to_value(&agg_results).unwrap_or_default();
        Ok(facets_from_aggregations(&agg_json))
    }

    /// Facet counts restricted to one collection or one crawl, for the scoped
    /// facet overview on detail pages. Same aggregation as [`facet_overview`], but
    /// run over a term query on the scope field instead of match-all — still just
    /// the aggregation (no result fetch), so still cheap.
    pub fn facet_overview_scoped(&self, scope: FacetScope) -> Result<Vec<FacetGroup>> {
        let schema = self.index.schema();
        let (field, value) = match scope {
            FacetScope::Collection(v) => (schema.get_field(FIELD_COLLECTION).unwrap(), v),
            FacetScope::Crawl(v) => (schema.get_field(FIELD_CRAWL_ID).unwrap(), v),
        };
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let query = tantivy::query::TermQuery::new(
            Term::from_field_text(field, value),
            IndexRecordOption::Basic,
        );
        let agg_collector =
            AggregationCollector::from_aggs(facet_aggregations(), AggContextParams::default());
        let agg_results = searcher.search(&query, &agg_collector)?;
        let agg_json = serde_json::to_value(&agg_results).unwrap_or_default();
        Ok(facets_from_aggregations(&agg_json))
    }

    /// Search the top `limit` results by relevance. A thin wrapper over
    /// [`search_faceted`](Self::search_faceted) that returns only the hits (no
    /// facet counts, no pagination); kept for callers that don't need them.
    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<SearchResult>> {
        Ok(self.search_faceted(query_str, limit, 0)?.results)
    }

    /// Search with facet counts and pagination. Returns one page of results
    /// (`limit` hits starting at `offset`), the total number of matches, and
    /// facet buckets (counts per value) for each facet dimension, all computed
    /// from the same query in a single pass.
    pub fn search_faceted(
        &self,
        query_str: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResponse> {
        // Map the `crawl:` filter alias onto its real `crawl_id` field before
        // parsing (the query parser resolves against schema field names).
        let query_str = &rewrite_crawl_alias(query_str);
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let schema = self.index.schema();

        let title_f = schema.get_field(FIELD_TITLE).unwrap();
        let body_f = schema.get_field(FIELD_BODY).unwrap();
        let doc_type_f = schema.get_field(FIELD_DOC_TYPE).unwrap();
        let coll_id_f = schema.get_field(FIELD_CRAWL_ID).unwrap();
        let coll_name_f = schema.get_field(FIELD_CRAWL_NAME).unwrap();
        let url_f = schema.get_field(FIELD_URL).unwrap();
        let ts_f = schema.get_field(FIELD_TS).unwrap();
        let domain_f = schema.get_field(FIELD_DOMAIN).unwrap();
        let description_f = schema.get_field(FIELD_DESCRIPTION).unwrap();
        let headings_f = schema.get_field(FIELD_HEADINGS).unwrap();
        let keywords_f = schema.get_field(FIELD_KEYWORDS).unwrap();
        let author_f = schema.get_field(FIELD_AUTHOR).unwrap();
        let url_tokens_f = schema.get_field(FIELD_URL_TOKENS).unwrap();
        let collection_f = schema.get_field(FIELD_COLLECTION).unwrap();

        // Bare words search the title, headings, body, description, keywords,
        // author, and URL words. Other fields (domain:, url:, title:, author:)
        // are also reachable by explicit `field:` syntax.
        let mut query_parser = QueryParser::for_index(
            &self.index,
            vec![
                title_f,
                headings_f,
                body_f,
                description_f,
                keywords_f,
                author_f,
                url_tokens_f,
            ],
        );
        // Require all terms by default (`climate change` means both), which
        // matches what people expect from a search box more than OR does.
        query_parser.set_conjunction_by_default();
        // A title match is the strongest relevance signal; headings next.
        query_parser.set_field_boost(title_f, TITLE_BOOST);
        query_parser.set_field_boost(headings_f, HEADINGS_BOOST);
        // Parse leniently: a malformed query (stray quote, bad `field:`) yields
        // a best-effort query instead of a hard error, so the search box never
        // 500s while someone is experimenting with syntax.
        let (query, _errors) = query_parser.parse_query_lenient(query_str);

        // One pass over the query: a bounded window of top-scored captures (for
        // grouping), the total capture count, and the facet counts (a terms
        // aggregation per dimension over the fast fields).
        let top_collector = TopDocs::with_limit(CANDIDATE_CAP).order_by_score();
        let agg_collector =
            AggregationCollector::from_aggs(facet_aggregations(), AggContextParams::default());
        let (candidates, total_captures, agg_results) =
            searcher.search(&query, &(top_collector, Count, agg_collector))?;

        // Collapse repeat captures of the same URL: walking the candidates in
        // score order, the first (best-ranked) capture of a URL becomes the
        // result and later captures just bump its count. Collection-level docs
        // and any capture with no URL are never merged. Grouping is over the
        // top `CANDIDATE_CAP` captures, so `capped` flags when more matched.
        struct Group {
            addr: tantivy::DocAddress,
            captures: usize,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut by_url: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (_score, addr) in &candidates {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let url = get_text(&doc, url_f);
            let is_page = get_text(&doc, doc_type_f) == "page";
            if is_page && !url.is_empty() {
                if let Some(&gi) = by_url.get(&url) {
                    groups[gi].captures += 1;
                    continue;
                }
                by_url.insert(url, groups.len());
            }
            groups.push(Group {
                addr: *addr,
                captures: 1,
            });
        }
        let total_hits = groups.len();
        let capped = total_captures > CANDIDATE_CAP;

        // Generate snippets only for the requested page of groups (snippet
        // generation re-analyzes text, so we defer it past grouping).
        let mut snippet_gen = SnippetGenerator::create(&searcher, &query, body_f)?;
        // Tantivy's default snippet window is 150 chars; widen it for more
        // context around the matched terms in search results.
        snippet_gen.set_max_num_chars(350);

        let mut results = Vec::new();
        for g in groups.iter().skip(offset).take(limit) {
            let doc: TantivyDocument = searcher.doc(g.addr)?;
            let snippet = snippet_gen.snippet_from_doc(&doc);
            results.push(SearchResult {
                doc_type: get_text(&doc, doc_type_f),
                crawl_id: get_text(&doc, coll_id_f),
                crawl_name: get_text(&doc, coll_name_f),
                collection: get_text(&doc, collection_f),
                url: get_text(&doc, url_f),
                domain: get_text(&doc, domain_f),
                timestamp: get_text(&doc, ts_f),
                title: get_text(&doc, title_f),
                description: get_text(&doc, description_f),
                snippet: snippet.to_html(),
                capture_count: g.captures,
            });
        }

        // Facets and the timeline are aggregation-derived: they count *captures*
        // and are *exact* over the whole match set. total_hits counts *distinct
        // URLs* and is bounded by CANDIDATE_CAP. So a facet's count is generally
        // higher than the number of grouped results it would yield — the two
        // measure different things on purpose. Serialize the aggregation once
        // and reuse it for both extractors.
        let agg_json = serde_json::to_value(&agg_results).unwrap_or_default();
        Ok(SearchResponse {
            total_hits,
            capped,
            results,
            facets: facets_from_aggregations(&agg_json),
            timeline: timeline_from_aggregations(&agg_json),
        })
    }
}

/// The indexable fields of one page. Borrowed string slices so callers can pass
/// references without cloning; unset fields default to `""` via [`Default`], so
/// adding a field here does not force every call site to change.
#[derive(Debug, Default)]
pub struct Page<'a> {
    pub url: &'a str,
    pub timestamp: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub description: &'a str,
    pub headings: &'a str,
    /// `<meta name=keywords>` content.
    pub keywords: &'a str,
    /// Page author (`<meta name=author>` / `article:author`).
    pub author: &'a str,
    /// Coarse media type: `"html"` or `"pdf"` (empty if unknown).
    pub media_type: &'a str,
    /// Page language tag, e.g. `"en-US"` (stored as its primary subtag).
    pub lang: &'a str,
    /// HTTP response status code, if known.
    pub status: Option<u16>,
    /// Year from the HTTP `Last-Modified` header, if present.
    pub modified_year: Option<u64>,
    /// The WACZ this page came from (id and display name).
    pub crawl_id: &'a str,
    pub crawl_name: &'a str,
    /// The curated collection id (slug) this page's WACZ belongs to.
    pub collection: &'a str,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc_type: String,
    /// The WACZ this result came from (id and display name).
    pub crawl_id: String,
    pub crawl_name: String,
    /// The curated collection id (slug) the WACZ belongs to, for `collection:`
    /// filtering and linking to the collection page.
    pub collection: String,
    pub url: String,
    /// Exact host of the page URL (empty for collection results).
    pub domain: String,
    pub timestamp: String,
    pub title: String,
    /// Page description (`<meta description>` / og:description), if any.
    pub description: String,
    pub snippet: String,
    /// How many captures of this URL matched (1 when there are no repeats). The
    /// result shown is the best-ranked capture; the rest are collapsed into it.
    pub capture_count: usize,
}

/// One page of search results plus the facet counts and total match count for
/// the whole query (not just this page).
#[derive(Debug, Clone)]
pub struct SearchResponse {
    /// Total number of distinct results (URLs grouped) across all pages.
    pub total_hits: usize,
    /// Whether more captures matched than were scanned for grouping, so
    /// `total_hits` is a floor and deep pages may be incomplete.
    pub capped: bool,
    /// The requested page of results.
    pub results: Vec<SearchResult>,
    /// Facet counts per dimension, in display order.
    pub facets: Vec<FacetGroup>,
    /// Result counts per crawl month, oldest first (the results timeline).
    pub timeline: Vec<TimelineBucket>,
}

/// One month's slice of the results timeline.
#[derive(Debug, Clone)]
pub struct TimelineBucket {
    /// Crawl month as `YYYYMM` (e.g. `202503`).
    pub ym: u64,
    pub count: u64,
}

/// The counts for one facet dimension (e.g. "Site"), highest count first.
#[derive(Debug, Clone)]
pub struct FacetGroup {
    /// The index field name (e.g. `domain`), used to build `field:value` refine links.
    pub field: String,
    /// Human label for the dimension (e.g. `Site`).
    pub label: String,
    pub buckets: Vec<FacetBucket>,
}

/// One value within a facet dimension and how many results carry it.
#[derive(Debug, Clone)]
pub struct FacetBucket {
    pub value: String,
    pub count: u64,
}

/// What a scoped facet overview ([`SearchIndex::facet_overview_scoped`]) is
/// restricted to.
pub enum FacetScope<'a> {
    /// A curated collection, by its id/slug (the `collection` field).
    Collection(&'a str),
    /// A single crawl/WACZ, by its id (the `crawl_id` field).
    Crawl(&'a str),
}

/// The sidebar facet dimensions, in display order: `(index field, label)`. The
/// index field name doubles as the `field:value` filter name (e.g. `domain:`),
/// so a facet value links straight to a refine query.
const FACET_DIMENSIONS: [(&str, &str); 5] = [
    (FIELD_COLLECTION, "Collection"),
    (FIELD_YEAR, "Year"),
    (FIELD_SITE, "Site"),
    (FIELD_MEDIA_TYPE, "Type"),
    (FIELD_LANG, "Language"),
];

/// Filterable `field:value` fields that aren't sidebar facets: `month` (the
/// timeline) and `domain` (exact host — the Site facet uses the registrable
/// domain instead), with labels for their active-filter chips.
const EXTRA_FILTERS: [(&str, &str); 5] = [
    (FIELD_MONTH, "Month"),
    (FIELD_DOMAIN, "Host"),
    (FIELD_STATUS, "Status"),
    (FIELD_MODIFIED, "Modified"),
    // Scopes a search to a single crawl (WACZ). `crawl` is a friendly alias for
    // the internal `crawl_id` field (see `rewrite_crawl_alias`); its value is
    // an opaque WACZ id, so the server resolves it to the crawl's name for the
    // active-filter chip.
    (FILTER_CRAWL, "Crawl"),
];

/// User-facing filter name that scopes a search to a single crawl - a friendly
/// alias for the internal [`FIELD_CRAWL_ID`] field (a crawl is one WACZ, and
/// `crawl_id` would be both internal jargon and misleading in the UI).
const FILTER_CRAWL: &str = "crawl";

/// Rewrite the `crawl:` filter alias to the real `crawl_id:` field so the
/// query parser resolves it. Token-level: only a whole `crawl:<value>` token is
/// rewritten (a bare word `crawl` in the query text is left alone). Crawl ids are
/// simple tokens, so no range/quote handling is needed.
fn rewrite_crawl_alias(query_str: &str) -> String {
    query_str
        .split_whitespace()
        .map(|tok| match tok.strip_prefix("crawl:") {
            Some(value) => format!("{FIELD_CRAWL_ID}:{value}"),
            None => tok.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `field` can be used as a `field:value` filter: a sidebar facet
/// dimension or one of the extra filter fields. The single source of truth the
/// server uses to recognize active filters, so the two can't drift.
pub fn is_filter_field(field: &str) -> bool {
    FACET_DIMENSIONS
        .iter()
        .chain(EXTRA_FILTERS.iter())
        .any(|(f, _)| *f == field)
}

/// The human label for a filterable field (for active-filter chips), sharing
/// the facet labels above so they stay in sync.
pub fn filter_label(field: &str) -> &'static str {
    FACET_DIMENSIONS
        .iter()
        .chain(EXTRA_FILTERS.iter())
        .find(|(f, _)| *f == field)
        .map(|(_, label)| *label)
        .unwrap_or("Filter")
}

/// Max buckets requested per facet dimension. A dimension with more distinct
/// values (e.g. many hosts under `domain`) is silently truncated to its top
/// `FACET_SIZE` by count — the sidebar shows only the busiest values, and the
/// terms result's `sum_other_doc_count` (the rest) is discarded.
const FACET_SIZE: u32 = 50;

/// Max month buckets for the timeline (~10 years); older months beyond this are
/// dropped from the histogram.
const TIMELINE_SIZE: u32 = 120;

/// Build the terms-aggregation request: one bucket set per facet dimension plus
/// a month bucket set for the timeline. Deserialized from JSON because
/// [`Aggregations`] is a serde type and the JSON form is far more legible than
/// the nested builder structs.
fn facet_aggregations() -> Aggregations {
    let mut req = serde_json::Map::new();
    for (field, _label) in FACET_DIMENSIONS {
        req.insert(
            field.to_string(),
            serde_json::json!({ "terms": { "field": field, "size": FACET_SIZE } }),
        );
    }
    req.insert(
        FIELD_MONTH.to_string(),
        serde_json::json!({ "terms": { "field": FIELD_MONTH, "size": TIMELINE_SIZE } }),
    );
    // The request is well-formed by construction, so this never fails.
    serde_json::from_value(serde_json::Value::Object(req))
        .expect("facet aggregation request is valid")
}

/// Extract the month buckets from the aggregation results as a timeline sorted
/// oldest-first. Terms aggregations sort by count, so we re-sort chronologically.
fn timeline_from_aggregations(value: &serde_json::Value) -> Vec<TimelineBucket> {
    let Some(buckets) = value
        .get(FIELD_MONTH)
        .and_then(|d| d.get("buckets"))
        .and_then(|b| b.as_array())
    else {
        return Vec::new();
    };
    let mut out: Vec<TimelineBucket> = buckets
        .iter()
        .filter_map(|b| {
            let count = b.get("doc_count")?.as_u64()?;
            let ym = b.get("key")?.as_f64()? as u64;
            (ym > 0).then_some(TimelineBucket { ym, count })
        })
        .collect();
    out.sort_by_key(|t| t.ym);
    out
}

/// Convert Tantivy's aggregation results into ordered [`FacetGroup`]s. Empty
/// values (e.g. the blank domain/lang of collection-level docs) are dropped.
/// `value` is the aggregation results already serialized to JSON (the terms
/// result is `{buckets:[{key,doc_count}]}`, simpler to read than the internal
/// bucket enums). Serializing once and passing it in avoids re-serializing for
/// the timeline.
fn facets_from_aggregations(value: &serde_json::Value) -> Vec<FacetGroup> {
    let mut groups = Vec::new();
    for (field, label) in FACET_DIMENSIONS {
        let Some(buckets) = value
            .get(field)
            .and_then(|d| d.get("buckets"))
            .and_then(|b| b.as_array())
        else {
            continue;
        };
        let items: Vec<FacetBucket> = buckets
            .iter()
            .filter_map(|b| {
                let count = b.get("doc_count")?.as_u64()?;
                let value = match b.get("key")? {
                    serde_json::Value::String(s) => s.clone(),
                    // Numeric keys (year) come back as floats; show them as ints.
                    serde_json::Value::Number(n) => (n.as_f64()? as i64).to_string(),
                    _ => return None,
                };
                (!value.is_empty()).then_some(FacetBucket { value, count })
            })
            .collect();
        if !items.is_empty() {
            groups.push(FacetGroup {
                field: field.to_string(),
                label: label.to_string(),
                buckets: items,
            });
        }
    }
    groups
}

/// A string field that is indexed as a single raw token (like [`STRING`]),
/// stored, **and** kept as a fast (columnar) field so it can back a terms
/// aggregation for facet counts. The `raw` tokenizer keeps the whole value as
/// one term, so a facet bucket is the exact field value (e.g. one `domain:`
/// host), not individual words.
fn facet_string() -> TextOptions {
    TextOptions::default()
        .set_stored()
        .set_fast(Some("raw"))
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::Basic),
        )
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field(FIELD_DOC_TYPE, STRING | STORED);
    builder.add_text_field(FIELD_CRAWL_ID, STRING | STORED);
    builder.add_text_field(FIELD_CRAWL_NAME, STRING | STORED);
    // Curated collection id (slug), for `collection:` filtering and faceting.
    builder.add_text_field(FIELD_COLLECTION, facet_string());
    builder.add_text_field(FIELD_URL, STRING | STORED);
    builder.add_text_field(FIELD_TS, STRING | STORED);
    builder.add_text_field(FIELD_TITLE, TEXT | STORED);
    builder.add_text_field(FIELD_BODY, TEXT | STORED);
    // Description is stored so it can be shown when a page has no body snippet.
    builder.add_text_field(FIELD_DESCRIPTION, TEXT | STORED);
    // Headings are indexed (and boosted at query time) but not stored.
    builder.add_text_field(FIELD_HEADINGS, TEXT);
    // Keywords and author are indexed (searchable, incl. `author:`) but not stored.
    builder.add_text_field(FIELD_KEYWORDS, TEXT);
    builder.add_text_field(FIELD_AUTHOR, TEXT);
    // Exact host, for `domain:host` filtering and results display.
    builder.add_text_field(FIELD_DOMAIN, facet_string());
    // Registrable domain, for the cross-subdomain `site:` filter and Site facet.
    builder.add_text_field(FIELD_SITE, facet_string());
    // Tokenized URL words; indexed for search but not stored (we keep the URL).
    builder.add_text_field(FIELD_URL_TOKENS, TEXT);
    // Numeric crawl year: indexed for `year:2021` / `year:[2020 TO 2023]`, and
    // fast so it can back the year facet.
    builder.add_u64_field(FIELD_YEAR, INDEXED | STORED | FAST);
    // Numeric crawl month `YYYYMM`: indexed for `month:202103` /
    // `month:[202101 TO 202106]`, fast so it backs the results timeline.
    builder.add_u64_field(FIELD_MONTH, INDEXED | STORED | FAST);
    // Coarse media type (`html`/`pdf`) and page language, for filtering + facets.
    builder.add_text_field(FIELD_MEDIA_TYPE, facet_string());
    builder.add_text_field(FIELD_LANG, facet_string());
    // HTTP status code, for `status:200` / `status:[200 TO 299]`.
    builder.add_u64_field(FIELD_STATUS, INDEXED | STORED);
    // Last-Modified year, for `modified:2015` / range filtering.
    builder.add_u64_field(FIELD_MODIFIED, INDEXED | STORED);
    builder.build()
}

/// The primary language subtag of an HTML `lang` attribute, lowercased
/// (`en-US` -> `en`). Empty when there's no usable value.
fn primary_lang(lang: &str) -> String {
    lang.split(['-', '_'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Below this many bytes of body text, language detection is too unreliable to
/// trust.
const MIN_DETECT_BYTES: usize = 40;

/// whatlang only needs a modest sample to identify the dominant language, so we
/// cap the input to keep detection cheap on long pages (it runs per page that
/// lacks `<html lang>`, so cost matters at scale). A couple of KB is plenty.
const DETECT_SAMPLE_BYTES: usize = 2048;

/// Detect a page's language from its body text as a fallback when `<html lang>`
/// is absent. Returns the ISO 639-1 subtag (e.g. `en`) to match the codes used
/// by declared `lang`, or `None` when the text is too short, detection is not
/// reliable, or whatlang's language has no 639-1 code. whatlang reports a single
/// dominant language, which fits our single-valued `lang` field.
fn detect_lang(body: &str) -> Option<String> {
    let body = body.trim();
    if body.len() < MIN_DETECT_BYTES {
        return None;
    }
    // Detect on a bounded prefix rather than the whole body; truncate on a char
    // boundary so we never slice mid-UTF-8.
    let sample = if body.len() > DETECT_SAMPLE_BYTES {
        let mut end = DETECT_SAMPLE_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        &body[..end]
    } else {
        body
    };
    let info = whatlang::detect(sample)?;
    if !info.is_reliable() {
        return None;
    }
    lang3_to_lang1(info.lang().code()).map(String::from)
}

/// Map an ISO 639-3 code (as whatlang reports) to its ISO 639-1 two-letter
/// subtag. The arms mirror whatlang's supported language set; a language with no
/// 639-1 code (or a new whatlang language not listed here) returns `None`, so we
/// never store a code inconsistent with the declared `lang` values (better a gap
/// than a bucket that won't unify).
fn lang3_to_lang1(code3: &str) -> Option<&'static str> {
    Some(match code3 {
        "eng" => "en",
        "spa" => "es",
        "por" => "pt",
        "fra" => "fr",
        "deu" => "de",
        "ita" => "it",
        "nld" => "nl",
        "rus" => "ru",
        "ukr" => "uk",
        "bel" => "be",
        "bul" => "bg",
        "ces" => "cs",
        "pol" => "pl",
        "hrv" => "hr",
        "srp" => "sr",
        "mkd" => "mk",
        "slv" => "sl",
        "ron" => "ro",
        "ell" => "el",
        "dan" => "da",
        "swe" => "sv",
        "nob" => "nb",
        "fin" => "fi",
        "hun" => "hu",
        "est" => "et",
        "lit" => "lt",
        "lav" => "lv",
        "tur" => "tr",
        "aze" => "az",
        "uzb" => "uz",
        "tuk" => "tk",
        "cat" => "ca",
        "epo" => "eo",
        "cmn" => "zh",
        "jpn" => "ja",
        "kor" => "ko",
        "vie" => "vi",
        "tha" => "th",
        "ind" => "id",
        "tgl" => "tl",
        "jav" => "jv",
        "mya" => "my",
        "khm" => "km",
        "ara" => "ar",
        "heb" => "he",
        "yid" => "yi",
        "pes" => "fa",
        "urd" => "ur",
        "hin" => "hi",
        "ben" => "bn",
        "guj" => "gu",
        "pan" => "pa",
        "mar" => "mr",
        "kan" => "kn",
        "tam" => "ta",
        "tel" => "te",
        "mal" => "ml",
        "ori" => "or",
        "nep" => "ne",
        "sin" => "si",
        "kat" => "ka",
        "hye" => "hy",
        "amh" => "am",
        "zul" => "zu",
        "aka" => "ak",
        _ => return None,
    })
}

/// The four-digit crawl year parsed from a 14-digit page timestamp
/// (`20210417...` -> `2021`). `None` when the timestamp is missing or does not
/// start with a plausible year.
fn year_of(timestamp: &str) -> Option<u64> {
    let year: u64 = timestamp.get(..4)?.parse().ok()?;
    (1000..=9999).contains(&year).then_some(year)
}

/// The six-digit crawl month `YYYYMM` parsed from a 14-digit page timestamp
/// (`20210417...` -> `202104`). `None` when the year or month is implausible.
fn month_of(timestamp: &str) -> Option<u64> {
    let s = timestamp.get(..6)?;
    let year: u64 = s.get(..4)?.parse().ok()?;
    let month: u64 = s.get(4..6)?.parse().ok()?;
    ((1000..=9999).contains(&year) && (1..=12).contains(&month)).then_some(year * 100 + month)
}

/// The exact host of a URL, lowercased (e.g. `https://Example.com/a` -> `example.com`).
/// Empty when the URL has no host (relative paths, `urn:`, unparseable input).
fn domain_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default()
}

/// The registrable domain (eTLD+1) of a URL, via the Public Suffix List, so a
/// whole site unifies across subdomains and multi-level suffixes are handled
/// correctly (`www.example.co.uk` -> `example.co.uk`, `a.github.io` ->
/// `a.github.io` since `github.io` is a private suffix). Empty when there's no
/// host or no registrable domain (e.g. a bare public suffix, `urn:`).
pub(crate) fn site_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .and_then(|host| psl::domain_str(&host).map(|d| d.to_string()))
        .unwrap_or_default()
}

/// A searchable text rendering of a URL: the host and path with separators
/// turned into spaces, so the default tokenizer indexes each word. For example
/// `https://github.com/DocNow/hydrator` yields `github.com DocNow hydrator`,
/// making a search for `hydrator` match the page.
fn url_search_text(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return String::new(),
    };
    let mut parts: Vec<&str> = Vec::new();
    if let Some(host) = parsed.host_str() {
        parts.push(host);
    }
    // Split the path on `/` and keep non-empty segments (drops the leading `/`).
    parts.extend(parsed.path().split('/').filter(|s| !s.is_empty()));
    parts.join(" ")
}

fn get_text(doc: &TantivyDocument, field: tantivy::schema::Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Text extracted from an HTML page for indexing.
#[derive(Debug, Default, PartialEq)]
pub struct HtmlText {
    pub title: String,
    pub body: String,
    /// `<meta name=description>` or `og:description`, if present.
    pub description: String,
    /// Concatenated `<h1>`/`<h2>` heading text.
    pub headings: String,
    /// `<meta name=keywords>` content, if present.
    pub keywords: String,
    /// Page author: `<meta name=author>`, falling back to `article:author`.
    pub author: String,
    /// The `<html lang>` attribute value, if present (e.g. `en`, `en-US`).
    pub lang: String,
    /// The page's social-preview image URL: `<meta property=og:image>`, falling
    /// back to `twitter:image`. Used as the crawl's representative thumbnail. May
    /// be relative (resolved against the page URL by the caller).
    pub og_image: String,
}

/// Extract title, body text, description, and headings from raw HTML bytes,
/// skipping script/style content.
pub fn extract_html_text(html: &[u8]) -> HtmlText {
    use scraper::{Html, Selector};

    let html_str = String::from_utf8_lossy(html);
    let doc = Html::parse_document(&html_str);

    let title_sel = Selector::parse("title").unwrap();
    let title = doc
        .select(&title_sel)
        .next()
        .map(|e| e.text().collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string();

    // Description: prefer <meta name="description">, fall back to og:description.
    let description = meta_content(&doc, "meta[name=description]")
        .or_else(|| meta_content(&doc, "meta[property=\"og:description\"]"))
        .unwrap_or_default();

    // Social-preview image: prefer og:image, fall back to twitter:image. This is
    // the crawl's representative thumbnail source.
    let og_image = meta_content(&doc, "meta[property=\"og:image\"]")
        .or_else(|| meta_content(&doc, "meta[name=\"twitter:image\"]"))
        .unwrap_or_default();

    // Keywords and author from <meta> tags (author falls back to article:author).
    let keywords = meta_content(&doc, "meta[name=keywords]").unwrap_or_default();
    let author = meta_content(&doc, "meta[name=author]")
        .or_else(|| meta_content(&doc, "meta[property=\"article:author\"]"))
        .unwrap_or_default();

    // Headings: h1 and h2 text, in document order.
    let heading_sel = Selector::parse("h1, h2").unwrap();
    let headings = doc
        .select(&heading_sel)
        .map(|e| e.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    // Language from the <html lang="..."> attribute, if any.
    let html_sel = Selector::parse("html").unwrap();
    let lang = doc
        .select(&html_sel)
        .next()
        .and_then(|e| e.value().attr("lang"))
        .unwrap_or("")
        .trim()
        .to_string();

    let body_sel = Selector::parse("body").unwrap();
    let skip_sel = Selector::parse("script, style, noscript").unwrap();
    let mut body_parts: Vec<String> = Vec::new();

    if let Some(body) = doc.select(&body_sel).next() {
        let mut skip_ids = std::collections::HashSet::new();
        for skip_el in body.select(&skip_sel) {
            skip_ids.insert(skip_el.id());
            for desc in skip_el.descendants() {
                skip_ids.insert(desc.id());
            }
        }
        for node in body.descendants() {
            if skip_ids.contains(&node.id()) {
                continue;
            }
            if let scraper::node::Node::Text(t) = node.value() {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    body_parts.push(trimmed.to_string());
                }
            }
        }
    }

    HtmlText {
        title,
        body: body_parts.join(" "),
        description,
        headings,
        keywords,
        author,
        lang,
        og_image,
    }
}

/// The trimmed `content` attribute of the first element matching `selector`,
/// dropped if empty. `selector` must be a valid CSS selector.
fn meta_content(doc: &scraper::Html, selector: &str) -> Option<String> {
    let sel = scraper::Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a page with the common fields; unset fields default to empty.
    fn page<'a>(
        url: &'a str,
        title: &'a str,
        body: &'a str,
        cid: &'a str,
        cname: &'a str,
    ) -> Page<'a> {
        Page {
            url,
            title,
            body,
            crawl_id: cid,
            crawl_name: cname,
            ..Default::default()
        }
    }

    /// A page with a given URL and timestamp; fixed title/body for date tests.
    fn page_ts<'a>(url: &'a str, ts: &'a str) -> Page<'a> {
        Page {
            url,
            timestamp: ts,
            title: "T",
            body: "shared content",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        }
    }

    #[test]
    fn extract_text_from_html() {
        let html = b"<html><head><title>Hello World</title></head><body><p>Some text</p><script>var x=1;</script></body></html>";
        let t = extract_html_text(html);
        assert_eq!(t.title, "Hello World");
        assert!(t.body.contains("Some text"), "body: {}", t.body);
        assert!(
            !t.body.contains("var x"),
            "should exclude script content: {}",
            t.body
        );
    }

    #[test]
    fn extract_description_and_headings_from_html() {
        let html = br#"<html><head><title>T</title>
            <meta name="description" content="A concise summary">
            <meta property="og:description" content="OG fallback"></head>
            <body><h1>Main Heading</h1><h2>Sub Heading</h2><p>Body.</p></body></html>"#;
        let t = extract_html_text(html);
        assert_eq!(t.description, "A concise summary");
        assert!(
            t.headings.contains("Main Heading"),
            "headings: {}",
            t.headings
        );
        assert!(
            t.headings.contains("Sub Heading"),
            "headings: {}",
            t.headings
        );
    }

    #[test]
    fn extract_keywords_and_author_from_meta() {
        let html = br#"<html><head><title>T</title>
            <meta name="keywords" content="climate, policy, marmots">
            <meta name="author" content="Ada Lovelace"></head>
            <body>x</body></html>"#;
        let t = extract_html_text(html);
        assert!(t.keywords.contains("marmots"), "keywords: {}", t.keywords);
        assert_eq!(t.author, "Ada Lovelace");
    }

    #[test]
    fn author_falls_back_to_article_author() {
        let html = br#"<html><head>
            <meta property="article:author" content="Grace Hopper"></head>
            <body>x</body></html>"#;
        let t = extract_html_text(html);
        assert_eq!(t.author, "Grace Hopper");
    }

    #[test]
    fn optimize_merges_segments_and_preserves_search() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.disable_auto_merge(); // each commit -> its own segment
        for i in 0..4 {
            let url = format!("https://ex.com/{i}");
            let cid = format!("c{i}");
            idx.index_page(&Page {
                url: &url,
                title: "Snowfall",
                body: "snow in the mountains",
                crawl_id: &cid,
                crawl_name: "C",
                ..Default::default()
            })
            .unwrap();
            idx.commit().unwrap();
        }
        let before = idx.segment_count().unwrap();
        assert!(
            before >= 4,
            "expected a fragmented index, got {before} segments"
        );

        let (b, after) = idx.optimize(1, None).unwrap();
        assert_eq!(b, before);
        assert_eq!(after, 1, "should compact to a single segment");
        assert_eq!(idx.segment_count().unwrap(), 1);
        // All four docs survive the merge and are still searchable.
        assert_eq!(idx.search("snow", 10).unwrap().len(), 4);
    }

    #[test]
    fn optimize_respects_target_and_is_a_noop_when_already_small() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.disable_auto_merge();
        for i in 0..5 {
            let cid = format!("c{i}");
            idx.index_page(&Page {
                url: "https://ex.com/x",
                title: "T",
                body: "body",
                crawl_id: &cid,
                crawl_name: "C",
                ..Default::default()
            })
            .unwrap();
            idx.commit().unwrap();
        }
        assert!(idx.segment_count().unwrap() >= 5);
        // Compact toward 2, then optimizing again is a no-op (already ≤ target).
        let (_, after) = idx.optimize(2, None).unwrap();
        assert_eq!(after, 2);
        let (before2, after2) = idx.optimize(2, None).unwrap();
        assert_eq!(
            (before2, after2),
            (2, 2),
            "already-compact index is untouched"
        );
    }

    #[test]
    fn keywords_and_author_are_searchable() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/a",
            title: "Plain",
            body: "ordinary body",
            keywords: "marmots rodentia",
            author: "Ada Lovelace",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        assert_eq!(
            idx.search("rodentia", 10).unwrap().len(),
            1,
            "keywords searchable"
        );
        assert_eq!(
            idx.search("Lovelace", 10).unwrap().len(),
            1,
            "author searchable by bare word"
        );
        assert_eq!(
            idx.search("author:Lovelace", 10).unwrap().len(),
            1,
            "author: field query"
        );
    }

    #[test]
    fn description_falls_back_to_og_description() {
        let html = br#"<html><head><meta property="og:description" content="OG only"></head>
            <body>x</body></html>"#;
        let t = extract_html_text(html);
        assert_eq!(t.description, "OG only");
    }

    #[test]
    fn extract_lang_from_html_element() {
        let html = br#"<html lang="en-US"><head><title>T</title></head><body>x</body></html>"#;
        let t = extract_html_text(html);
        assert_eq!(t.lang, "en-US");
    }

    #[test]
    fn detect_lang_fills_in_when_html_lang_absent() {
        // Enough English text to detect reliably.
        let en = "The quick brown fox jumps over the lazy dog near the riverbank \
                  while the sun sets slowly behind the distant hills this evening.";
        assert_eq!(detect_lang(en).as_deref(), Some("en"));
        // French.
        let fr = "Le vif renard brun saute par-dessus le chien paresseux tandis que \
                  le soleil se couche lentement derrière les collines lointaines ce soir.";
        assert_eq!(detect_lang(fr).as_deref(), Some("fr"));
        // Too short to trust.
        assert_eq!(detect_lang("hi there"), None);
    }

    #[test]
    fn detect_lang_caps_long_multibyte_body_without_panicking() {
        // A body well over DETECT_SAMPLE_BYTES with multibyte chars (é, à) right
        // around the cut point must not panic on a mid-UTF-8 slice, and still
        // detect the dominant language.
        let sentence =
            "Le renard brun rapide sauté par-dessus le chien paresseux à côté de la rivière. ";
        let long = sentence.repeat(80); // > 2 KB, many multi-byte chars
        assert!(long.len() > DETECT_SAMPLE_BYTES);
        assert_eq!(detect_lang(&long).as_deref(), Some("fr"));
    }

    #[test]
    fn indexing_detects_lang_only_as_fallback() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        let french = "Le renard brun rapide saute par-dessus le chien paresseux et \
                      le soleil se couche derrière les collines lointaines ce soir la.";
        // Declared lang wins even when the body is another language.
        idx.index_page(&Page {
            url: "https://ex.com/declared",
            title: "T",
            body: french,
            lang: "en-GB",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        // No declared lang -> detected from body.
        idx.index_page(&Page {
            url: "https://ex.com/detected",
            title: "T",
            body: french,
            lang: "",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        let en = idx.search("lang:en", 10).unwrap();
        assert_eq!(en.len(), 1);
        assert_eq!(
            en[0].url, "https://ex.com/declared",
            "declared en-GB wins over the body"
        );
        let fr = idx.search("lang:fr", 10).unwrap();
        assert_eq!(fr.len(), 1);
        assert_eq!(
            fr[0].url, "https://ex.com/detected",
            "empty lang detected as fr from body"
        );
    }

    #[test]
    fn primary_lang_takes_the_first_subtag() {
        assert_eq!(primary_lang("en-US"), "en");
        assert_eq!(primary_lang("EN"), "en");
        assert_eq!(primary_lang("pt_BR"), "pt");
        assert_eq!(primary_lang(""), "");
    }

    #[test]
    fn type_and_lang_filters() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/page",
            title: "Doc",
            body: "shared",
            media_type: "html",
            lang: "en-US",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/file.pdf",
            title: "Report",
            body: "shared",
            media_type: "pdf",
            lang: "",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        let r = idx.search("type:pdf", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/file.pdf");

        let r = idx.search("shared type:html", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/page");

        // lang is stored as its primary subtag, so `lang:en` matches `en-US`.
        let r = idx.search("lang:en", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/page");
    }

    #[test]
    fn roundtrip_index_and_search() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.index_page(&page(
            "http://example.com/",
            "Example Page",
            "This is some interesting content about Rust programming",
            "abc12345",
            "My Collection",
        ))
        .unwrap();
        idx.commit().unwrap();

        let results = idx.search("Rust programming", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "http://example.com/");
        assert_eq!(results[0].crawl_id, "abc12345");
        assert_eq!(results[0].doc_type, "page");
    }

    #[test]
    fn description_and_headings_are_searchable() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/a",
            title: "Plain Title",
            body: "ordinary body",
            description: "a treatise on marmots",
            headings: "Notable Rodents",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        assert_eq!(
            idx.search("marmots", 10).unwrap().len(),
            1,
            "description searchable"
        );
        assert_eq!(
            idx.search("rodents", 10).unwrap().len(),
            1,
            "headings searchable"
        );
        assert_eq!(
            idx.search("a", 10).unwrap()[0].description,
            "a treatise on marmots"
        );
    }

    #[test]
    fn collection_document_is_searchable() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.index_collection(
            "abc12345",
            "My Archive",
            "my-archive",
            "A collection about digital preservation and web archiving",
        )
        .unwrap();
        idx.commit().unwrap();

        let results = idx.search("digital preservation", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_type, "collection");
        assert_eq!(results[0].crawl_id, "abc12345");
    }

    #[test]
    fn search_returns_snippet() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();

        idx.index_page(&page(
            "http://example.com/",
            "Example",
            "The quick brown fox jumps over the lazy dog near the riverbank",
            "abc12345",
            "Test",
        ))
        .unwrap();
        idx.commit().unwrap();

        let results = idx.search("fox", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].snippet.contains("fox"),
            "snippet should contain matched term: {}",
            results[0].snippet
        );
    }

    #[test]
    fn open_errors_on_schema_mismatch() {
        use tantivy::schema::{Schema, TEXT};
        let tmp = TempDir::new().unwrap();
        // Create an index with a different (older-style) schema.
        let mut b = Schema::builder();
        b.add_text_field("body", TEXT);
        tantivy::Index::create_in_dir(tmp.path(), b.build()).unwrap();

        // Opening with the current schema must fail cleanly (not panic), and
        // the message should point the user at reindex.
        let result = SearchIndex::open(tmp.path());
        assert!(result.is_err(), "opening a mismatched schema should error");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("reindex"),
            "error should suggest reindex: {msg}"
        );
    }

    #[test]
    fn search_no_results() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.commit().unwrap();

        let results = idx.search("nonexistent", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn status_and_modified_year_filters() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/ok",
            title: "OK",
            body: "shared",
            status: Some(200),
            modified_year: Some(2015),
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.index_page(&Page {
            url: "https://ex.com/gone",
            title: "Gone",
            body: "shared",
            status: Some(404),
            modified_year: Some(2020),
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        let r = idx.search("status:200", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/ok");

        let r = idx.search("shared modified:2020", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/gone");

        // Status ranges work (u64 field): 4xx only.
        let r = idx.search("status:[400 TO 499]", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/gone");
    }

    #[test]
    fn site_of_extracts_registrable_domain() {
        // Subdomains unify to the registrable domain.
        assert_eq!(site_of("https://www.example.com/a"), "example.com");
        assert_eq!(site_of("https://blog.example.com/"), "example.com");
        // Multi-level public suffix handled via the PSL.
        assert_eq!(site_of("https://www.bbc.co.uk/news"), "bbc.co.uk");
        // Private suffixes (github.io) are effective TLDs, so subdomains stay distinct.
        assert_eq!(site_of("https://alice.github.io/"), "alice.github.io");
        // No host / unparseable input yields an empty site.
        assert_eq!(site_of("urn:text:foo"), "");
    }

    #[test]
    fn site_filter_spans_subdomains_while_domain_is_exact() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page(
            "https://www.example.com/a",
            "A",
            "shared",
            "c1",
            "C1",
        ))
        .unwrap();
        idx.index_page(&page(
            "https://blog.example.com/b",
            "B",
            "shared",
            "c1",
            "C1",
        ))
        .unwrap();
        idx.index_page(&page("https://other.org/c", "C", "shared", "c1", "C1"))
            .unwrap();
        idx.commit().unwrap();

        // site: matches the whole registrable domain across subdomains.
        let r = idx.search("site:example.com", 10).unwrap();
        assert_eq!(r.len(), 2, "site: spans www. and blog.");

        // domain: stays exact-host.
        let r = idx.search("domain:www.example.com", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://www.example.com/a");
    }

    #[test]
    fn domain_of_extracts_lowercased_host() {
        assert_eq!(domain_of("https://Example.COM/a/b?x=1"), "example.com");
        assert_eq!(domain_of("http://sub.example.org/"), "sub.example.org");
        // No host / unparseable input yields an empty domain.
        assert_eq!(domain_of("urn:text:foo"), "");
        assert_eq!(domain_of("not a url"), "");
    }

    #[test]
    fn url_search_text_yields_host_and_path_words() {
        let text = url_search_text("https://github.com/DocNow/hydrator");
        assert_eq!(text, "github.com DocNow hydrator");
    }

    #[test]
    fn search_matches_words_from_the_url() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page(
            "https://github.com/DocNow/hydrator",
            "Some Title",
            "unrelated body text",
            "abc12345",
            "Test",
        ))
        .unwrap();
        idx.commit().unwrap();

        // "hydrator" appears only in the URL, but url words are searchable.
        let results = idx.search("hydrator", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://github.com/DocNow/hydrator");
        assert_eq!(results[0].domain, "github.com");
    }

    #[test]
    fn domain_filter_restricts_to_exact_host() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page(
            "https://example.com/one",
            "One",
            "shared word",
            "c1",
            "C1",
        ))
        .unwrap();
        idx.index_page(&page(
            "https://other.org/two",
            "Two",
            "shared word",
            "c1",
            "C1",
        ))
        .unwrap();
        idx.commit().unwrap();

        let results = idx.search("domain:example.com", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/one");

        // Combined with a term, still AND-scoped to that domain.
        let results = idx.search("domain:example.com shared", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/one");
    }

    #[test]
    fn collection_filter_restricts_results() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&Page {
            url: "https://a.com/1",
            title: "A",
            body: "shared",
            crawl_id: "w1",
            crawl_name: "W1",
            collection: "demo",
            ..Default::default()
        })
        .unwrap();
        idx.index_page(&Page {
            url: "https://b.com/1",
            title: "B",
            body: "shared",
            crawl_id: "w2",
            crawl_name: "W2",
            collection: "other",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        let r = idx.search("collection:demo", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].collection, "demo");

        // Combined with a term, still scoped to the collection.
        let r = idx.search("collection:demo shared", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://a.com/1");
    }

    #[test]
    fn multi_word_queries_require_all_terms() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page("https://ex.com/a", "A", "alpha beta", "c1", "C1"))
            .unwrap();
        idx.index_page(&page("https://ex.com/b", "B", "alpha gamma", "c1", "C1"))
            .unwrap();
        idx.commit().unwrap();

        // AND-by-default: only the page containing BOTH words matches.
        let results = idx.search("alpha beta", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://ex.com/a");

        // A term present in neither-together combination returns nothing.
        let results = idx.search("beta gamma", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn title_matches_rank_above_body_matches() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        // The term is in page 1's title and page 2's body only.
        idx.index_page(&page(
            "https://ex.com/title-hit",
            "kangaroo",
            "filler text",
            "c1",
            "C1",
        ))
        .unwrap();
        idx.index_page(&page(
            "https://ex.com/body-hit",
            "filler",
            "kangaroo text",
            "c1",
            "C1",
        ))
        .unwrap();
        idx.commit().unwrap();

        let results = idx.search("kangaroo", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].url, "https://ex.com/title-hit",
            "title match should rank first"
        );
    }

    #[test]
    fn year_of_parses_leading_year() {
        assert_eq!(year_of("20210417120000"), Some(2021));
        assert_eq!(year_of("2021"), Some(2021));
        assert_eq!(year_of(""), None);
        assert_eq!(year_of("notadate"), None);
        assert_eq!(year_of("0099010100"), None); // implausible year
    }

    #[test]
    fn year_filter_exact_and_range() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page_ts("https://ex.com/2019", "20190101000000"))
            .unwrap();
        idx.index_page(&page_ts("https://ex.com/2021", "20210101000000"))
            .unwrap();
        idx.index_page(&page_ts("https://ex.com/2023", "20230101000000"))
            .unwrap();
        idx.commit().unwrap();

        // Exact year.
        let r = idx.search("year:2021", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/2021");

        // Inclusive range.
        let r = idx.search("year:[2020 TO 2023]", 10).unwrap();
        assert_eq!(r.len(), 2, "2021 and 2023 fall in range");

        // Combined with a term (AND-scoped).
        let r = idx.search("shared year:2019", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://ex.com/2019");
    }

    /// Look up one facet dimension's buckets as a value->count map.
    fn facet_map(resp: &SearchResponse, field: &str) -> std::collections::HashMap<String, u64> {
        resp.facets
            .iter()
            .find(|g| g.field == field)
            .map(|g| {
                g.buckets
                    .iter()
                    .map(|b| (b.value.clone(), b.count))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn facet_counts_reflect_the_query() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        // Three "shared" pages: two on example.com (2021 html), one on other.org
        // (2023 pdf), spread across two collections.
        idx.index_page(&Page {
            url: "https://example.com/a",
            title: "A",
            body: "shared",
            timestamp: "20210101000000",
            media_type: "html",
            lang: "en-US",
            collection: "demo",
            crawl_id: "w1",
            crawl_name: "W1",
            ..Default::default()
        })
        .unwrap();
        idx.index_page(&Page {
            url: "https://example.com/b",
            title: "B",
            body: "shared",
            timestamp: "20210601000000",
            media_type: "html",
            lang: "en",
            collection: "demo",
            crawl_id: "w1",
            crawl_name: "W1",
            ..Default::default()
        })
        .unwrap();
        idx.index_page(&Page {
            url: "https://other.org/c",
            title: "C",
            body: "shared",
            timestamp: "20230101000000",
            media_type: "pdf",
            lang: "fr",
            collection: "news",
            crawl_id: "w2",
            crawl_name: "W2",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        let resp = idx.search_faceted("shared", 10, 0).unwrap();
        assert_eq!(resp.total_hits, 3);

        // The Site facet is the registrable domain (FIELD_SITE), not the host.
        let sites = facet_map(&resp, FIELD_SITE);
        assert_eq!(sites.get("example.com"), Some(&2));
        assert_eq!(sites.get("other.org"), Some(&1));

        let years = facet_map(&resp, FIELD_YEAR);
        assert_eq!(years.get("2021"), Some(&2));
        assert_eq!(years.get("2023"), Some(&1));

        let types = facet_map(&resp, FIELD_MEDIA_TYPE);
        assert_eq!(types.get("html"), Some(&2));
        assert_eq!(types.get("pdf"), Some(&1));

        let colls = facet_map(&resp, FIELD_COLLECTION);
        assert_eq!(colls.get("demo"), Some(&2));
        assert_eq!(colls.get("news"), Some(&1));

        // Narrowing the query narrows the facet counts to the matching subset.
        let resp = idx
            .search_faceted("shared domain:example.com", 10, 0)
            .unwrap();
        assert_eq!(resp.total_hits, 2);
        assert_eq!(facet_map(&resp, FIELD_YEAR).get("2021"), Some(&2));
        assert!(
            !facet_map(&resp, FIELD_YEAR).contains_key("2023"),
            "2023 filtered out"
        );
    }

    #[test]
    fn repeat_captures_of_a_url_are_grouped() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        // The same URL captured three times (different crawls), plus a distinct URL.
        for ts in ["20210101000000", "20220101000000", "20230101000000"] {
            idx.index_page(&Page {
                url: "https://ex.com/a",
                title: "A",
                body: "shared",
                timestamp: ts,
                crawl_id: "c1",
                crawl_name: "C1",
                ..Default::default()
            })
            .unwrap();
        }
        idx.index_page(&Page {
            url: "https://ex.com/b",
            title: "B",
            body: "shared",
            crawl_id: "c1",
            crawl_name: "C1",
            ..Default::default()
        })
        .unwrap();
        idx.commit().unwrap();

        let resp = idx.search_faceted("shared", 10, 0).unwrap();
        // Two distinct results, not four captures.
        assert_eq!(resp.total_hits, 2);
        assert!(!resp.capped);
        let a = resp
            .results
            .iter()
            .find(|r| r.url == "https://ex.com/a")
            .unwrap();
        assert_eq!(
            a.capture_count, 3,
            "three captures of /a collapse into one result"
        );
        let b = resp
            .results
            .iter()
            .find(|r| r.url == "https://ex.com/b")
            .unwrap();
        assert_eq!(b.capture_count, 1);
    }

    #[test]
    fn month_of_parses_year_and_month() {
        assert_eq!(month_of("20210417120000"), Some(202104));
        assert_eq!(month_of("202412"), Some(202412));
        assert_eq!(month_of("20211320"), None, "month 13 is invalid");
        assert_eq!(month_of("2021"), None, "too short for a month");
        assert_eq!(month_of(""), None);
    }

    #[test]
    fn timeline_is_chronological_and_counts_months() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        // Two captures in 2021-03, one in 2021-01, one in 2023-07 (distinct URLs
        // so month counts aren't affected by URL grouping).
        for (url, ts) in [
            ("https://ex.com/1", "20210301000000"),
            ("https://ex.com/2", "20210315000000"),
            ("https://ex.com/3", "20210101000000"),
            ("https://ex.com/4", "20230701000000"),
        ] {
            idx.index_page(&page_ts(url, ts)).unwrap();
        }
        idx.commit().unwrap();

        let resp = idx.search_faceted("shared", 10, 0).unwrap();
        let tl: Vec<(u64, u64)> = resp.timeline.iter().map(|t| (t.ym, t.count)).collect();
        // Oldest first, one bucket per distinct month, with correct counts.
        assert_eq!(tl, vec![(202101, 1), (202103, 2), (202307, 1)]);

        // Filtering by month narrows to that month only.
        let resp = idx.search_faceted("shared month:202103", 10, 0).unwrap();
        assert_eq!(resp.total_hits, 2);
    }

    #[test]
    fn pagination_offsets_and_reports_total() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        for i in 0..25 {
            let url = format!("https://ex.com/{i:02}");
            idx.index_page(&Page {
                url: &url,
                title: "T",
                body: "shared",
                crawl_id: "c1",
                crawl_name: "C1",
                ..Default::default()
            })
            .unwrap();
        }
        idx.commit().unwrap();

        let p1 = idx.search_faceted("shared", 20, 0).unwrap();
        assert_eq!(
            p1.total_hits, 25,
            "total counts all matches, not just the page"
        );
        assert_eq!(p1.results.len(), 20, "first page is full");

        let p2 = idx.search_faceted("shared", 20, 20).unwrap();
        assert_eq!(p2.total_hits, 25);
        assert_eq!(p2.results.len(), 5, "second page holds the remainder");

        // No overlap between the two pages.
        let urls1: std::collections::HashSet<_> = p1.results.iter().map(|r| &r.url).collect();
        assert!(
            p2.results.iter().all(|r| !urls1.contains(&r.url)),
            "pages must not overlap"
        );
    }

    #[test]
    fn malformed_query_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let mut idx = SearchIndex::open(tmp.path()).unwrap();
        idx.index_page(&page("https://ex.com/a", "A", "hello world", "c1", "C1"))
            .unwrap();
        idx.commit().unwrap();

        // An unbalanced quote would be a parse error; lenient parsing must not
        // propagate it as an Err (the search box should never 500).
        assert!(idx.search("\"hello", 10).is_ok());
        assert!(idx.search("title:", 10).is_ok());
    }
}
