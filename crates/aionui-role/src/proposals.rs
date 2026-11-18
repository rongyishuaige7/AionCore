//! Pending approval proposals for agent-authored major changes on a resident
//! role (`PLAN.md` §6):
//! - Major changes (held until user approves): modifying `charter`, relaxing
//!   `denied_tools`, turning off `asks_when_scheduled` or `verifies`, or
//!   creating/updating/deleting scheduled tasks of a resident role.
//! - Tightening changes (applied immediately): adding tools to `denied_tools`,
//!   enabling `asks_when_scheduled` or `verifies`, or pausing (`enabled=false`)
//!   a scheduled task.

use aionui_api_types::{RoleProposal, RoleProposalChange};

use crate::store::RoleDefinition;

pub(crate) struct ClassifiedRoleUpdate {
    /// Tightened definition to persist immediately, if any tightening field changed.
    pub immediate: Option<(RoleDefinition, String)>,
    /// Major change to hold as a pending proposal, if any major field changed.
    pub proposal: Option<(String, String, RoleProposalChange)>,
}

/// Compare `existing` against validated incoming fields (`charter`, normalized
/// `denied_tools`, `verifies`, `asks_when_scheduled`), splitting immediate
/// tightenings from major changes that require user approval.
pub(crate) fn classify_role_update(
    existing: &RoleDefinition,
    charter: String,
    denied_tools: Vec<String>,
    verifies: bool,
    asks_when_scheduled: bool,
    now: i64,
) -> ClassifiedRoleUpdate {
    // Tightening vs relaxing on `denied_tools`:
    // - Tools newly added to `denied_tools` tighten immediately.
    // - Tools removed from `denied_tools` are a relaxation -> held as proposal.
    let added_denials: Vec<String> = denied_tools
        .iter()
        .filter(|tool| !existing.denied_tools.contains(tool))
        .cloned()
        .collect();
    let removed_denials: Vec<String> = existing
        .denied_tools
        .iter()
        .filter(|tool| !denied_tools.contains(tool))
        .cloned()
        .collect();

    let tightened_verifies = !existing.verifies && verifies;
    let relaxed_verifies = existing.verifies && !verifies;

    let tightened_ask = !existing.asks_when_scheduled && asks_when_scheduled;
    let relaxed_ask = existing.asks_when_scheduled && !asks_when_scheduled;

    let charter_changed = existing.charter != charter;

    let mut immediate_reasons = Vec::new();
    let mut next_def = existing.clone();
    if !added_denials.is_empty() {
        for tool in &added_denials {
            if !next_def.denied_tools.contains(tool) {
                next_def.denied_tools.push(tool.clone());
            }
        }
        next_def.denied_tools.sort();
        next_def.denied_tools.dedup();
        immediate_reasons.push(format!("deny {}", added_denials.join(", ")));
    }
    if tightened_verifies {
        next_def.verifies = true;
        immediate_reasons.push("enable review gate".into());
    }
    if tightened_ask {
        next_def.asks_when_scheduled = true;
        immediate_reasons.push("enable ask-first on scheduled runs".into());
    }
    let immediate = if !immediate_reasons.is_empty() {
        next_def.updated_at = now;
        Some((next_def, format!("tighten: {}", immediate_reasons.join("; "))))
    } else {
        None
    };

    let mut major_parts = Vec::new();
    let mut detail_lines = Vec::new();

    if charter_changed {
        major_parts.push("update charter");
        detail_lines.push(format!("charter:\n- {}\n+ {}", existing.charter, charter));
    }
    if !removed_denials.is_empty() {
        major_parts.push("allow tools");
        detail_lines.push(format!("allow previously denied tools: {}", removed_denials.join(", ")));
    }
    if relaxed_ask {
        major_parts.push("turn off ask-first");
        detail_lines.push("asks_when_scheduled: true -> false".into());
    }
    if relaxed_verifies {
        major_parts.push("turn off review gate");
        detail_lines.push("verifies: true -> false".into());
    }

    let proposal = if !major_parts.is_empty() {
        // Build the post-approval target `denied_tools`: keep any newly added
        // denials while removing `removed_denials`.
        let proposed_denied = if !removed_denials.is_empty() {
            let mut target = existing.denied_tools.clone();
            target.extend(added_denials);
            target.retain(|t| !removed_denials.contains(t));
            target.sort();
            target.dedup();
            Some(target)
        } else {
            None
        };
        let change = RoleProposalChange::UpdateRole {
            charter: charter_changed.then_some(charter),
            denied_tools: proposed_denied,
            verifies: relaxed_verifies.then_some(false),
            asks_when_scheduled: relaxed_ask.then_some(false),
        };
        Some((major_parts.join(", "), detail_lines.join("\n\n"), change))
    } else {
        None
    };

    ClassifiedRoleUpdate { immediate, proposal }
}

pub(crate) fn new_proposal_id(now: i64, existing: &[RoleProposal]) -> String {
    let base = format!("prop-{now}");
    if !existing.iter().any(|p| p.id == base) {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !existing.iter().any(|p| p.id == candidate) {
            return candidate;
        }
        suffix += 1;
    }
}
