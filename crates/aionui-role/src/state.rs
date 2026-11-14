use std::sync::Arc;

use crate::service::RoleService;

/// Shared state for role route handlers.
#[derive(Clone)]
pub struct RoleRouterState {
    pub service: Arc<RoleService>,
}
