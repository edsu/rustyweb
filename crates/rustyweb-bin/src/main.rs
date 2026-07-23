use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::fmt::{
    format::{Format, FormatEvent, Writer},
    FmtContext,
};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

// Wraps the default log format so that WARN and ERROR lines are highlighted in
// bold color across the entire line, not just the level label.
#[derive(Default)]
struct ColorLineFormat(Format);

impl<S, N> FormatEvent<S, N> for ColorLineFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let level = *event.metadata().level();
        let color = if writer.has_ansi_escapes() {
            match level {
                tracing::Level::ERROR => Some("\x1b[1;31m"),
                tracing::Level::WARN => Some("\x1b[1;33m"),
                _ => None,
            }
        } else {
            None
        };

        let Some(start) = color else {
            return self.0.format_event(ctx, writer, event);
        };

        let mut buf = String::new();
        self.0.format_event(ctx, Writer::new(&mut buf), event)?;
        let line = buf.trim_end_matches('\n');
        writeln!(writer, "{start}{line}\x1b[0m")
    }
}

#[derive(Parser)]
#[command(name = "rustyweb", about = "Web archive player", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index one or more WACZ files (kept in <home>/archive) or http(s) URLs.
    Index {
        /// WACZ files or http(s) URLs to index. A local WACZ must live under
        /// <home>/archive; for several, glob it: `index archive/*.wacz`. Provide
        /// at least one here or via --from-file.
        paths: Vec<String>,

        /// Read more WACZ files/URLs from a text file, one per line (blank lines
        /// and lines starting with `#` are ignored). Use `-` to read from stdin.
        #[arg(short = 'f', long = "from-file", value_name = "FILE")]
        from_file: Option<String>,

        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,

        /// WACZ display name (defaults to the WACZ title or filename).
        #[arg(long)]
        name: Option<String>,

        /// Add the WACZ(s) to this collection (created if new). Without it, each
        /// WACZ is its own collection.
        #[arg(long)]
        collection: Option<String>,

        /// Download a remote WACZ into <home>/archive and index it as a local
        /// file (durable copy, whole-file fixity, offline replay) instead of
        /// streaming it in place. No effect on local sources.
        #[arg(long)]
        download: bool,

        /// Number of records to fetch concurrently while CDX-guided (streaming)
        /// indexing. Default: 4 for remote URLs (gentle on the host; raise it,
        /// e.g. 16, for object stores like S3), CPU count for local files.
        /// Capped at 64 per host as a proactive politeness ceiling.
        #[arg(long, value_name = "N")]
        concurrency: Option<usize>,

        /// Verbose logging (debug level). Replaces the progress bar with detailed
        /// per-record logs.
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Start the replay web server.
    Serve {
        /// Address to listen on.
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        bind: String,

        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,
    },
    /// Rebuild the search index from collections.json (re-fetches remote sources).
    Reindex {
        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,

        /// Number of records to fetch concurrently while re-streaming each
        /// source. Default: 4 for remote URLs (gentle on the host; raise it,
        /// e.g. 16, for object stores like S3), CPU count for local files.
        /// Capped at 64 per host as a proactive politeness ceiling.
        #[arg(long, value_name = "N")]
        concurrency: Option<usize>,

        /// Verbose logging (debug level). Replaces the progress bar with detailed
        /// per-record logs.
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Search indexed WACZ files for CDX records matching a URL.
    SearchUrl {
        /// URL to search for (exact match against archived URLs).
        url: String,

        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,
    },
    /// Verify the fixity of indexed WACZ files by re-hashing each one.
    Verify {
        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,
    },
    /// Manage collections (curated groups of WACZs).
    Collection {
        #[command(subcommand)]
        action: CollectionCmd,
    },
    /// Manage an individual crawl (WACZ).
    Crawl {
        #[command(subcommand)]
        action: CrawlCmd,
    },
    /// Import content into rustyweb from an external web-archiving service.
    Import {
        #[command(subcommand)]
        action: ImportCmd,
    },
}

/// Sources that content can be imported from. Each is its own command (their
/// flags differ), grouped under `import`.
#[derive(Subcommand)]
enum ImportCmd {
    /// Import WACZ files from a Browsertrix instance (Webrecorder's hosted
    /// crawler). Authenticates with credentials from the environment:
    /// BROWSERTRIX_USER + BROWSERTRIX_PASSWORD, or a BROWSERTRIX_TOKEN — kept out
    /// of the command line so secrets don't appear in the process list.
    Browsertrix {
        /// Browsertrix host (use this for a self-hosted instance).
        #[arg(long, default_value = rustyweb_lib::browsertrix::DEFAULT_HOST)]
        host: String,

        /// Organization to import from (its slug or id). Defaults to your only
        /// org; required when the account has more than one.
        #[arg(long)]
        org: Option<String>,

        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,

        /// Import only this Browsertrix collection (its id, slug, or name).
        /// Default: the whole org.
        #[arg(long, conflicts_with = "crawl")]
        collection: Option<String>,

        /// Import only this archived item (a crawl or upload) by id. Implies
        /// including it even if it hasn't been QA'd.
        #[arg(long)]
        crawl: Option<String>,

        /// Group the imported crawls into this rustyweb collection (created if
        /// new). Without it, each crawl is its own collection.
        #[arg(long)]
        into: Option<String>,

        /// Also import crawls that haven't been QA'd. By default only crawls a
        /// reviewer has QA'd in Browsertrix are imported.
        #[arg(long)]
        include_unreviewed: bool,

        /// Only import crawls whose QA review rating is at least N (1–5). Implies
        /// reviewed-only.
        #[arg(
            long,
            value_name = "N",
            value_parser = clap::value_parser!(u8).range(1..=5),
            conflicts_with = "include_unreviewed"
        )]
        min_review: Option<u8>,

        /// Import at most N items (0 = all).
        #[arg(long, value_name = "N", default_value_t = 0)]
        limit: usize,

        /// List what would be imported, without downloading or indexing.
        #[arg(long)]
        dry_run: bool,

        /// Stream-index without downloading (index-only footprint). The WACZ
        /// isn't copied locally; replay re-resolves a fresh presigned URL from
        /// Browsertrix on demand, so `serve` needs the same credentials.
        /// Default: download a durable local copy.
        #[arg(long)]
        stream: bool,

        /// Re-download and re-index items even if they were already imported
        /// (otherwise already-synced items are skipped).
        #[arg(long)]
        force: bool,

        /// Verbose logging (debug level). Replaces the progress bar with detailed
        /// per-record logs.
        #[arg(short = 'v', long)]
        verbose: bool,
    },
}

