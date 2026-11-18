use std::sync::Arc;

use aionui_api_types::{RoleDreamResponse, RoleProposalChange};

use crate::service::RoleService;

/// External side effects invoked by `/api/roles` routes in `aionui-app`
/// (applying user-approved cron proposals, cleaning up cron jobs when a role is
/// dismissed, and running S5 dreamer maintenance).
#[async_trait::async_trait]
pub trait RoleExternalPort: Send + Sync {
    async fn apply_cron_proposal(
        &self,
        user_id: &str,
        role_id: &str,
        change: &RoleProposalChange,
    ) -> Result<(), String>;

    async fn on_role_dismissed(&self, user_id: &str, role_id: &str);

    async fn run_dreamer(&self, user_id: &str, role_id: &str) -> Result<RoleDreamResponse, String>;
}

/// Shared state for role route handlers.
#[derive(Clone)]
pub struct RoleRouterState {
    pub service: Arc<RoleService>,
    pub external: Option<Arc<dyn RoleExternalPort>>,
}

impl RoleRouterState {
    pub fn new(service: Arc<RoleService>) -> Self {
        Self {
            service,
            external: None,
        }
    }

    pub fn with_external(service: Arc<RoleService>, external: Arc<dyn RoleExternalPort>) -> Self {
        Self {
            service,
            external: Some(external),
        }
    }
}
