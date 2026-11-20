//! `aioncore antigravity-hook`: agy's PreToolUse gate.
//!
//! agy runs this once per tool call, writing the request to our stdin and
//! reading the decision from our stdout. We forward the request to the running
//! AionUi backend, which raises a permission card and blocks until the user
//! answers.
//!
//! FAIL-CLOSED: when invoked inside an AionUi-managed session, every failure
//! path (missing env, unreachable backend, malformed payload, timeout) answers
//! `deny`. When `<workspace>/.agents/hooks.json` is picked up by a standalone
//! `agy` CLI started outside AionUi (no callback address AND no AionUi backend
//! in the ancestor process tree), we return `allow` so agy's own CLI
//! permission settings govern the session.

use std::collections::HashSet;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use aionui_api_types::{AntigravityHookConfig, AntigravityHookInput, AntigravityHookOutput};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// agy's own headless timeout is `--print-timeout` (default 5m). Stay well
/// under it so a wedged bridge surfaces as a denial rather than a stalled turn.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(240);

/// Bound on how deep the parent-process walk climbs.
const MAX_ANCESTOR_DEPTH: usize = 64;

const MISSING_CALLBACK_REASON: &str = "aionui hook is not configured (missing callback address)";
const EXTERNAL_CLI_ALLOW_REASON: &str = "outside AionUi session (no AionUi host in process ancestry)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostAncestry {
    /// Confirmed an AionUi backend process in the ancestor chain.
    Aioncore,
    /// Reached the root of the user process tree without finding an AionUi host.
    External,
    /// Could not inspect the process tree; fail closed.
    Unknown,
}

pub(crate) async fn run_antigravity_hook() -> ExitCode {
    let decision = decide().await;
    let body = serde_json::to_string(&decision).unwrap_or_else(|_| {
        // Serializing our own type cannot realistically fail, but agy MUST get
        // parseable JSON or it will treat the hook as broken.
        r#"{"decision":"deny","reason":"aionui hook could not serialize its answer"}"#.to_owned()
    });
    let mut stdout = tokio::io::stdout();
    let _ = stdout.write_all(body.as_bytes()).await;
    let _ = stdout.flush().await;
    ExitCode::SUCCESS
}

async fn decide() -> AntigravityHookOutput {
    let mut raw = String::new();
    if let Err(e) = tokio::io::stdin().read_to_string(&mut raw).await {
        return AntigravityHookOutput::deny(format!("aionui hook could not read the request: {e}"));
    }
    let input: AntigravityHookInput = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return AntigravityHookOutput::deny(format!("aionui hook could not parse the request: {e}")),
    };

    let base_url = match std::env::var(AntigravityHookConfig::ENV_BASE_URL) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return decision_without_callback(detect_host_ancestry()),
    };
    let token = std::env::var(AntigravityHookConfig::ENV_TOKEN).unwrap_or_default();
    let conversation_id = std::env::var(AntigravityHookConfig::ENV_CONVERSATION_ID)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| input.conversation_id.clone());

    let url = format!(
        "{}/internal/antigravity-hook/{conversation_id}",
        base_url.trim_end_matches('/')
    );
    let client = match reqwest::Client::builder().timeout(CALLBACK_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return AntigravityHookOutput::deny(format!("aionui hook could not build its client: {e}")),
    };

    match client
        .post(&url)
        .header(AntigravityHookConfig::TOKEN_HEADER, token)
        .body(raw)
        .header("content-type", "application/json")
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            decision_from_response(status, &body)
        }
        // The user closed AionUi, the turn was cancelled, or nobody answered in
        // time. Denying is the only safe reading of "no answer".
        Err(e) => AntigravityHookOutput::deny(format!("aionui did not answer: {e}")),
    }
}