#[derive(Subcommand)]
enum CrawlCmd {
    /// Set curator-controlled properties of a crawl.
    Set {
        /// Crawl id (the 8-char id shown on the crawl page / in `collection list`).
        id: String,

        /// Pin a representative image for this crawl from a local image file
        /// (PNG/JPEG/WebP/GIF). Overrides the auto-selected thumbnail and is kept
        /// across reindexing.
        #[arg(long, value_name = "FILE")]
        image: Option<PathBuf>,

        /// A curator note (Markdown) for this crawl — e.g. to document absences
        /// or context. Written to a committable `crawls/<id>.md`.
        #[arg(long, conflicts_with = "note_file")]
        note: Option<String>,

        /// Read the crawl note (Markdown) from a file (use `-` for stdin).
        #[arg(long, value_name = "FILE")]
        note_file: Option<PathBuf>,

        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,
    },
}

#[derive(Subcommand)]
// `Set` carries the finding-aid fields, so it's much larger than `List`; a clap
// command enum is constructed once, so the size difference doesn't matter.
#[allow(clippy::large_enum_variant)]
enum CollectionCmd {
    /// Create or update a collection's finding-aid metadata (created if it
    /// doesn't exist). Structured fields go to YAML front-matter; the narrative
    /// prose is the Markdown body of a committable `collections/<slug>.md` — set
    /// it with --narrative[-file], or just hand-edit that file.
    Set {
        /// Collection name (its id is a slug of this).
        name: String,

        /// A short abstract / caption for the collection (EAD <abstract>).
        #[arg(long)]
        description: Option<String>,

        /// Repository / owner running this rustyweb instance (EAD <repository>).
        #[arg(long)]
        curator: Option<String>,

        /// Collecting org/person responsible for the records (DACS Name of
        /// Creator, EAD <origination>) — distinct from --curator.
        #[arg(long)]
        creator: Option<String>,

        /// Curatorial coverage-date statement (EAD <unitdate>), distinct from
        /// the auto-derived capture range.
        #[arg(long)]
        dates: Option<String>,

        /// Conditions governing access and use / license (EAD <userestrict>).
        #[arg(long)]
        rights: Option<String>,

        /// A topical subject / access point (repeat for several).
        #[arg(long = "subject", value_name = "SUBJECT")]
        subjects: Vec<String>,

        /// Pin a representative image for the whole collection from a local
        /// image file (PNG/JPEG/WebP/GIF), committed under the collection.
        #[arg(long, value_name = "FILE")]
        thumbnail: Option<PathBuf>,

        /// The Scope & Content / provenance narrative (Markdown) inline.
        #[arg(long, conflicts_with = "narrative_file")]
        narrative: Option<String>,

        /// Read the narrative (Markdown) from a file (use `-` for stdin).
        #[arg(long, value_name = "FILE")]
        narrative_file: Option<PathBuf>,

        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,
    },
    /// List collections and their WACZ counts.
    List {
        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,
    },
}

/// Read a whole text argument from a file, or from stdin when the path is `-`
/// (for `--narrative-file`/`--note-file`).
fn read_text_arg(path: &Path) -> Result<String> {
    if path.as_os_str() == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading text from stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading text from {}", path.display()))
    }
}

/// Read a newline-delimited list of WACZ files/URLs from a file, or from stdin
/// when `src` is `-`. Blank lines and `#` comment lines are skipped; each
/// remaining line is trimmed.
fn read_source_list(src: &str) -> Result<Vec<String>> {
    let text = if src == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading WACZ list from stdin")?;
        buf
    } else {
        std::fs::read_to_string(src).with_context(|| format!("reading WACZ list from {src}"))?
    };
    Ok(parse_source_lines(&text))
}

/// Parse a newline-delimited source list: trim each line, drop blank lines and
/// `#` comments.
fn parse_source_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect()
}

/// An indexing progress indicator (indicatif). A fresh bar is created per WACZ so
/// it's only on screen while that WACZ is being worked on — the setup phase shows
/// an indeterminate spinner (labelled with the current activity, e.g.
/// "downloading" / "reading index"), then it flips to a determinate bar once the
/// record total is known. Between WACZs there's no bar, so library log lines
/// print cleanly instead of colliding with it.
struct BarProgress {
    // Interior mutability: the `IndexProgress` methods take `&self`, but the bar
    // is (re)created per WACZ. `None` between WACZs.
    inner: std::sync::Mutex<Option<Active>>,
}

/// The live bar plus the short WACZ label (kept so `phase`/`set_total` can
/// recompose the message).
struct Active {
    pb: indicatif::ProgressBar,
    label: String,
}

impl BarProgress {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(None),
        }
    }

    /// Clear any active bar (safety net for the error path, where `finish` on the
    /// library side may not run).
    fn clear(&self) {
        if let Some(a) = self.inner.lock().unwrap().take() {
            a.pb.finish_and_clear();
        }
    }
}

/// The indeterminate style, for phases with no per-record total (setup, and the
/// merge/commit tail after records are read).
fn spinner_style() -> indicatif::ProgressStyle {
    indicatif::ProgressStyle::with_template("{spinner:.green} {msg} ({elapsed})").unwrap()
}

/// The determinate style, once we know the record total. The unit is **records**
/// (CDX entries fetched: HTML/PDF responses + `urn:text` rendered text), not
/// pages — several records merge into one page, so this is always >= the final
/// page count reported by the "indexed N pages" summary. {per_sec} + {eta} answer
/// "how long will this take?" — indicatif derives both from a moving window of
/// recent progress, so they track the current streaming rate rather than a naive
/// lifetime average.
fn bar_style() -> indicatif::ProgressStyle {
    indicatif::ProgressStyle::with_template(
        "{spinner:.green} {msg} [{bar:30.cyan/blue}] {pos}/{len} records \
         ({per_sec}, {elapsed} elapsed, eta {eta})",
    )
    .unwrap()
    .progress_chars("=>-")
}

/// The trailing path/URL segment, for a compact bar label.
fn short_label(label: &str) -> String {
    label
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(label)
        .to_string()
}

