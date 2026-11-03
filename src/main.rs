use clap::Parser;
use git_dag_parser::{cli, commands, parser};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// glibc malloc slows down when pass 2 frees delta cache entries of many
// sizes in mixed order: linux went from 204 s to 335 s with the 3b cache.
// mimalloc keeps each allocation fast (133 s). Only the binary sets it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = cli::Cli::parse();

    match cli.command {
        cli::Commands::Init {
            repo,
            delta_cache_size,
        } => commands::init::run(
            &repo,
            &parser::ParseOptions {
                delta_cache_limit: delta_cache_size,
            },
        ),
        cli::Commands::Analyze { dag_file } => commands::analyze::run(&dag_file),
        cli::Commands::Query { dag_file, query } => commands::query::run(&dag_file, &query),
    }
}
