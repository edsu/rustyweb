use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
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
