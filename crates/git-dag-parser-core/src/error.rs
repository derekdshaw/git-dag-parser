use thiserror::Error;

/// Errors that can occur during Git object parsing
#[derive(Error, Debug)]
pub enum ParseError {
    #[error("Not a valid Git repository: {0}")]
    NotAGitRepo(String),

    #[error("SHA-256 repositories are not supported (extensions.objectFormat = sha256)")]
    UnsupportedObjectFormat,

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
