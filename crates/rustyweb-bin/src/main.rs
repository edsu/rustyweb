use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::fmt::{
    format::{FormatEvent, Format, Writer},
    FmtContext,
};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

// Wraps the default log format so that WARN and ERROR lines are highlighted in
// bold color across the entire line, not just the level label.
struct ColorLineFormat(Format);

impl Default for ColorLineFormat {
    fn default() -> Self {
        Self(Format::default())
    }
}

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
                tracing::Level::WARN  => Some("\x1b[1;33m"),
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
        write!(writer, "{start}{line}\x1b[0m\n")
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
        /// WACZ files or http(s) URLs to index (at least one). A local WACZ must
        /// live under <home>/archive; for several, glob it: `index archive/*.wacz`.
        paths: Vec<String>,

        /// rustyweb home directory (holds archive/ and index/).
        #[arg(long, default_value = ".")]
        home: PathBuf,

        /// Collection display name (defaults to the WACZ title or filename).
        #[arg(long)]
        name: Option<String>,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(true)
        // Logs go to stderr so stdout can carry data (and be silenced during
        // indexing to hide third-party PDF extraction noise).
        .with_writer(std::io::stderr)
        .event_format(ColorLineFormat::default())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Index { paths, home, name } => {
            // `index` no longer auto-scans <home>/archive; a bare invocation is
            // almost always a mistake, so guide the user to the two things they
            // probably meant instead.
            if paths.is_empty() {
                eprintln!(
                    "index needs at least one WACZ file (kept in <home>/archive) or an\n\
                     http(s) URL. For example:\n\
                     \n\
                     \x20 rustyweb index archive/site.wacz      index a local WACZ (must be in archive/)\n\
                     \x20 rustyweb index archive/*.wacz         index several at once\n\
                     \x20 rustyweb index https://ex.org/b.wacz  index a remote WACZ\n\
                     \n\
                     To rebuild the existing index from collections.json (including\n\
                     remote sources), use: rustyweb reindex"
                );
                std::process::exit(2);
            }
            let locations = paths;
            let total = locations.len();
            for (i, location) in locations.iter().enumerate() {
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
                let result = rustyweb_lib::index::index_location(location, &home, name.as_deref());
                drop(quiet);
                result?;
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
            eprintln!("skipping remote collection {} ({})", col.name, col.source.location());
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