impl rustyweb_lib::index::IndexProgress for BarProgress {
    fn begin(&self, label: &str) {
        // Indeterminate spinner: the record total isn't known until the CDX is
        // read. steady_tick animates it during the blocking network setup. The
        // verb is filled in by `phase`; until then just show the label.
        let mut guard = self.inner.lock().unwrap();
        if let Some(old) = guard.take() {
            old.pb.finish_and_clear();
        }
        let pb = indicatif::ProgressBar::new_spinner();
        pb.set_style(spinner_style());
        let label = short_label(label);
        pb.set_message(label.clone());
        pb.enable_steady_tick(std::time::Duration::from_millis(120));
        *guard = Some(Active { pb, label });
    }
    fn phase(&self, phase: &str) {
        if let Some(a) = &*self.inner.lock().unwrap() {
            // Reset to the spinner style: `phase` marks an indeterminate phase,
            // including the transition *back* from the determinate records bar
            // (merge/commit tail) so it doesn't sit at 100% with a decaying ETA.
            a.pb.set_style(spinner_style());
            a.pb.set_message(format!("{} — {phase}…", a.label));
        }
    }
    fn set_total(&self, total: u64) {
        if let Some(a) = &*self.inner.lock().unwrap() {
            a.pb.set_length(total);
            a.pb.set_position(0);
            a.pb.set_style(bar_style());
            a.pb.set_message(a.label.clone());
        }
    }
    fn set_records(&self, done: u64) {
        if let Some(a) = &*self.inner.lock().unwrap() {
            a.pb.set_position(done);
        }
    }
    fn wacz_indexed(&self, label: &str, pages: u64) {
        // `println` on the active bar writes a line that persists after the bar is
        // cleared, so the run leaves a record of what it did.
        let line = format!(
            "✓ indexed {pages} page{} from {}",
            if pages == 1 { "" } else { "s" },
            short_label(label)
        );
        match &*self.inner.lock().unwrap() {
            Some(a) => a.pb.println(line),
            None => eprintln!("{line}"),
        }
    }
    fn finish(&self) {
        if let Some(a) = self.inner.lock().unwrap().take() {
            a.pb.finish_and_clear();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Progress bar vs. logs (for `index`):
    //  - `-v`         -> debug logs, no bar.
    //  - interactive  -> the bar carries progress, so hush INFO (it would collide
    //                    with and duplicate the bar); keep WARN/ERROR.
    //  - non-TTY      -> no bar (piping/CI), so keep INFO so logs aren't lost.
    // RUST_LOG overrides the level in all cases.
    let verbose = matches!(
        &cli.command,
        Commands::Index { verbose: true, .. }
            | Commands::Reindex { verbose: true, .. }
            | Commands::Import {
                action: ImportCmd::Browsertrix { verbose: true, .. }
            }
    );
    // `index`, `reindex`, and `browsertrix` (which indexes what it downloads) all
    // stream records and show the progress bar.
    let shows_progress = matches!(
        &cli.command,
        Commands::Index { .. } | Commands::Reindex { .. } | Commands::Import { .. }
    );
    let show_bar = shows_progress && !verbose && std::io::stderr().is_terminal();
    let default_level = if verbose {
        "debug"
    } else if show_bar {
        "warn"
    } else {
        "info"
    };
    // Default filter: our level, but silence third-party parsers that log noisy,
    // non-fatal diagnostics through the tracing-log bridge during indexing:
    //  - pdf-extract/lopdf: per-glyph warnings ("unknown glyph name ...").
    //  - html5ever: HTML quirks like "foster parenting not implemented" while
    //    parsing archived pages for text/images — harmless, and it recovers.
    // We handle these outcomes ourselves, so the log lines are pure noise (and
    // stomp the progress bar). RUST_LOG overrides the whole thing to see them.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(default_level)
            .add_directive("pdf_extract=off".parse().unwrap())
            .add_directive("lopdf=off".parse().unwrap())
            .add_directive("html5ever=off".parse().unwrap())
    });
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(true)
        // Logs go to stderr so stdout can carry data (and be silenced during
        // indexing to hide third-party PDF extraction noise).
        .with_writer(std::io::stderr)
        .event_format(ColorLineFormat::default())
        .init();

