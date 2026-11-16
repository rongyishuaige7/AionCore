//! Brings a follow-up back to the member that set it, and a scheduled task to
//! the member whose conversation it is bound to.
//!
//! A member's timer never starts a run of its own. When it comes due the note
//! is put in the member's mailbox like any other message, so it takes its
//! turn behind whatever the member is doing, and a member that is dormant is
//! woken the way a teammate's message would wake it.

use super::TeamSession;
use crate::error::TeamError;
use crate::types::MailboxMessageType;
use crate::work_source::WorkSource;

/// Mailbox sender of a follow-up, the one every system notice uses.
const FOLLOW_UP_SENDER: &str = "team_system";

impl TeamSession {
    /// Puts `content` in the member's mailbox and conversation, then wakes the
    /// member, starting its process first if it is dormant. A member the user
    /// paused keeps the note for when it is resumed.
    pub(crate) async fn deliver_follow_up(&self, slot_id: &str, content: &str) -> Result<(), TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        self.ensure_member_runtime_lazy(slot_id, true).await?;
        let notice = self
            .mailbox
            .write(
                &self.team.id,
                slot_id,
                FOLLOW_UP_SENDER,
                MailboxMessageType::Message,
                content,
                None,
            )
            .await?;
        self.project_team_system_message(slot_id, &agent.conversation_id, &notice.id, &notice.content)
            .await;
        self.enqueue_system_work(slot_id, WorkSource::FollowUp, Some(notice.id), None)
            .await?;
        Ok(())
    }
}
