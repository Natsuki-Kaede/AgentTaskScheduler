/// The named job or submission does not exist.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NotFound(pub String);

/// The request does not fit the job's current state.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Conflict(pub String);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidInput(pub String);
