use std::io::IsTerminal;
use std::path::PathBuf;

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
        /// indexing. Default: 16 for remote URLs (hides network latency), CPU
        /// count for local files (extraction is CPU-bound).
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
}

#[derive(Subcommand)]
enum CollectionCmd {
    /// Create or update a collection's metadata (created if it doesn't exist).
    Set {
        /// Collection name (its id is a slug of this).
        name: String,

        /// A description of the collection.
        #[arg(long)]
        description: Option<String>,

        /// Curator / owner.
        #[arg(long)]
        curator: Option<String>,

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
    let verbose = matches!(&cli.command, Commands::Index { verbose: true, .. });
    let is_index = matches!(&cli.command, Commands::Index { .. });
    let show_bar = is_index && !verbose && std::io::stderr().is_terminal();
    let default_level = if verbose {
        "debug"
    } else if show_bar {
        "warn"
    } else {
        "info"
    };
    // Default filter: our level, but silence pdf-extract/lopdf, which log noisy
    // per-glyph warnings ("unknown glyph name ...") through the tracing-log bridge
    // during PDF text extraction. We already handle PDF outcomes ourselves, so
    // these are pure noise (and stomp the progress bar). RUST_LOG overrides the
    // whole thing if you want to see them.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(default_level)
            .add_directive("pdf_extract=off".parse().unwrap())
            .add_directive("lopdf=off".parse().unwrap())
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
                    collection.as_deref(),
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

            tokio::select! {
                result = rustyweb_lib::server::serve(&bind, &home) => {
                    result?;
                }
                _ = ctrl_c => {}
                _ = terminate => {}
            }
        }

        Commands::Reindex { home } => {
            // Like `index`, silence stdout to hide third-party PDF extraction
            // noise; our logs are on stderr.
            let quiet = gag::Gag::stdout().ok();
            let result = rustyweb_lib::index::reindex(&home);
            drop(quiet);
            result?;
            tracing::info!("reindex complete");
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
                home,
            } => {
                let id = rustyweb_lib::index::set_collection(&home, &name, description, curator)?;
                println!("collection \"{name}\" ({id}) updated");
            }
            CollectionCmd::List { home } => {
                run_collection_list(&home)?;
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

#[cfg(test)]
mod tests {
    use super::parse_source_lines;

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
