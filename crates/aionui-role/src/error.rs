/// Errors surfaced by the role service. Route handlers map these to `ApiError`
/// with stable machine codes; filesystem detail never leaks to the client.
#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    /// The id cannot name a role directory (empty, too long, or characters
    /// outside `[A-Za-z0-9._-]`). Maps to 400.
    #[error("invalid role id: {0}")]
    InvalidId(String),
    /// Malformed definition or memory (unknown tool, over the size cap). Maps to 400.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// No role is defined for this id. Maps to 404.
    #[error("role not found: {0}")]
    NotFound(String),
    /// The role's definition file exists but cannot be parsed.
    #[error("role definition is unreadable: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
