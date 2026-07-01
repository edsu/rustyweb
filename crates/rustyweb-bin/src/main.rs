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
    /// Index one or more WACZ files.
    Index {
        /// WACZ files or directories to index.
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
    /// Search indexed WACZ files for CDX records matching a URL.
    SearchUrl {
        /// URL to search for (exact match against archived URLs).
        url: String,

        /// Index directory created by `rustyweb index`.
        #[arg(short, long, default_value = "index")]
        index_dir: PathBuf,
    },
    /// Verify the fixity of indexed WACZ files by re-hashing each one.
    Verify {
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

        Commands::SearchUrl { url, index_dir } => {
            run_search_url(&url, &index_dir)?;
        }

        Commands::Verify { index_dir } => {
            let all_ok = run_verify(&index_dir)?;
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
fn run_verify(index_dir: &std::path::Path) -> Result<bool> {
    use rustyweb_lib::collections::{file_sha256, CollectionManifest};

    let manifest = CollectionManifest::open(index_dir)?;
    if manifest.collections.is_empty() {
        println!("No collections registered in {}", index_dir.display());
        return Ok(true);
    }

    let mut ok = 0usize;
    let mut missing = 0usize;
    let mut modified = 0usize;

    for col in &manifest.collections {
        if !col.path.exists() {
            println!("MISSING   {} ({})", col.name, col.path.display());
            missing += 1;
            continue;
        }
        match file_sha256(&col.path) {
            Ok(hash) if hash == col.sha256 => {
                println!("OK        {} ({})", col.name, col.path.display());
                ok += 1;
            }
            Ok(hash) => {
                println!(
                    "MODIFIED  {} ({}) - expected {}… got {}…",
                    col.name,
                    col.path.display(),
                    short_hash(&col.sha256),
                    short_hash(&hash),
                );
                modified += 1;
            }
            Err(e) => {
                println!("ERROR     {} ({}) - {e}", col.name, col.path.display());
                missing += 1;
            }
        }
    }

    println!("\n{ok} OK, {missing} missing, {modified} modified");
    Ok(missing == 0 && modified == 0)
}

/// First 8 characters of a hex hash for compact display.
fn short_hash(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

fn run_search_url(url: &str, index_dir: &std::path::Path) -> Result<()> {
    use rustyweb_lib::collections::CollectionManifest;
    use rustyweb_lib::wacz::search_cdx;

    let manifest = CollectionManifest::open(index_dir)?;
    if manifest.collections.is_empty() {
        println!("No collections registered in {}", index_dir.display());
        return Ok(());
    }

    let mut found_any = false;
    for col in &manifest.collections {
        if !col.path.exists() {
            eprintln!("warning: {} not found at {}", col.name, col.path.display());
            continue;
        }
        let records = search_cdx(&col.path, url)?;
        if records.is_empty() {
            continue;
        }
        found_any = true;
        println!("Collection: {} ({})", col.name, col.path.display());
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
