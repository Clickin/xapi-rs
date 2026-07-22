use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Path(String);

impl Path {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn push(mut self, segment: impl fmt::Display) -> Self {
        if !self.0.is_empty() {
            self.0.push('.');
        }
        self.0.push_str(&segment.to_string());
        self
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("{0}")]
    Message(String),
    #[error("invalid {target} value {value:?} for wire type {wire}")]
    InvalidScalar {
        value: String,
        wire: String,
        target: String,
    },
    #[error("limit exceeded: {0}")]
    Limit(&'static str),
}

impl DecodeError {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
    pub fn scalar(
        value: impl Into<String>,
        wire: impl fmt::Display,
        target: impl Into<String>,
    ) -> Self {
        Self::InvalidScalar {
            value: value.into(),
            wire: wire.to_string(),
            target: target.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("{0}")]
    Message(String),
    #[error("value cannot be encoded as {0}")]
    InvalidScalar(String),
    #[error("limit exceeded: {0}")]
    Limit(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("decode error at {path}: {source}")]
    Decode {
        path: Path,
        #[source]
        source: DecodeError,
    },
    #[error("encode error at {path}: {source}")]
    Encode {
        path: Path,
        #[source]
        source: EncodeError,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn prepend_path(self, prefix: impl Into<String>) -> Self {
        let prefix = Path::new(prefix);
        match self {
            Self::Decode { path, source } => Self::Decode {
                path: if path.to_string().is_empty() {
                    prefix
                } else {
                    prefix.push(path)
                },
                source,
            },
            Self::Encode { path, source } => Self::Encode {
                path: if path.to_string().is_empty() {
                    prefix
                } else {
                    prefix.push(path)
                },
                source,
            },
            other => other,
        }
    }
}

impl From<DecodeError> for Error {
    fn from(source: DecodeError) -> Self {
        Self::Decode {
            path: Path::default(),
            source,
        }
    }
}

impl From<EncodeError> for Error {
    fn from(source: EncodeError) -> Self {
        Self::Encode {
            path: Path::default(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
