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
