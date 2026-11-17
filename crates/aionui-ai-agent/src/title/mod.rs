//! Model snapshots for title jobs; none of these runners owns a chat runtime.

mod acp;
mod provider;

pub(crate) use acp::AcpTitleGenerator;
pub use aionui_session::{SUMMARY_MAX_BYTES, SUMMARY_TIMEOUT, TITLE_INSTRUCTIONS, TitleGenerator};
pub(crate) use provider::ProviderTitleGenerator;