    // pdf-extract / lopdf can *panic* on malformed PDFs. `extract_pdf_text`
    // already catches these via `catch_unwind` (a bad PDF is skipped, indexing
    // continues), but the default panic hook still prints the alarming, progress-
    // bar-stomping "thread 'main' panicked at pdf-extract..." message. Suppress
    // panics originating in those crates; delegate everything else to the default
    // hook. Set once here (thread-safe) so it also holds for parallel indexing.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let from_pdf = info.location().is_some_and(|loc| {
            let f = loc.file();
            f.contains("pdf-extract") || f.contains("lopdf")
        });
        if !from_pdf {
            default_hook(info);
        }
    }));

    match cli.command {
        // `verbose` is read up front (to set the log level / bar); ignore here.
        Commands::Index {
            paths,
            from_file,
            home,
            name,
            collection,
            download,
            concurrency,
            verbose: _,
        } => {
            // Sources come from the positional args plus, optionally, a
            // newline-delimited list from a file or stdin.
            let mut locations = paths;
            if let Some(src) = &from_file {
                locations.extend(read_source_list(src)?);
            }

            // `index` no longer auto-scans <home>/archive; a bare invocation is
            // almost always a mistake, so guide the user to the things they
            // probably meant instead.
            if locations.is_empty() {
                eprintln!(
                    "index needs at least one WACZ file (kept in <home>/archive) or an\n\
                     http(s) URL. For example:\n\
                     \n\
                     \x20 rustyweb index archive/site.wacz          index a local WACZ (must be in archive/)\n\
                     \x20 rustyweb index archive/*.wacz             index several at once\n\
                     \x20 rustyweb index https://ex.org/b.wacz      index a remote WACZ\n\
                     \x20 rustyweb index --from-file urls.txt       index a list from a file\n\
                     \x20 cat urls.txt | rustyweb index -f -        index a list from stdin\n\
                     \n\
                     To rebuild the existing index from the manifest (including\n\
                     remote sources), use: rustyweb reindex"
                );
                std::process::exit(2);
            }

            // Every crawl belongs to a collection. Rather than invent a
            // singleton per WACZ, we ask the curator to say what this is part of
            // — the deliberate "stop and think" moment (index a glob into one
            // collection to decide it once).
            let Some(collection) = collection.as_deref() else {
                eprintln!(
                    "index needs --collection <NAME>: every crawl belongs to a curated\n\
                     collection. For example:\n\
                     \n\
                     \x20 rustyweb index archive/*.wacz --collection \"Ukraine Cultural Heritage\"\n\
                     \n\
                     Pick a name that says what these crawls are a part of and why you're\n\
                     keeping them; you can describe it further with: rustyweb collection set"
                );
                std::process::exit(2);
            };

            // A progress bar makes a slow streaming index (each remote page
            // record is a separate HTTP range request) visible. Shown only on an
            // interactive stderr and not under -v (see `show_bar` above).
            let bar = show_bar.then(BarProgress::new);
            let progress = bar
                .as_ref()
                .map(|b| b as &dyn rustyweb_lib::index::IndexProgress);

            let total = locations.len();
            for (i, location) in locations.iter().enumerate() {
                // No bar is active between WACZs (each begins/finishes its own),
                // so this logs cleanly without colliding with the bar.
                tracing::info!(
                    source = %location,
                    progress = format!("{}/{}", i + 1, total),
                    "indexing"
                );
                // pdf-extract prints font/glyph diagnostics straight to stdout
                // (e.g. "unknown glyph name '.notdef' ...") that can't be
                // filtered by log level. Silence stdout while indexing runs;
                // our logs are on stderr and are unaffected.
                let quiet = gag::Gag::stdout().ok();
                let result = rustyweb_lib::index::index_location(
                    location,
                    &home,
                    name.as_deref(),
                    collection,
                    download,
                    concurrency,
                    progress,
                );
                drop(quiet);
                if let Err(e) = result {
                    // Clear any spinner/bar left up by an aborted WACZ before the
                    // error propagates.
                    if let Some(b) = &bar {
                        b.clear();
                    }
                    return Err(e);
                }
            }
            tracing::info!("indexing complete");
        }

        Commands::Serve { bind, home } => {
            let ctrl_c = async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to listen for ctrl-c");
                tracing::info!("received shutdown signal");
            };

            #[cfg(unix)]
            let terminate = async {
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler")
                    .recv()
                    .await;
                tracing::info!("received SIGTERM");
            };

            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();

            // Resolver so Browsertrix-sourced crawls can be replayed (fresh
            // presigned URLs on demand). Logs in lazily with the env credentials,
            // so a server with no Browsertrix sources never needs them.
            let resolver: std::sync::Arc<dyn rustyweb_lib::index::SourceResolver> =
                std::sync::Arc::new(BrowsertrixResolver::new());

            tokio::select! {
                result = rustyweb_lib::server::serve_with_resolver(&bind, &home, Some(resolver)) => {
                    result?;
                }
                _ = ctrl_c => {}
                _ = terminate => {}
            }
        }

        Commands::Reindex {
            home,
            concurrency,
            verbose: _,
        } => {
            // A full reindex re-streams every source, so the progress bar is even
            // more welcome here than for `index`. Same gating (interactive, not -v).
            let bar = show_bar.then(BarProgress::new);
            let progress = bar
                .as_ref()
                .map(|b| b as &dyn rustyweb_lib::index::IndexProgress);
            // Like `index`, silence stdout to hide third-party PDF extraction
            // noise; our logs are on stderr.
            let quiet = gag::Gag::stdout().ok();
            // Resolver for any Browsertrix sources in the manifest (logs in
            // lazily, so a manifest without them needs no credentials).
            let resolver = BrowsertrixResolver::new();
            let result =
                rustyweb_lib::index::reindex(&home, concurrency, Some(&resolver), progress);
            drop(quiet);
            if result.is_err() {
                // Clear any spinner/bar left up before the error propagates.
                if let Some(b) = &bar {
                    b.clear();
                }
            }
            result?;
        }

        Commands::SearchUrl { url, home } => {
            run_search_url(&url, &home)?;
        }

        Commands::Verify { home } => {
            let all_ok = run_verify(&home)?;
            if !all_ok {
                std::process::exit(1);
            }
        }

        Commands::Collection { action } => match action {
            CollectionCmd::Set {
                name,
                description,
                curator,
                creator,
                dates,
                rights,
                subjects,
                narrative,
                narrative_file,
                thumbnail,
                home,
            } => {
                let narrative = match narrative_file {
                    Some(path) => Some(read_text_arg(&path)?),
                    None => narrative,
                };
                let fields = rustyweb_lib::collections::CollectionFields {
                    description,
                    curator,
                    creator,
                    dates,
                    rights,
                    // An empty repeated --subject means "not provided" (leave as
                    // is); pass Some only when the curator gave at least one.
                    subjects: (!subjects.is_empty()).then_some(subjects),
                    narrative,
                };
                let id = rustyweb_lib::index::set_collection(&home, &name, &fields)?;
                if let Some(file) = &thumbnail {
                    rustyweb_lib::index::set_collection_thumbnail(&home, &name, file)?;
                }
                let readme = home.join("collections").join(&id).join("README.md");
                println!("collection \"{name}\" ({id}) updated");
                if fields.narrative.is_none() {
                    println!("  add a scope note by editing {}", readme.display());
                }
            }
            CollectionCmd::List { home } => {
                run_collection_list(&home)?;
            }
        },

        Commands::Crawl { action } => match action {
            CrawlCmd::Set {
                id,
                image,
                note,
                note_file,
                home,
            } => {
                let mut did = false;
                if let Some(file) = &image {
                    rustyweb_lib::index::set_crawl_thumbnail(&home, &id, file)?;
                    println!("pinned thumbnail for crawl {id} from {}", file.display());
                    did = true;
                }
                let note = match note_file {
                    Some(path) => Some(read_text_arg(&path)?),
                    None => note,
                };
                if let Some(note) = note {
                    rustyweb_lib::index::set_crawl_note(&home, &id, &note)?;
                    println!("note updated for crawl {id}");
                    did = true;
                }
                if !did {
                    eprintln!("nothing to set — pass --image <FILE> or --note <TEXT>");
                    std::process::exit(2);
                }
            }
        },

        Commands::Import { action } => match action {
            ImportCmd::Browsertrix {
                host,
                org,
                home,
                collection,
                crawl,
                into,
                include_unreviewed,
                min_review,
                limit,
                dry_run,
                stream,
                force,
                verbose: _,
            } => {
                let bar = show_bar.then(BarProgress::new);
                let progress = bar
                    .as_ref()
                    .map(|b| b as &dyn rustyweb_lib::index::IndexProgress);
                let opts = ImportOpts {
                    collection: collection.as_deref(),
                    crawl: crawl.as_deref(),
                    into: into.as_deref(),
                    include_unreviewed,
                    min_review,
                    limit,
                    dry_run,
                    stream,
                    force,
                };
                let result = run_browsertrix(&host, org.as_deref(), &home, &opts, progress);
                if result.is_err() {
                    if let Some(b) = &bar {
                        b.clear();
                    }
                }
                result?;
            }
        },
    }

    Ok(())
}

