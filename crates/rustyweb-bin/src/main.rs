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

        // Buffer without inner ANSI so the inner reset codes don't break our
        // full-line color, then wrap the finished line.
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
    /// Index one or more WARC / WACZ files.
    Index {
        /// Files or directories to index.
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Directory where the index will be stored.
        #[arg(short, long, default_value = "index")]
        index_dir: PathBuf,

        /// Collection display name (defaults to the filename stem).
        #[arg(long)]
        name: Option<String>,
    },
    /// Start the replay web server.
    Serve {
        /// Address to listen on.
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        bind: String,

        /// Index directory created by `rustyweb index`.
        #[arg(short, long, default_value = "index")]
        index_dir: PathBuf,
    },
    /// Verify integrity of indexed collections by re-hashing each file.
    Check {
        /// Index directory created by `rustyweb index`.
        #[arg(short, long, default_value = "index")]
        index_dir: PathBuf,
    },
    /// Look up a URL in the index and print any matching CDX records.
    Lookup {
        /// URL to look up (exact, fuzzy, and prefix fallbacks are tried).
        url: String,

        /// Index directory created by `rustyweb index`.
        #[arg(short, long, default_value = "index")]
        index_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(true)
        .event_format(ColorLineFormat::default())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Index { paths, index_dir, name } => {
            let total = paths.len();
            for (i, path) in paths.iter().enumerate() {
                tracing::info!(
                    file = %path.display(),
                    progress = format!("{}/{}", i + 1, total),
                    "indexing"
                );
                rustyweb_lib::index::index_path(path, &index_dir, name.as_deref())?;
            }
            tracing::info!("indexing complete");
        }

        Commands::Serve { bind, index_dir } => {
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
                result = rustyweb_lib::server::serve(&bind, &index_dir) => {
                    result?;
                }
                _ = ctrl_c => {}
                _ = terminate => {}
            }
        }

        Commands::Check { index_dir } => {
            run_check(&index_dir)?;
        }

        Commands::Lookup { url, index_dir } => {
            run_lookup(&url, &index_dir)?;
        }
    }

    Ok(())
}

fn run_lookup(url: &str, index_dir: &std::path::Path) -> Result<()> {
    use rustyweb_lib::cdx::{CdxStore, MatchType, normalize_url_fuzzy};

    let store = CdxStore::open(&index_dir.join("cdx"))?;

    // 1. Exact match.
    let mut records = store.query(url, MatchType::Exact, None, None, 100)?;
    let mut match_kind = "exact";

    // 2. Fuzzy: strip tracking params, sort remaining.
    if records.is_empty() {
        let fuzzy = normalize_url_fuzzy(url);
        if fuzzy != url {
            records = store.query(&fuzzy, MatchType::Exact, None, None, 100)?;
            match_kind = "fuzzy (tracking params stripped)";
        }
    }

    // 3. Prefix: strip query string entirely.
    if records.is_empty() {
        if let Ok(mut stripped) = url::Url::parse(url) {
            if stripped.query().is_some() {
                stripped.set_query(None);
                records = store.query(stripped.as_str(), MatchType::Prefix, None, None, 100)?;
                match_kind = "prefix (query string stripped)";
            }
        }
    }

    if records.is_empty() {
        println!("Not found: {url}");
        return Ok(());
    }

    println!("Found {} record(s) via {} match for: {url}\n", records.len(), match_kind);
    for r in &records {
        println!("  timestamp:     {}", r.timestamp);
        println!("  url:           {}", r.original_url);
        println!("  status:        {}", r.status);
        println!("  mime:          {}", r.mimetype);
        println!("  length:        {} bytes (compressed)", r.length);
        println!("  digest:        {}", r.digest);
        // warc_path may be "outer.wacz\x1einner.warc.gz" — display both parts.
        if let Some((outer, inner)) = r.warc_path.split_once('\x1e') {
            println!("  wacz:          {}", outer);
            println!("  inner_warc:    {}", inner);
        } else {
            println!("  warc_path:     {}", r.warc_path);
        }
        println!("  warc_offset:   {}", r.warc_offset);
        println!("  record_length: {}", r.warc_record_length);
        println!();
    }

    Ok(())
}

fn run_check(index_dir: &std::path::Path) -> Result<()> {
    use rustyweb_lib::collections::{CollectionManifest, file_sha256};

    let manifest = CollectionManifest::open(index_dir)?;
    if manifest.collections.is_empty() {
        println!("No collections registered in {}", index_dir.display());
        return Ok(());
    }

    let mut ok = 0usize;
    let mut missing = 0usize;
    let mut modified = 0usize;

    for col in &manifest.collections {
        if !col.path.exists() {
            println!("MISSING  {} ({})", col.name, col.path.display());
            missing += 1;
            continue;
        }
        match file_sha256(&col.path) {
            Ok(hash) if hash == col.sha256 => {
                println!("OK       {} ({})", col.name, col.path.display());
                ok += 1;
            }
            Ok(hash) => {
                println!(
                    "MODIFIED {} ({}) — expected {} got {}",
                    col.name,
                    col.path.display(),
                    &col.sha256[..8],
                    &hash[..8]
                );
                modified += 1;
            }
            Err(e) => {
                println!("ERROR    {} ({}) — {e}", col.name, col.path.display());
                missing += 1;
            }
        }
    }

    println!(
        "\n{} OK, {} missing, {} modified",
        ok, missing, modified
    );

    if missing > 0 || modified > 0 {
        std::process::exit(1);
    }
    Ok(())
}
