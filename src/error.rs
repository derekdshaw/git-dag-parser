use thiserror::Error;

/// Errors that can occur during Git object parsing
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Invalid pack file header: expected 'PACK', got {0:?}")]
    InvalidPackHeader([u8; 4]),

    #[error("Unsupported pack version: {0}")]
    UnsupportedPackVersion(u32),

    #[allow(dead_code)]
    #[error("Invalid object type: {0}")]
    InvalidObjectType(u8),

    #[error("Failed to decompress object: {0}")]
    DecompressionError(String),

    #[error("Invalid object format: {0}")]
    InvalidObjectFormat(String),

    #[error("Object not found: {0}")]
    ObjectNotFound(String),

    #[allow(dead_code)]
    #[error("Delta base not found at offset {0}")]
    DeltaBaseNotFound(u64),

    #[error("Invalid delta instruction")]
    InvalidDeltaInstruction,

    #[error("Too many distinct path names (limit {0})")]
    TooManyNames(usize),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

/// Errors that can occur during DAG operations
#[derive(Error, Debug)]
pub enum DagError {
    #[allow(dead_code)]
    #[error("Repository not found at: {0}")]
    RepoNotFound(String),

    #[error("Not a valid Git repository: {0}")]
    NotAGitRepo(String),

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
