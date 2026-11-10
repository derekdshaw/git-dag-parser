use git_dag_parser_core::error::ParseError;
use thiserror::Error;

/// Errors that can occur during DAG operations
#[derive(Error, Debug)]
pub enum DagError {
    #[allow(dead_code)]
    #[error("Repository not found at: {0}")]
    RepoNotFound(String),

    #[allow(dead_code)]
    #[error("DAG file not found: {0}")]
    DagFileNotFound(String),

    #[allow(dead_code)]
    #[error("Invalid DAG file format: {0}")]
    InvalidDagFormat(String),

    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
