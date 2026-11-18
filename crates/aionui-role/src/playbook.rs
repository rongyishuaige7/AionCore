//! ACE-style itemized playbook (`memory/PLAYBOOK.md`), shared boss profile
//! (`roles/_boss/USER.md`), and zero-LLM decay scoring.
//!
//! Playbook lines look like:
//! `- [p-0001] (+3/-1) 发周报前先核对数据源`
//! Boss profile lines look like:
//! `- [u-0001] 用户喜欢先看结论再看细节`

use std::collections::HashMap;

use aionui_api_types::{PlaybookDeltaOp, RolePlaybookEntry};
use serde::{Deserialize, Serialize};

/// Cap on one playbook or boss-profile bullet, in characters.
pub const MAX_PLAYBOOK_ITEM_CHARS: usize = 400;
/// Cap on active bullets in `PLAYBOOK.md`.
pub const MAX_PLAYBOOK_ITEMS: usize = 60;
/// Cap on active bullets in `roles/_boss/USER.md`.
pub const MAX_BOSS_ITEMS: usize = 50;
/// Grace floor (in days) before a zero-use item can be archived by decay.
pub const PLAYBOOK_GRACE_DAYS: i64 = 14;
/// Idle threshold (in days) after which low-score items are archived by decay.
pub const PLAYBOOK_ARCHIVE_IDLE_DAYS: i64 = 30;
/// Idle threshold (in days) after which untouched `progress/<task>.md` files are archived.
pub const PROGRESS_ARCHIVE_IDLE_DAYS: i64 = 21;

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const HALF_LIFE_DAYS: f64 = 30.0;
const DECAY_SCORE_FLOOR: f64 = 0.45;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PlaybookMeta {
    #[serde(default = "default_next_id")]
    pub next_id: u32,
    #[serde(default)]
    pub items: HashMap<String, PlaybookItemStamp>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct PlaybookItemStamp {
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_next_id() -> u32 {
    1
}

/// Parse `PLAYBOOK.md` into structured entries, attaching timestamps from `meta`.
pub(crate) fn parse_playbook(raw: &str, meta: &PlaybookMeta) -> Vec<RolePlaybookEntry> {
    let mut entries = Vec::new();
    for line in raw.lines() {
        if let Some(mut entry) = parse_playbook_line(line) {
            if let Some(stamp) = meta.items.get(&entry.id) {
                entry.updated_at = stamp.updated_at.max(stamp.created_at);
            }
            entries.push(entry);
        }
    }
    entries
}

/// Render `RolePlaybookEntry` items back to canonical `PLAYBOOK.md` format.
pub(crate) fn render_playbook(entries: &[RolePlaybookEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for entry in entries {
        out.push_str(&format!(
            "- [{}] (+{}/-{}) {}\n",
            entry.id, entry.helpful, entry.harmful, entry.content
        ));
    }
    out
}

/// Format active playbook entries for prompt injection (`standing_role.rs`).
pub(crate) fn prompt_playbook_lines(entries: &[RolePlaybookEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            format!(
                "[{}] (+{}/-{}) {}",
                entry.id, entry.helpful, entry.harmful, entry.content
            )
        })
        .collect()
}

fn parse_playbook_line(line: &str) -> Option<RolePlaybookEntry> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("- [")?;
    let (id, after_id) = rest.split_once(']')?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let after_id = after_id.trim_start();
    let (helpful, harmful, content) = if let Some(after_paren) = after_id.strip_prefix('(')
        && let Some((counts, tail)) = after_paren.split_once(')')
        && let Some((pos, neg)) = parse_counts(counts)
    {
        (pos, neg, tail.trim())
    } else {
        (0, 0, after_id)
    };
    if content.is_empty() {
        return None;
    }
    Some(RolePlaybookEntry {
        id: id.to_owned(),
        helpful,
        harmful,
        content: content.to_owned(),
        updated_at: 0,
    })
}

fn parse_counts(raw: &str) -> Option<(u32, u32)> {
    let (pos_raw, neg_raw) = raw.trim().split_once('/')?;
    let pos = pos_raw.trim().strip_prefix('+')?.parse::<u32>().ok()?;
    let neg = neg_raw.trim().strip_prefix('-')?.parse::<u32>().ok()?;
    Some((pos, neg))
}

fn extract_id_num(id: &str, prefix: &str) -> Option<u32> {
    id.strip_prefix(prefix)?.parse::<u32>().ok()
}

/// Apply incremental ACE delta operations to `entries` and `meta`.
/// Returns how many operations actually mutated state.
pub(crate) fn apply_playbook_deltas(
    entries: &mut Vec<RolePlaybookEntry>,
    meta: &mut PlaybookMeta,
    archived_raw: &str,
    ops: &[PlaybookDeltaOp],
    now: i64,
) -> usize {
    let max_seen = entries
        .iter()
        .map(|e| e.id.as_str())
        .chain(archived_raw.lines().filter_map(|line| {
            line.trim()
                .strip_prefix("- [")
                .and_then(|r| r.split_once(']').map(|(id, _)| id))
        }))
        .filter_map(|id| extract_id_num(id, "p-"))
        .max()
        .unwrap_or(0);
    meta.next_id = meta.next_id.max(max_seen + 1);

    let mut applied = 0;
    for op in ops {
        match op {
            PlaybookDeltaOp::Add { content } => {
                let clean = flatten_capped(content, MAX_PLAYBOOK_ITEM_CHARS);
                if clean.is_empty() {
                    continue;
                }
                if let Some(existing) = entries
                    .iter_mut()
                    .find(|item| item.content.eq_ignore_ascii_case(&clean) || item.content == clean)
                {
                    existing.helpful = existing.helpful.saturating_add(1);
                    existing.updated_at = now;
                    let stamp = meta.items.entry(existing.id.clone()).or_insert(PlaybookItemStamp {
                        created_at: now,
                        updated_at: now,
                    });
                    stamp.updated_at = now;
                    applied += 1;
                    continue;
                }
                if entries.len() >= MAX_PLAYBOOK_ITEMS {
                    continue;
                }
                let id = format!("p-{:04}", meta.next_id);
                meta.next_id += 1;
                meta.items.insert(
                    id.clone(),
                    PlaybookItemStamp {
                        created_at: now,
                        updated_at: now,
                    },
                );
                entries.push(RolePlaybookEntry {
                    id,
                    helpful: 1,
                    harmful: 0,
                    content: clean,
                    updated_at: now,
                });
                applied += 1;
            }
            PlaybookDeltaOp::Update {
                id,
                content,
                helpful_delta,
                harmful_delta,
            } => {
                let target_id = id.trim();
                let Some(existing) = entries.iter_mut().find(|item| item.id == target_id) else {
                    continue;
                };
                let mut changed = false;
                if let Some(new_text) = content {
                    let clean = flatten_capped(new_text, MAX_PLAYBOOK_ITEM_CHARS);
                    if !clean.is_empty() && clean != existing.content {
                        existing.content = clean;
                        changed = true;
                    }
                }
                let add_pos = (*helpful_delta).min(5);
                let add_neg = (*harmful_delta).min(5);
                if add_pos > 0 {
                    existing.helpful = existing.helpful.saturating_add(add_pos);
                    changed = true;
                }
                if add_neg > 0 {
                    existing.harmful = existing.harmful.saturating_add(add_neg);
                    changed = true;
                }
                if changed {
                    existing.updated_at = now;
                    let stamp = meta.items.entry(existing.id.clone()).or_insert(PlaybookItemStamp {
                        created_at: now,
                        updated_at: now,
                    });
                    stamp.updated_at = now;
                    applied += 1;
                }
            }
            PlaybookDeltaOp::Remove { id } => {
                let target_id = id.trim();
                let before = entries.len();
                entries.retain(|item| item.id != target_id);
                if entries.len() != before {
                    meta.items.remove(target_id);
                    applied += 1;
                }
            }
        }
    }
    applied
}

/// Zero-LLM decay pass over `entries`:
/// 1. Backfills missing timestamps in `meta` to `now` (grace floor for hand-edited items).
/// 2. Archives contradicted items (`harmful >= 2 && harmful > helpful`) immediately.
/// 3. After `PLAYBOOK_GRACE_DAYS`, computes `utility * 0.5^(idle_days / 30)` and
///    archives items idle `>= PLAYBOOK_ARCHIVE_IDLE_DAYS` whose decayed score falls
///    below `DECAY_SCORE_FLOOR`.
///
/// Returns the archived entries removed from `entries`.
pub(crate) fn decay_playbook(
    entries: &mut Vec<RolePlaybookEntry>,
    meta: &mut PlaybookMeta,
    now: i64,
) -> Vec<RolePlaybookEntry> {
    let mut kept = Vec::with_capacity(entries.len());
    let mut archived = Vec::new();

    for mut entry in entries.drain(..) {
        let stamp = meta.items.entry(entry.id.clone()).or_insert(PlaybookItemStamp {
            created_at: now,
            updated_at: now,
        });
        let touched = stamp.updated_at.max(stamp.created_at);
        entry.updated_at = touched;

        let contradicted = entry.harmful >= 2 && entry.harmful > entry.helpful;
        let idle_days = ((now.saturating_sub(touched)) / DAY_MS).max(0);
        let score = playbook_decay_score(entry.helpful, entry.harmful, idle_days);
        let min_archive_days = PLAYBOOK_ARCHIVE_IDLE_DAYS.max(PLAYBOOK_GRACE_DAYS);
        let decayed_out = idle_days >= min_archive_days && score < DECAY_SCORE_FLOOR;

        if contradicted || decayed_out {
            archived.push(entry);
        } else {
            kept.push(entry);
        }
    }

    *entries = kept;
    archived
}

/// Deterministic utility × half-life score used by zero-LLM decay.
pub fn playbook_decay_score(helpful: u32, harmful: u32, idle_days: i64) -> f64 {
    let utility = 1.0 + (1.0 + f64::from(helpful)).ln() - 0.8 * f64::from(harmful);
    if utility <= 0.0 {
        return 0.0;
    }
    let decay = 0.5_f64.powf((idle_days.max(0) as f64) / HALF_LIFE_DAYS);
    utility * decay
}

/// Append archived playbook items to `PLAYBOOK-archived.md`.
pub(crate) fn append_archived_playbook(existing: &str, archived: &[RolePlaybookEntry], day: &str) -> String {
    if archived.is_empty() {
        return existing.to_owned();
    }
    let mut out = existing.trim_end().to_owned();
    if !out.is_empty() {
        out.push('\n');
    }
    for entry in archived {
        out.push_str(&format!(
            "- [{}] (+{}/-{}) (archived {day}) {}\n",
            entry.id, entry.helpful, entry.harmful, entry.content
        ));
    }
    out
}

/// Apply incremental ACE delta operations to `roles/_boss/USER.md` (`- [u-0001] <fact>`).
/// Returns `Some(updated_markdown)` when at least one change took effect.
pub(crate) fn apply_boss_deltas(existing_raw: &str, ops: &[PlaybookDeltaOp]) -> Option<String> {
    if ops.is_empty() {
        return None;
    }
    let mut items: Vec<(String, String)> = Vec::new();
    let mut max_num: u32 = 0;

    for line in existing_raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- [")
            && let Some((id, tail)) = rest.split_once(']')
        {
            let id = id.trim();
            let content = tail.trim();
            if !id.is_empty() && !content.is_empty() {
                if let Some(num) = extract_id_num(id, "u-") {
                    max_num = max_num.max(num);
                }
                items.push((id.to_owned(), content.to_owned()));
                continue;
            }
        }
        if let Some(plain) = trimmed.strip_prefix("- ").map(str::trim).filter(|s| !s.is_empty()) {
            max_num += 1;
            items.push((format!("u-{max_num:04}"), plain.to_owned()));
        }
    }

    let mut next_num = max_num + 1;
    let mut changed = false;

    for op in ops {
        match op {
            PlaybookDeltaOp::Add { content } => {
                let clean = flatten_capped(content, MAX_PLAYBOOK_ITEM_CHARS);
                if clean.is_empty() {
                    continue;
                }
                if items
                    .iter()
                    .any(|(_, text)| text.eq_ignore_ascii_case(&clean) || text == &clean)
                {
                    continue;
                }
                if items.len() >= MAX_BOSS_ITEMS {
                    continue;
                }
                items.push((format!("u-{next_num:04}"), clean));
                next_num += 1;
                changed = true;
            }
            PlaybookDeltaOp::Update { id, content, .. } => {
                let target_id = id.trim();
                let Some(new_text) = content else {
                    continue;
                };
                let clean = flatten_capped(new_text, MAX_PLAYBOOK_ITEM_CHARS);
                if clean.is_empty() {
                    continue;
                }
                if let Some((_, text)) = items.iter_mut().find(|(item_id, _)| item_id == target_id)
                    && *text != clean
                {
                    *text = clean;
                    changed = true;
                }
            }
            PlaybookDeltaOp::Remove { id } => {
                let target_id = id.trim();
                let before = items.len();
                items.retain(|(item_id, _)| item_id != target_id);
                if items.len() != before {
                    changed = true;
                }
            }
        }
    }

    if !changed {
        return None;
    }
    let mut out = String::new();
    for (id, text) in items {
        out.push_str(&format!("- [{id}] {text}\n"));
    }
    Some(out)
}

fn flatten_capped(raw: &str, max_chars: usize) -> String {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        flat
    } else {
        flat.chars().take(max_chars).collect::<String>().trim_end().to_owned()
    }
}