/// Turn a callback response into a decision.
///
/// **The status is read before the body.** A non-2xx carries an `ErrorResponse`,
/// not an `AntigravityHookOutput`, so handing it to the decision parser reports
/// a rejection as a parse failure. That is what users saw when CSRF was blocking
/// this endpoint (fixed in #860): the reply carried
/// `aionui returned an unreadable decision: error decoding response body`,
/// which names the wrong thing to go looking at — the body was perfectly
/// readable, the request had been refused.
///
/// Split out from the caller so the mapping is testable without standing up a
/// server; the reasons above are exactly the cases worth pinning.
///
/// Every branch denies. A hook that cannot obtain a clear approval must never
/// let the tool through.
fn decision_from_response(status: reqwest::StatusCode, body: &str) -> AntigravityHookOutput {
    if !status.is_success() {
        // Name the code when the body is our own error envelope; some rejections
        // (a proxy, a crash page) will not have one.
        let code = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["code"].as_str().map(str::to_owned));
        return match code {
            Some(code) => AntigravityHookOutput::deny(format!("aionui refused the request: HTTP {status} ({code})")),
            None => AntigravityHookOutput::deny(format!("aionui refused the request: HTTP {status}")),
        };
    }
    match serde_json::from_str::<AntigravityHookOutput>(body) {
        Ok(out) => out,
        Err(e) => AntigravityHookOutput::deny(format!("aionui returned an unreadable decision: {e}")),
    }
}

/// Decide when `ENV_BASE_URL` is absent: fail closed if an AionUi backend is in
/// our ancestor chain (or if ancestry cannot be inspected); allow only when
/// confirmed to be running from an external CLI session outside AionUi.
fn decision_without_callback(ancestry: HostAncestry) -> AntigravityHookOutput {
    match ancestry {
        HostAncestry::External => AntigravityHookOutput::allow(EXTERNAL_CLI_ALLOW_REASON),
        HostAncestry::Aioncore | HostAncestry::Unknown => AntigravityHookOutput::deny(MISSING_CALLBACK_REASON),
    }
}

fn current_exe_name() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

fn is_aioncore_process_name(name: &str, self_exe: Option<&str>) -> bool {
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_owned());
    let base_trimmed = base.trim();
    if base_trimmed.is_empty() {
        return false;
    }
    if base_trimmed.eq_ignore_ascii_case("aioncore") || base_trimmed.eq_ignore_ascii_case("aioncore.exe") {
        return true;
    }
    self_exe.is_some_and(|exe| base_trimmed == exe)
}

/// Walk parent links starting from `start_pid` (excluding the hook process
/// itself) to determine whether an AionUi backend process sits above us.
///
/// `lookup` returns `Ok(Some((ppid, name)))` for an inspectable process,
/// `Ok(None)` when the walk hits a privilege/lifetime boundary (e.g. root-owned
/// `launchd`/`login` or an already-reaped shell), and `Err(())` on unexpected
/// inspection failure (which fails closed as `Unknown`).
fn detect_host_ancestry_with<F>(start_pid: u32, self_exe: Option<&str>, mut lookup: F) -> HostAncestry
where
    F: FnMut(u32) -> Result<Option<(u32, String)>, ()>,
{
    let mut pid = start_pid;
    let mut visited = HashSet::new();

    for _ in 0..MAX_ANCESTOR_DEPTH {
        if pid <= 1 {
            return HostAncestry::External;
        }
        if !visited.insert(pid) {
            return HostAncestry::Unknown;
        }
        let Some((ppid, name)) = (match lookup(pid) {
            Ok(info) => info,
            Err(()) => return HostAncestry::Unknown,
        }) else {
            return HostAncestry::External;
        };
        if is_aioncore_process_name(&name, self_exe) {
            return HostAncestry::Aioncore;
        }
        pid = ppid;
    }

    HostAncestry::Unknown
}

fn detect_host_ancestry() -> HostAncestry {
    let self_exe = current_exe_name();
    platform_detect_host_ancestry(self_exe.as_deref())
}

#[cfg(target_os = "macos")]
fn platform_detect_host_ancestry(self_exe: Option<&str>) -> HostAncestry {
    // Start from the parent of this hook process so `aioncore antigravity-hook`
    // does not match itself.
    let ppid = unsafe { libc::getppid() };
    let Ok(start_pid) = u32::try_from(ppid) else {
        return HostAncestry::Unknown;
    };
    detect_host_ancestry_with(start_pid, self_exe, macos_lookup_process)
}