/// Print each collection with its WACZ count and description.
fn run_collection_list(home: &std::path::Path) -> Result<()> {
    use rustyweb_lib::collections::Manifest;

    let index_dir = rustyweb_lib::index::index_dir(home);
    let manifest = Manifest::open(&index_dir)?;
    if manifest.collections.is_empty() {
        println!("No collections registered in {}", index_dir.display());
        return Ok(());
    }
    for c in &manifest.collections {
        let count = manifest.members_of(&c.id).count();
        let desc = c.description.as_deref().unwrap_or("");
        println!("{:<24} {:>3} WACZ  {}", c.name, count, desc);
    }
    Ok(())
}

/// Re-hash every WACZ registered in the manifest and compare against the SHA-256
/// recorded at index time. Reports each collection as OK / MODIFIED / MISSING
/// and returns `false` if any collection failed its fixity check.
fn run_verify(home: &std::path::Path) -> Result<bool> {
    use rustyweb_lib::collections::{file_sha256, Manifest, Source};

    let index_dir = rustyweb_lib::index::index_dir(home);
    let manifest = Manifest::open(&index_dir)?;
    if manifest.waczs.is_empty() {
        println!("No collections registered in {}", index_dir.display());
        return Ok(true);
    }

    let mut ok = 0usize;
    let mut missing = 0usize;
    let mut modified = 0usize;
    let mut remote = 0usize;

    for col in &manifest.waczs {
        let loc = col.source.location();
        // Remote sources would have to be re-downloaded to re-hash; skip them.
        if matches!(col.source, Source::Url(_)) {
            println!("REMOTE    {} ({loc}) - skipped (not re-fetched)", col.name);
            remote += 1;
            continue;
        }
        let path = col.source.resolve(home).unwrap();
        if !path.exists() {
            println!("MISSING   {} ({loc})", col.name);
            missing += 1;
            continue;
        }
        match file_sha256(&path) {
            Ok(hash) if hash == col.sha256 => {
                println!("OK        {} ({loc})", col.name);
                ok += 1;
            }
            Ok(hash) => {
                println!(
                    "MODIFIED  {} ({loc}) - expected {}… got {}…",
                    col.name,
                    short_hash(&col.sha256),
                    short_hash(&hash),
                );
                modified += 1;
            }
            Err(e) => {
                println!("ERROR     {} ({loc}) - {e}", col.name);
                missing += 1;
            }
        }
    }

    println!("\n{ok} OK, {missing} missing, {modified} modified, {remote} remote (skipped)");
    Ok(missing == 0 && modified == 0)
}

/// First 8 characters of a hex hash for compact display.
fn short_hash(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

fn run_search_url(url: &str, home: &std::path::Path) -> Result<()> {
    use rustyweb_lib::collections::{Manifest, Source};
    use rustyweb_lib::wacz::search_cdx;

    let index_dir = rustyweb_lib::index::index_dir(home);
    let manifest = Manifest::open(&index_dir)?;
    if manifest.waczs.is_empty() {
        println!("No collections registered in {}", index_dir.display());
        return Ok(());
    }

    let mut found_any = false;
    for col in &manifest.waczs {
        // This debugging aid reads the CDX from the local WACZ; skip remote
        // sources rather than re-downloading them.
        if matches!(col.source, Source::Url(_)) {
            eprintln!(
                "skipping remote collection {} ({})",
                col.name,
                col.source.location()
            );
            continue;
        }
        let path = col.source.resolve(home).unwrap();
        if !path.exists() {
            eprintln!("warning: {} not found at {}", col.name, path.display());
            continue;
        }
        let records = search_cdx(&path, url)?;
        if records.is_empty() {
            continue;
        }
        found_any = true;
        println!("Collection: {} ({})", col.name, path.display());
        for r in &records {
            println!("  url:       {}", r.url);
            println!("  timestamp: {}", r.timestamp);
            println!("  status:    {}", r.status);
            println!("  mime:      {}", r.mime);
            println!("  filename:  {}", r.filename);
            println!("  offset:    {}", r.offset);
            println!("  length:    {}", r.length);
            println!();
        }
    }

    if !found_any {
        println!("Not found: {url}");
    }

    Ok(())
}

/// Authenticate to Browsertrix, resolve the org, and import its archived items:
/// download each item's WACZ into `<home>/archive` and index it as a durable
/// local (File) source. `--dry-run` lists what would be imported instead.
///
/// Each WACZ is downloaded via its presigned `replay.json` URL (a flat, single
/// WACZ) rather than the combined per-item `/download` (which can be a nested
/// multi-WACZ — see rustyweb-15z.8), so it indexes cleanly today. Downloading
/// (rather than streaming in place) keeps replay durable: the presigned URLs
/// expire in ~48 h, so they're an ingest artifact, not a replay source.
/// Options for a Browsertrix import (from the `browsertrix` subcommand).
struct ImportOpts<'a> {
    /// Import only this Browsertrix collection (its id); `None` = whole org.
    collection: Option<&'a str>,
    /// Import only this archived item by id; `None` = all selected.
    crawl: Option<&'a str>,
    /// Group imports into this rustyweb collection; `None` = one per crawl.
    into: Option<&'a str>,
    /// Import crawls that haven't been QA'd too (default: reviewed-only).
    include_unreviewed: bool,
    /// Minimum QA review rating to import (implies reviewed-only).
    min_review: Option<u8>,
    limit: usize,
    dry_run: bool,
    /// Stream-index without downloading (index-only footprint); replay
    /// re-resolves a fresh presigned URL on demand.
    stream: bool,
    force: bool,
}

