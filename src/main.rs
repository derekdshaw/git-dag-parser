mod progress;

use clap::Parser;
use git_dag_parser::{cli, commands};
use git_dag_parser_core::parser;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

// glibc malloc slows down when pass 2 frees delta cache entries of many
// sizes in mixed order: linux went from 204 s to 335 s with the 3b cache.
// mimalloc keeps each allocation fast (133 s). Only the binary sets it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// mimalloc has a numbered list of settings, and `mi_option_set` takes the
// number of a setting and its new value. 15 is the number of the "purge
// delay" setting: how many milliseconds mimalloc waits before it gives
// memory that the program freed back to the operating system. Until then
// the memory still counts as used by this process.
//
// The number comes from the list in mimalloc's C header (mimalloc.h of the
// mimalloc v3 that libmimalloc-sys 0.1.49 builds). The Rust bindings have
// no name for this setting, so it is written out here. Check it again when
// libmimalloc-sys is updated.
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

fn main() -> anyhow::Result<()> {
    // Give freed memory back at once (a delay of 0). The default waits 1 s,
    // and up to 10 s for some memory. Pass 2's threads free and allocate a
    // lot of memory in that time: on 32 threads, linux used up to 12.1 GB
    // in pass 2 with the default, and 8.4 GB with no delay.
    // SAFETY: a call into mimalloc's C library that changes one setting;
    // mimalloc allows this at any time.
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };

    // Initialize tracing. The filter applies to all layers, so below the
    // info level there is no progress bar. The progress events go to the
    // bar only, not to the log.
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(filter_fn(|m| m.target() != parser::PROGRESS_TARGET)),
        )
        .with(progress::ProgressLayer::default())
        .init();

    let cli = cli::Cli::parse();

    // RAYON_NUM_THREADS, if set, overrides the default thread count
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        let hardware = std::thread::available_parallelism().map_or(1, |n| n.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads(parser::default_threads(hardware))
            .build_global()?;
    }

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