#[cfg(target_os = "macos")]
fn macos_lookup_process(pid: u32) -> Result<Option<(u32, String)>, ()> {
    let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
        return Err(());
    };
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            raw_pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if got != size {
        let err = std::io::Error::last_os_error().raw_os_error();
        return match err {
            // Root-owned ancestors (login, launchd) or already-exited wrappers
            // cannot belong to a live user-owned AionUi backend.
            Some(libc::EPERM | libc::ESRCH) => Ok(None),
            _ => Err(()),
        };
    }

    // Prefer full executable path so binaries longer than MAXCOMLEN (e.g. custom
    // release names) are never truncated, falling back to pbi_name / pbi_comm.
    let mut path_buf = [0u8; 4096];
    let path_len = unsafe { libc::proc_pidpath(raw_pid, path_buf.as_mut_ptr().cast(), path_buf.len() as u32) };
    let name = if path_len > 0 {
        let len = (path_len as usize).min(path_buf.len());
        String::from_utf8_lossy(&path_buf[..len]).into_owned()
    } else {
        let comm = c_char_array_to_string(&info.pbi_name);
        if comm.is_empty() {
            c_char_array_to_string(&info.pbi_comm)
        } else {
            comm
        }
    };

    Ok(Some((info.pbi_ppid, name)))
}

#[cfg(target_os = "macos")]
fn c_char_array_to_string(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(target_os = "linux")]
fn platform_detect_host_ancestry(self_exe: Option<&str>) -> HostAncestry {
    let ppid = unsafe { libc::getppid() };
    let Ok(start_pid) = u32::try_from(ppid) else {
        return HostAncestry::Unknown;
    };
    detect_host_ancestry_with(start_pid, self_exe, linux_lookup_process)
}