/// Authenticate to Browsertrix, resolve the org, and import its archived items:
/// download each item's WACZ into `<home>/archive` and index it as a durable
/// local (File) source. `--dry-run` lists what would be imported instead.
///
/// By default only crawls a reviewer has QA'd in Browsertrix are imported (see
/// [`passes_review`]); `--include-unreviewed` and `--min-review` adjust that,
/// and naming a single `--crawl` always includes it.
///
/// Each WACZ is downloaded via its presigned `replay.json` URL (a flat, single
/// WACZ) rather than the combined per-item `/download` (which can be a nested
/// multi-WACZ — see rustyweb-15z.8), so it indexes cleanly today. Downloading
/// (rather than streaming in place) keeps replay durable: the presigned URLs
/// expire in ~48 h, so they're an ingest artifact, not a replay source.
fn run_browsertrix(
    host: &str,
    org: Option<&str>,
    home: &std::path::Path,
    opts: &ImportOpts,
    progress: Option<&dyn rustyweb_lib::index::IndexProgress>,
) -> Result<()> {
    use rustyweb_lib::browsertrix::ItemQuery;

    let client = connect(host)?;
    let host = client.host().to_string();
    let orgs = client.orgs().context("listing organizations")?;
    let org = resolve_org(&orgs, org)?;
    tracing::info!(org = %org.name, id = %org.id, "using organization");

    // Resolve --collection (id, slug, or name) to the Browsertrix collection the
    // API's filter needs (by UUID).
    let selected_collection = match opts.collection {
        Some(sel) => {
            let colls = client.collections(&org.id).context("listing collections")?;
            Some(resolve_collection(&colls, sel)?)
        }
        None => None,
    };

    // Where imports land as a rustyweb collection (every crawl belongs to one):
    // an explicit --into wins; otherwise importing a Browsertrix collection
    // yields a rustyweb collection of the same name; otherwise (a whole-org or
    // single-crawl import) fall back to the org name — a meaningful collecting
    // body — rather than scattering singletons.
    let into: &str = opts
        .into
        .or(selected_collection.as_ref().map(|c| c.name.as_str()))
        .unwrap_or(&org.name);

    // Selection is server-side (a collection or a single item); the review
    // filter is applied client-side so we can report what was skipped.
    let query = ItemQuery {
        collection_id: selected_collection.as_ref().map(|c| c.id.as_str()),
        item_id: opts.crawl,
    };
    let items = client
        .items(&org.id, &query)
        .context("listing archived items")?;

    // A single named --crawl is an explicit choice, so import it regardless of
    // its review status.
    let include_unreviewed = opts.include_unreviewed || opts.crawl.is_some();
    let reviewed: Vec<&rustyweb_lib::browsertrix::Item> = items
        .iter()
        .filter(|it| passes_review(it, include_unreviewed, opts.min_review))
        .collect();
    // User-facing status goes to stderr with eprintln (not tracing), so it's
    // visible even in bar mode, where the log level is raised to `warn`.
    let skipped_review = items.len() - reviewed.len();
    if skipped_review > 0 {
        eprintln!(
            "skipping {skipped_review} crawl(s) that aren't QA'd \
             (use --include-unreviewed to import them)"
        );
    }

    let count = if opts.limit == 0 {
        reviewed.len()
    } else {
        opts.limit.min(reviewed.len())
    };
    let selected = &reviewed[..count];

    if opts.dry_run {
        println!(
            "{} item(s) to import from \"{}\" ({}):",
            selected.len(),
            org.name,
            org.slug
        );
        for &item in selected {
            println!(
                "  {:<6} {:>10}  {:>4}  {}  {}",
                if item.is_upload() { "upload" } else { "crawl" },
                rustyweb_lib::server::human_size(item.file_size),
                item.review_status
                    .map_or("—".to_string(), |s| format!("QA{s}")),
                item.id,
                item.name
            );
        }
        if count < reviewed.len() {
            println!(
                "  … {} more (raise --limit to import them)",
                reviewed.len() - count
            );
        }
        return Ok(());
    }

    if selected.is_empty() {
        eprintln!("nothing to import.");
        return Ok(());
    }
    eprintln!(
        "importing {} crawl(s) into {}…",
        selected.len(),
        home.display()
    );

    let archive = rustyweb_lib::index::archive_dir(home);
    std::fs::create_dir_all(&archive)
        .with_context(|| format!("creating archive dir {}", archive.display()))?;

    // Incremental sync: skip resources already recorded in the manifest (by
    // host + item + content hash) so re-running against a large account is
    // cheap. `--force` re-imports regardless. `mut` so within-run duplicates are
    // skipped too.
    let mut seen = load_imported(home);

    // Resolves Browsertrix sources for `--stream` (re-resolves a fresh presigned
    // URL at index time). Lazy: unused when downloading.
    let resolver = BrowsertrixResolver::new();

    let mut imported = 0usize;
    let mut skipped = 0usize;
    for &item in selected {
        let resources = client
            .resources(&org.id, item)
            .with_context(|| format!("resolving resources for item {}", item.id))?;
        if resources.is_empty() {
            tracing::warn!(item = %item.name, id = %item.id, "no WACZ resources; skipping");
            continue;
        }
        // Each item's WACZ(s) land in their own subdirectory under archive/, so
        // two items whose resources share a filename can't clobber each other
        // (their crawl id is derived from the path, so a shared path would also
        // merge their manifest entries). The subdir is still under archive/, so
        // it satisfies the "local WACZ lives in archive/" rule.
        let item_dir = archive.join(safe_component(&item.id));
        std::fs::create_dir_all(&item_dir)
            .with_context(|| format!("creating {}", item_dir.display()))?;

        for (i, res) in resources.iter().enumerate() {
            let key = (host.clone(), item.id.clone(), res.hash.clone());
            if !opts.force && seen.contains(&key) {
                tracing::debug!(item = %item.name, "already imported; skipping");
                skipped += 1;
                continue;
            }

            let size = if res.size > 0 {
                format!(" ({})", rustyweb_lib::server::human_size(res.size))
            } else {
                String::new()
            };

            // pdf-extract writes glyph diagnostics to stdout during indexing;
            // silence it (as `index`/`reindex` do) so it doesn't stomp the bar.
            let crawl_id = if opts.stream {
                // Index-only: record a Browsertrix source (stable identity) and
                // stream it in place; replay/reindex re-resolve a fresh URL.
                let source = rustyweb_lib::collections::Source::Browsertrix {
                    host: host.clone(),
                    org: org.id.clone(),
                    item: item.id.clone(),
                    resource: res.name.clone(),
                };
                eprintln!("↻ streaming {}{size}", res.name);
                let quiet = gag::Gag::stdout().ok();
                let indexed = rustyweb_lib::index::index_location_with_resolver(
                    &source.location(),
                    home,
                    Some(&item.name),
                    into,
                    false,
                    None,
                    Some(&resolver),
                    progress,
                );
                drop(quiet);
                indexed.with_context(|| format!("streaming {}", res.name))?;
                rustyweb_lib::collections::wacz_id(&source)
            } else {
                // Download a durable local copy and index it as a File source.
                let filename = safe_wacz_filename(&res.name, &format!("resource-{i}"));
                let dest = item_dir.join(&filename);
                eprintln!("↓ downloading {filename}{size}");
                download_wacz(&res.path, &dest)?;
                let quiet = gag::Gag::stdout().ok();
                let indexed = rustyweb_lib::index::index_location(
                    &dest.to_string_lossy(),
                    home,
                    Some(&item.name),
                    into,
                    false,
                    None,
                    progress,
                );
                drop(quiet);
                indexed.with_context(|| format!("indexing {}", dest.display()))?;
                let abs = dest.canonicalize().unwrap_or(dest.clone());
                rustyweb_lib::collections::wacz_id(&rustyweb_lib::collections::Source::for_file(
                    &abs, home,
                ))
            };

            // Record provenance so a later run can skip this resource, and carry
            // the QA review rating (DACS Appraisal signal) onto the crawl.
            rustyweb_lib::index::set_browsertrix_provenance_by_id(
                home,
                &crawl_id,
                &host,
                &item.id,
                &res.hash,
                item.review_status,
            )
            .context("recording provenance")?;
            seen.insert(key);
            imported += 1;
        }
    }
    eprintln!(
        "done: imported {imported} crawl(s){}",
        if skipped > 0 {
            format!(", skipped {skipped} already up to date")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Resolve a `--collection` value (a Browsertrix collection id, slug, or name)
/// to the collection — we need its UUID for the API's item filter and its name
/// to default the rustyweb target collection. Mirrors [`resolve_org`].
fn resolve_collection(
    colls: &[rustyweb_lib::browsertrix::Collection],
    want: &str,
) -> Result<rustyweb_lib::browsertrix::Collection> {
    colls
        .iter()
        .find(|c| c.id == want || c.slug == want || c.name == want)
        .cloned()
        .ok_or_else(|| {
            let slugs = colls
                .iter()
                .map(|c| c.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!("no collection matching \"{want}\"; available: {slugs}")
        })
}

/// Whether an item passes the QA-review filter. Reviewed-only is the default;
/// `include_unreviewed` lets everything through, and `min_review` (which implies
/// reviewed-only) requires at least that rating.
fn passes_review(
    item: &rustyweb_lib::browsertrix::Item,
    include_unreviewed: bool,
    min_review: Option<u8>,
) -> bool {
    if include_unreviewed {
        return true;
    }
    match item.review_status {
        Some(s) => min_review.is_none_or(|m| s >= m),
        None => false,
    }
}

/// Load the set of already-imported Browsertrix resources — `(host, item_id,
/// resource_hash)` — from the manifest, for incremental-sync skip checks.
/// Returns empty when there's no manifest yet.
fn load_imported(home: &std::path::Path) -> std::collections::HashSet<(String, String, String)> {
    use rustyweb_lib::collections::Manifest;

    let index_dir = rustyweb_lib::index::index_dir(home);
    Manifest::open(&index_dir)
        .map(|m| {
            m.waczs
                .iter()
                .filter_map(|w| w.browsertrix.as_ref())
                .map(|b| (b.host.clone(), b.item_id.clone(), b.resource_hash.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Download a WACZ to `dest` via a temp `.part` file renamed on success, so an
/// interrupted download never leaves a truncated file that looks complete.
/// Returns the number of bytes written.
fn download_wacz(url: &str, dest: &std::path::Path) -> Result<u64> {
    use std::io::{Read, Write};

    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);

    let mut reader =
        rustyweb_lib::http_range::get_reader(url).with_context(|| format!("fetching {url}"))?;
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {url}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", tmp.display()))?;
        written += n as u64;
    }
    file.sync_all().ok();
    drop(file);
    std::fs::rename(&tmp, dest).with_context(|| format!("finalizing {}", dest.display()))?;
    Ok(written)
}

/// A filesystem-safe `.wacz` filename derived from a resource name (falling back
/// to the item id). Non-`[A-Za-z0-9._-]` characters — including any path
/// separators — become `_`, so the result stays a single component inside the
/// archive folder.
fn safe_wacz_filename(name: &str, fallback: &str) -> String {
    let base = if name.trim().is_empty() {
        fallback
    } else {
        name.trim()
    };
    let mut out = safe_component(base);
    if !out.to_ascii_lowercase().ends_with(".wacz") {
        out.push_str(".wacz");
    }
    out
}

/// Build a Browsertrix client from environment credentials, so secrets never
/// appear in argv. A `BROWSERTRIX_TOKEN` (an existing JWT) short-circuits login;
/// otherwise `BROWSERTRIX_USER` + `BROWSERTRIX_PASSWORD` are used.
fn connect(host: &str) -> Result<rustyweb_lib::browsertrix::Client> {
    use rustyweb_lib::browsertrix::Client;

    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(token) = env("BROWSERTRIX_TOKEN") {
        return Ok(Client::with_token(host, &token));
    }
    match (env("BROWSERTRIX_USER"), env("BROWSERTRIX_PASSWORD")) {
        (Some(user), Some(password)) => {
            Client::login(host, &user, &password).context("logging in to Browsertrix")
        }
        _ => anyhow::bail!(
            "set BROWSERTRIX_USER and BROWSERTRIX_PASSWORD (or BROWSERTRIX_TOKEN) in the \
             environment to authenticate — they're read from the environment so credentials \
             stay out of the command line"
        ),
    }
}

/// Resolves stored Browsertrix sources (`Source::Browsertrix`) to fresh presigned
/// URLs, for both indexing (`import --stream`, `reindex`) and replay (`serve`).
/// Logs in lazily per host with the same env credentials as [`connect`] and
/// caches the client, so no login happens unless a Browsertrix source is
/// actually encountered.
struct BrowsertrixResolver {
    clients: std::sync::Mutex<std::collections::HashMap<String, rustyweb_lib::browsertrix::Client>>,
}

impl BrowsertrixResolver {
    fn new() -> Self {
        Self {
            clients: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Resolve using the cached client for `host` (logging in if there's none),
    /// find the named resource, and return its fresh presigned URL.
    fn try_resolve(&self, host: &str, org: &str, item: &str, resource: &str) -> Result<String> {
        let mut clients = self.clients.lock().unwrap();
        if !clients.contains_key(host) {
            clients.insert(host.to_string(), connect(host)?);
        }
        let resources = clients.get(host).unwrap().item_resources(org, item)?;
        resources
            .into_iter()
            .find(|r| r.name == resource)
            .map(|r| r.path)
            .ok_or_else(|| {
                anyhow::anyhow!("WACZ resource {resource:?} not found in Browsertrix item {item}")
            })
    }
}

impl rustyweb_lib::index::SourceResolver for BrowsertrixResolver {
    fn resolve(&self, source: &rustyweb_lib::collections::Source) -> Result<String> {
        let rustyweb_lib::collections::Source::Browsertrix {
            host,
            org,
            item,
            resource,
        } = source
        else {
            anyhow::bail!("resolver only handles Browsertrix sources");
        };
        match self.try_resolve(host, org, item, resource) {
            Ok(url) => Ok(url),
            // The cached login may have gone stale (Browsertrix JWTs expire well
            // under a long-lived server's lifetime). Drop it and retry once with
            // a fresh login before giving up. Any non-auth failure just fails
            // again and surfaces here.
            Err(first) => {
                tracing::debug!(%host, error = %first, "Browsertrix resolve failed; re-authenticating and retrying");
                self.clients.lock().unwrap().remove(host);
                self.try_resolve(host, org, item, resource)
                    .with_context(|| format!("resolving Browsertrix item {item}"))
            }
        }
    }
}

/// Choose the org to import from. With `want` (a slug or id), match it exactly;
/// otherwise use the sole org, erroring if there are none or several.
fn resolve_org(
    orgs: &[rustyweb_lib::browsertrix::Org],
    want: Option<&str>,
) -> Result<rustyweb_lib::browsertrix::Org> {
    let slugs = || {
        orgs.iter()
            .map(|o| o.slug.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match want {
        Some(w) => orgs
            .iter()
            .find(|o| o.slug == w || o.id == w)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no org matching \"{w}\"; available: {}", slugs())),
        None => match orgs {
            [] => anyhow::bail!("no organizations are available for this account"),
            [only] => Ok(only.clone()),
            _ => anyhow::bail!("several orgs are available; pass --org <slug>: {}", slugs()),
        },
    }
}

/// A filesystem-safe single path component: any character outside
/// `[A-Za-z0-9._-]` (including path separators) becomes `_`.
fn safe_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_source_lines;
    use super::{passes_review, resolve_collection, resolve_org, safe_wacz_filename};
    use rustyweb_lib::browsertrix::{Collection, Item, Org};

    fn item(review: Option<u8>) -> Item {
        Item {
            id: "x".into(),
            name: "n".into(),
            item_type: "crawl".into(),
            file_size: 0,
            review_status: review,
        }
    }

    fn org(id: &str, slug: &str) -> Org {
        Org {
            id: id.to_string(),
            slug: slug.to_string(),
            name: format!("{slug} name"),
        }
    }

    #[test]
    fn resolve_org_uses_the_sole_org_by_default() {
        let orgs = vec![org("o1", "gov")];
        assert_eq!(resolve_org(&orgs, None).unwrap().id, "o1");
    }

    #[test]
    fn resolve_org_requires_a_choice_when_several_exist() {
        let orgs = vec![org("o1", "gov"), org("o2", "edu")];
        let err = resolve_org(&orgs, None).err().unwrap().to_string();
        assert!(err.contains("several"), "{err}");
        assert!(err.contains("gov") && err.contains("edu"), "{err}");
    }

    #[test]
    fn resolve_org_matches_slug_or_id() {
        let orgs = vec![org("o1", "gov"), org("o2", "edu")];
        assert_eq!(resolve_org(&orgs, Some("edu")).unwrap().id, "o2");
        assert_eq!(resolve_org(&orgs, Some("o1")).unwrap().slug, "gov");
    }

    #[test]
    fn resolve_org_errors_on_unknown_org() {
        let orgs = vec![org("o1", "gov")];
        let err = resolve_org(&orgs, Some("nope")).err().unwrap().to_string();
        assert!(err.contains("nope") && err.contains("gov"), "{err}");
    }

    #[test]
    fn resolve_collection_matches_id_slug_or_name() {
        let colls = vec![
            Collection {
                id: "uuid-1".into(),
                slug: "gov-arc".into(),
                name: "US Gov".into(),
            },
            Collection {
                id: "uuid-2".into(),
                slug: "edu".into(),
                name: "Universities".into(),
            },
        ];
        assert_eq!(resolve_collection(&colls, "gov-arc").unwrap().id, "uuid-1");
        assert_eq!(resolve_collection(&colls, "uuid-2").unwrap().id, "uuid-2");
        assert_eq!(
            resolve_collection(&colls, "Universities").unwrap().id,
            "uuid-2"
        );

        let err = resolve_collection(&colls, "nope")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("nope") && err.contains("gov-arc"), "{err}");
    }

    #[test]
    fn passes_review_defaults_to_reviewed_only() {
        // Default: reviewed passes, unreviewed is skipped.
        assert!(passes_review(&item(Some(3)), false, None));
        assert!(!passes_review(&item(None), false, None));
    }

    #[test]
    fn passes_review_include_unreviewed_lets_everything_through() {
        assert!(passes_review(&item(None), true, None));
        assert!(passes_review(&item(Some(1)), true, None));
    }

    #[test]
    fn passes_review_min_review_is_a_threshold() {
        assert!(passes_review(&item(Some(5)), false, Some(4)));
        assert!(passes_review(&item(Some(4)), false, Some(4)));
        assert!(!passes_review(&item(Some(3)), false, Some(4)));
        assert!(!passes_review(&item(None), false, Some(4)));
    }

    #[test]
    fn safe_wacz_filename_sanitizes_and_ensures_extension() {
        assert_eq!(safe_wacz_filename("my crawl.wacz", "id1"), "my_crawl.wacz");
        // Path separators and other unsafe chars become '_'; extension appended.
        assert_eq!(
            safe_wacz_filename("../etc/passwd", "id1"),
            ".._etc_passwd.wacz"
        );
        assert_eq!(safe_wacz_filename("plain", "id1"), "plain.wacz");
        // Empty name falls back to the item id.
        assert_eq!(safe_wacz_filename("   ", "abc123"), "abc123.wacz");
        // Already-correct names are preserved (case-insensitive extension check).
        assert_eq!(safe_wacz_filename("a-b_c.WACZ", "id1"), "a-b_c.WACZ");
    }

    #[test]
    fn parse_source_lines_skips_blanks_and_comments_and_trims() {
        let text = "\
# a list of WACZs
archive/one.wacz

  archive/two.wacz  
https://ex.org/three.wacz
   # indented comment
";
        assert_eq!(
            parse_source_lines(text),
            vec![
                "archive/one.wacz".to_string(),
                "archive/two.wacz".to_string(),
                "https://ex.org/three.wacz".to_string(),
            ]
        );
    }

    #[test]
    fn parse_source_lines_empty_input_yields_nothing() {
        assert!(parse_source_lines("").is_empty());
        assert!(parse_source_lines("# only a comment\n\n").is_empty());
    }
}
