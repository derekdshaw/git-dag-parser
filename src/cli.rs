use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "git-dag-parser")]
#[command(about = "Fast Git DAG parser that directly reads Git object store", long_about = None)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Parse a Git repository and build the object relationship graph
    ///
    /// Reads the Git object store directly (pack files and loose objects)
    /// to build a complete DAG of commits, trees, blobs, and their relationships.
    Init {
        /// Path to the Git repository (must contain .git directory)
        #[arg(short, long, default_value = ".")]
        repo: PathBuf,

        /// Byte limit of the delta cache, such as 512mb or 2gb. The cache
        /// keeps resolved delta bases while a pack is read; 0 caches nothing.
        #[arg(long, value_parser = crate::size::parse_size, default_value = "1gb")]
        delta_cache_size: u64,
    },

    /// Analyze the parsed DAG and generate reports
    ///
    /// Produces statistics about repository size, largest objects,
    /// commit contributions, and more.
    Analyze {
        /// Path to the DAG file created by 'init'
        #[arg(short, long)]
        dag_file: PathBuf,
    },

    /// Query the DAG for specific information
    ///
    /// Supports queries like finding objects by hash, listing
    /// commits that touch a file, etc.
    Query {
        /// Path to the DAG file created by 'init'
        #[arg(short, long)]
        dag_file: PathBuf,

        /// Query string (e.g., "blob:<hash>", "path:<filepath>")
        #[arg(short, long)]
        query: String,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn init_rejects_a_delta_cache_size_that_is_not_valid() {
        let err = Cli::try_parse_from(["git-dag-parser", "init", "--delta-cache-size", "12x"])
            .err()
            .expect("12x is not a valid size");
        assert!(err.to_string().contains("12x"), "{err}");
    }
}
