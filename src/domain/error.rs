#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NotFound(pub String);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Conflict(pub String);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidInput(pub String);
