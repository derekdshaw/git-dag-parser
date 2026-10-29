use clap::Parser;
use git_dag_parser::{cli, commands};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
        cli::Commands::Init { repo } => commands::init::run(&repo),
        cli::Commands::Analyze { dag_file } => commands::analyze::run(&dag_file),
        cli::Commands::Query { dag_file, query } => commands::query::run(&dag_file, &query),
    }
}
