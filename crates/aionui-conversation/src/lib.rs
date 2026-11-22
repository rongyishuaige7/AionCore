#![warn(clippy::disallowed_types)]

//! Conversation and message CRUD with streaming relay and event emission.
mod acp_error_recovery;
mod agent_health_policy;
mod background_stream;
mod convert;
pub mod error;
pub(crate) mod message_cursor;
mod message_persistence;
pub mod response_middleware;
mod role_retrospective;
mod role_thread;
mod role_thread_sync;
pub mod routes;
pub mod routes_aux;
mod runtime_completion;
pub mod runtime_create;
mod runtime_persistence;
pub mod runtime_routes;
pub mod runtime_state;
pub mod service;
mod service_ops;
pub(crate) mod session_context;
pub mod session_mentions;
pub mod skill_resolver;
pub mod skill_snapshot;
mod startup_recovery;
pub mod state;
mod stream_persistence;
pub mod stream_relay;
pub mod task_options;
mod title_service;
mod turn_continuation_policy;
mod turn_orchestrator;
mod turn_recovery_policy;

pub use convert::row_to_response_with_extra;
pub use error::ConversationError;
pub use response_middleware::{MessageMiddleware, MiddlewareResult, strip_think_tags};
pub use role_retrospective::{
    CONDENSE_BATCH_SIZE, DREAMER_CURATE_THRESHOLD_BYTES, RoleDreamTasksOutcome, RoleRetrospectiveContext,
};
pub use role_thread::{Line, RoleThreadMark, RoleThreadState, Speaker, is_unfinished_text_row, lines_of};
pub use routes::conversation_routes;
pub use routes_aux::conversation_ops_routes;
pub use runtime_create::ConversationCreateError;
pub use runtime_routes::{ConversationRuntimeRouterState, conversation_runtime_routes};
pub use service::is_temp_session_workspace;
pub use service::{
    ConversationAgentTurnOutcome, ConversationAgentTurnRequest, ConversationAgentTurnStarted,
    ConversationAgentTurnStartedCallback, ConversationAgentTurnStatus, ConversationKeeperBrief, ConversationRolePort,
    ConversationService,
};
pub use state::ConversationRouterState;
pub use turn_orchestrator::ASK_FIRST_RUNTIME_MODE;

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