#[cfg(target_os = "linux")]
fn linux_lookup_process(pid: u32) -> Result<Option<(u32, String)>, ()> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(None);
        }
        Err(_) => return Err(()),
    };
    let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return Err(());
    };
    let Some(ppid) = after.split_whitespace().nth(1).and_then(|v| v.parse::<u32>().ok()) else {
        return Err(());
    };
    let exe_or_comm = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|c| c.trim().to_owned())
        })
        .unwrap_or_default();
    Ok(Some((ppid, exe_or_comm)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_detect_host_ancestry(_self_exe: Option<&str>) -> HostAncestry {
    HostAncestry::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::AntigravityHookDecision;

    #[tokio::test]
    async fn unconfigured_bridge_denies_instead_of_allowing() {
        // Safety property: a hook that cannot reach AionUi must never let the
        // tool through.
        unsafe {
            std::env::remove_var(AntigravityHookConfig::ENV_BASE_URL);
        }
        let out = AntigravityHookOutput::deny("unconfigured");
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
    }

    /// The bug this split exists for: a refusal used to be reported as a parse
    /// failure. Live 2026-08-17, while CSRF was blocking this endpoint, the user
    /// saw `aionui returned an unreadable decision: error decoding response
    /// body` — which points at the body when the problem was the status.
    #[test]
    fn a_refusal_is_named_as_one_not_as_an_unreadable_body() {
        let out = decision_from_response(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"success":false,"error":"CSRF token validation failed","code":"CSRF_INVALID"}"#,
        );
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
        let reason = serde_json::to_value(&out).unwrap()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            reason.contains("403") && reason.contains("CSRF_INVALID"),
            "the reason must name the status and the code, not the parser: {reason}"
        );
        assert!(
            !reason.contains("unreadable"),
            "a refused request is not an unreadable one: {reason}"
        );
    }

    /// Not every rejection carries our error envelope — a proxy or a crash page
    /// will not. The status alone still has to reach the user.
    #[test]
    fn a_refusal_without_our_error_envelope_still_names_the_status() {
        let out = decision_from_response(reqwest::StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>");
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
        let reason = serde_json::to_value(&out).unwrap()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(reason.contains("502"), "reason must still name the status: {reason}");
    }

    /// The branch that keeps its old message: a 2xx whose body really is
    /// unparseable. Here "unreadable" is the truth.
    #[test]
    fn an_unparseable_success_body_is_still_reported_as_unreadable() {
        let out = decision_from_response(reqwest::StatusCode::OK, "not json");
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
        let reason = serde_json::to_value(&out).unwrap()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(reason.contains("unreadable"), "reason: {reason}");
    }

    /// The happy path must still get through, or this change would deny
    /// everything and look like a fix.
    #[test]
    fn an_approval_is_passed_through_unchanged() {
        let out = decision_from_response(reqwest::StatusCode::OK, r#"{"decision":"allow"}"#);
        assert_eq!(out.decision, AntigravityHookDecision::Allow);
    }

    #[test]
    fn deny_output_serializes_to_agys_vocabulary() {
        let json = serde_json::to_value(AntigravityHookOutput::deny("nope")).unwrap();
        assert_eq!(json["decision"], "deny");
    }

    #[test]
    fn missing_callback_denies_when_aioncore_is_an_ancestor() {
        let ancestry = detect_host_ancestry_with(400, Some("aioncore"), |pid| match pid {
            400 => Ok(Some((300, "/bin/sh".to_owned()))),
            300 => Ok(Some((200, "/Users/x/.local/bin/agy".to_owned()))),
            200 => Ok(Some((
                100,
                "/Users/x/.local/share/yi-work/backend/current/aioncore".to_owned(),
            ))),
            _ => Ok(None),
        });
        assert_eq!(ancestry, HostAncestry::Aioncore);
        let out = decision_without_callback(ancestry);
        assert_eq!(out.decision, AntigravityHookDecision::Deny);
        assert_eq!(out.reason.as_deref(), Some(MISSING_CALLBACK_REASON));
    }

    #[test]
    fn missing_callback_denies_when_custom_packaged_backend_is_an_ancestor() {
        let ancestry = detect_host_ancestry_with(300, Some("backend"), |pid| match pid {
            300 => Ok(Some((200, "agy".to_owned()))),
            200 => Ok(Some((1, "/Apps/Aion UI/backend".to_owned()))),
            _ => Ok(None),
        });
        assert_eq!(ancestry, HostAncestry::Aioncore);
        assert_eq!(
            decision_without_callback(ancestry).decision,
            AntigravityHookDecision::Deny
        );
    }

    #[test]
    fn missing_callback_allows_when_launched_from_external_terminal() {
        let ancestry = detect_host_ancestry_with(500, Some("aioncore"), |pid| match pid {
            500 => Ok(Some((400, "/Users/x/.local/bin/agy".to_owned()))),
            400 => Ok(Some((300, "/bin/zsh".to_owned()))),
            300 => Ok(Some((200, "tmux".to_owned()))),
            200 => Ok(Some((1, "/Applications/Ghostty.app/Contents/MacOS/ghostty".to_owned()))),
            _ => Ok(None),
        });
        assert_eq!(ancestry, HostAncestry::External);
        let out = decision_without_callback(ancestry);
        assert_eq!(out.decision, AntigravityHookDecision::Allow);
        assert_eq!(out.reason.as_deref(), Some(EXTERNAL_CLI_ALLOW_REASON));
    }

    #[test]
    fn missing_callback_allows_when_walk_hits_privilege_boundary() {
        let ancestry = detect_host_ancestry_with(300, Some("aioncore"), |pid| match pid {
            300 => Ok(Some((200, "agy".to_owned()))),
            200 => Ok(None), // e.g. root-owned login/sshd boundary
            _ => Err(()),
        });
        assert_eq!(ancestry, HostAncestry::External);
        assert_eq!(
            decision_without_callback(ancestry).decision,
            AntigravityHookDecision::Allow
        );
    }

    #[test]
    fn missing_callback_denies_when_process_inspection_fails() {
        let ancestry = detect_host_ancestry_with(300, Some("aioncore"), |_| Err(()));
        assert_eq!(ancestry, HostAncestry::Unknown);
        assert_eq!(
            decision_without_callback(ancestry).decision,
            AntigravityHookDecision::Deny
        );
    }

    #[test]
    fn missing_callback_denies_on_pid_cycle() {
        let ancestry = detect_host_ancestry_with(300, Some("aioncore"), |pid| match pid {
            300 => Ok(Some((200, "agy".to_owned()))),
            200 => Ok(Some((300, "zsh".to_owned()))),
            _ => Ok(None),
        });
        assert_eq!(ancestry, HostAncestry::Unknown);
        assert_eq!(
            decision_without_callback(ancestry).decision,
            AntigravityHookDecision::Deny
        );
    }

    #[test]
    fn host_process_name_matching_is_exact_on_basename() {
        assert!(is_aioncore_process_name("aioncore", None));
        assert!(is_aioncore_process_name("/opt/yi/aioncore", None));
        assert!(is_aioncore_process_name("AionCore.exe", None));
        assert!(is_aioncore_process_name("/Apps/Aion UI/backend", Some("backend")));
        assert!(!is_aioncore_process_name("/Users/x/.local/bin/agy", Some("aioncore")));
        assert!(!is_aioncore_process_name("aioncore-helper", Some("aioncore")));
        assert!(!is_aioncore_process_name("", Some("aioncore")));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_process_ancestry_inspection_resolves_cleanly() {
        assert_ne!(platform_detect_host_ancestry(Some("aioncore")), HostAncestry::Unknown);
    }
}
