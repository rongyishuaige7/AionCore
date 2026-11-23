//! Host-side auto-approval policy for Antigravity (`agy`) `PreToolUse` callbacks.
//!
//! Because agy runs with `--dangerously-skip-permissions` and `hooks.json`
//! matches every tool (`"*"`), this module is the single gatekeeper that
//! translates AionUi's permission modes (`yolo`, `accept-edits`, `plan`,
//! `default`) into concrete auto-approval decisions before falling back to an
//! interactive permission card (which agy will abandon after ~30s).
//!
//! Safety boundary:
//! - Only `yolo` auto-approves arbitrary tools and commands.
//! - Read-only inspection tools and strictly verified read-only shell commands
//!   are auto-approved across all modes unless they target sensitive credential
//!   paths (`.ssh`, `.aws`, `.gnupg`, `.env`, private keys, etc.).
//! - `accept-edits` additionally auto-approves file-editing tools (again
//!   excluding sensitive credential paths), while non-read-only commands still
//!   require approval.
//! - Anything ambiguous, malformed, or outside the explicit allowlists returns
//!   `None` so the caller falls back to the interactive permission card.

use serde_json::Value;

use super::argv::FULL_AUTO_SENTINEL;

const ACCEPT_EDITS_MODE: &str = "accept-edits";

/// Tools that never read local files or mutate host state.
const ALWAYS_READ_ONLY_TOOLS: &[&str] = &[
    "search_web",
    "read_url_content",
    "list_resources",
    "read_resource",
    "get_project_info",
];

/// Local file/code inspection tools. Auto-approved unless their arguments point
/// at a sensitive credential path.
const LOCAL_READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "view_file",
    "view_file_outline",
    "view_code_item",
    "list_dir",
    "list_directory",
    "find_by_name",
    "grep_search",
    "codebase_search",
];

/// File-editing tools auto-approved in `accept-edits` mode (excluding sensitive
/// credential paths).
const FILE_EDIT_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "multi_edit",
    "replace_file_content",
    "multi_replace_file_content",
    "write_to_file",
    "create_file",
    "patch_file",
    "apply_patch",
];

/// Shell programs that are inherently read-only (no built-in file mutation or
/// command execution flags in normal use).
const SAFE_READ_ONLY_PROGRAMS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "egrep", "fgrep", "rg", "cut", "tr", "diff", "file", "stat", "du", "df",
    "pwd", "echo", "which", "nl", "basename", "dirname", "realpath", "readlink", "uname", "date", "id", "whoami", "ps",
    "jq",
];

/// Decide whether an Antigravity tool call should be auto-approved under `mode`.
///
/// Returns `Some(reason)` when the call may proceed immediately without raising
/// a permission card, or `None` when the user (or a session-level `AllowAlways`
/// entry) must approve it.
pub(super) fn auto_approve_reason(mode: Option<&str>, tool_name: &str, input: &Value) -> Option<&'static str> {
    if mode.is_some_and(|m| m.eq_ignore_ascii_case(FULL_AUTO_SENTINEL)) {
        return Some("full-auto");
    }

    let tool = tool_name.trim();
    if is_read_only_tool(tool, input) {
        return Some("read-only tool");
    }

    if tool == "run_command" && is_read_only_shell_input(input) {
        return Some("read-only shell command");
    }

    if mode.is_some_and(|m| m.eq_ignore_ascii_case(ACCEPT_EDITS_MODE))
        && FILE_EDIT_TOOLS.contains(&tool)
        && !input_touches_sensitive_path(input)
    {
        return Some("accept-edits mode");
    }

    None
}

fn is_read_only_tool(tool: &str, input: &Value) -> bool {
    if ALWAYS_READ_ONLY_TOOLS.contains(&tool) {
        return true;
    }

    if LOCAL_READ_ONLY_TOOLS.contains(&tool) {
        return !input_touches_sensitive_path(input);
    }

    match tool {
        "manage_task" => action_field(input).is_some_and(|a| matches!(a, "list" | "status")),
        "manage_subagents" => action_field(input).is_some_and(|a| a == "list"),
        _ => false,
    }
}

fn action_field(input: &Value) -> Option<&str> {
    input
        .get("Action")
        .or_else(|| input.get("action"))
        .and_then(Value::as_str)
        .map(str::trim)
}

/// Returns `true` if any string value in `input` looks like a sensitive
/// credential or secret file path.
pub(super) fn input_touches_sensitive_path(value: &Value) -> bool {
    match value {
        Value::String(s) => is_sensitive_path(s),
        Value::Array(items) => items.iter().any(input_touches_sensitive_path),
        Value::Object(map) => map.values().any(input_touches_sensitive_path),
        _ => false,
    }
}

fn is_sensitive_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let normalized = trimmed.replace('\\', "/").to_ascii_lowercase();
    let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();

    if segments.iter().any(|s| matches!(*s, ".ssh" | ".aws" | ".gnupg")) {
        return true;
    }

    let Some(file_name) = segments.last().copied() else {
        return false;
    };

    if matches!(
        file_name,
        ".netrc"
            | "id_rsa"
            | "id_ed25519"
            | "id_ecdsa"
            | "id_dsa"
            | "credentials.json"
            | ".npmrc"
            | ".pypirc"
            | "secrets.json"
            | "secrets.yaml"
            | "secrets.yml"
    ) {
        return true;
    }

    if file_name == ".env" || file_name.starts_with(".env.") {
        return !matches!(
            file_name,
            ".env.example" | ".env.sample" | ".env.template" | ".env.dist"
        );
    }

    false
}

fn is_read_only_shell_input(input: &Value) -> bool {
    let Some(command) = extract_command_line(input) else {
        return false;
    };
    if let Some(cwd) = input.get("Cwd").or_else(|| input.get("cwd")).and_then(Value::as_str)
        && is_sensitive_path(cwd)
    {
        return false;
    }
    is_read_only_shell_command(command)
}

fn extract_command_line(input: &Value) -> Option<&str> {
    input
        .get("CommandLine")
        .or_else(|| input.get("command"))
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Strictly verify that a shell command line consists exclusively of safe
/// read-only commands joined by `&&` or `|`, with no redirections, command
/// substitutions, subshells, backgrounding, or sensitive path accesses.
pub(super) fn is_read_only_shell_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || command.contains('\n') || command.contains('\r') {
        return false;
    }

    let Some(segments) = split_safe_segments(command) else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }

    let mut has_non_cd_command = false;
    for seg in segments {
        let Some(tokens) = tokenize_shell_segment(&seg) else {
            return false;
        };
        if tokens.is_empty() {
            return false;
        }
        if tokens.iter().any(|t| is_sensitive_path(t)) {
            return false;
        }
        // Reject leading environment assignments (`FOO=bar cmd`).
        if tokens[0].contains('=') {
            return false;
        }
        let program = program_basename(&tokens[0]);
        if program == "cd" {
            // Allow simple `cd <dir>` prefix segments (no flags, single target).
            if tokens.len() != 2 || tokens[1].starts_with('-') {
                return false;
            }
            continue;
        }
        has_non_cd_command = true;
        if !is_safe_read_only_argv(program, &tokens[1..]) {
            return false;
        }
    }

    has_non_cd_command
}

/// Split a command line on unquoted `&&` and `|` while rejecting any dangerous
/// shell metacharacter (`>`, `<`, `;`, `||`, lone `&`, `` ` ``, `$(`, `(`, `)`,
/// `{`, `}`, unclosed quotes).
fn split_safe_segments(command: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            current.push(c);
            i += 1;
            continue;
        }

        if c == '\\' {
            // Preserve escape sequences for the tokenizer; reject trailing lone backslash.
            if i + 1 >= chars.len() {
                return None;
            }
            current.push(c);
            current.push(chars[i + 1]);
            i += 2;
            continue;
        }

        if in_double {
            // Inside double quotes, command substitution (`$(` or `` ` ``) still executes.
            if c == '`' || (c == '$' && chars.get(i + 1) == Some(&'(')) {
                return None;
            }
            if c == '"' {
                in_double = false;
            }
            current.push(c);
            i += 1;
            continue;
        }

        match c {
            '\'' => {
                in_single = true;
                current.push(c);
                i += 1;
            }
            '"' => {
                in_double = true;
                current.push(c);
                i += 1;
            }
            // Disallow redirections, command substitution, subshells, braces, semicolons.
            '>' | '<' | ';' | '`' | '(' | ')' | '{' | '}' => return None,
            '$' if chars.get(i + 1) == Some(&'(') => return None,
            '|' => {
                // Reject `||`; allow single pipe `|`.
                if chars.get(i + 1) == Some(&'|') {
                    return None;
                }
                let trimmed = current.trim();
                if trimmed.is_empty() {
                    return None;
                }
                segments.push(trimmed.to_owned());
                current.clear();
                i += 1;
            }
            '&' => {
                // Allow `&&`; reject lone `&` (background) and `&&&`.
                if chars.get(i + 1) != Some(&'&') || chars.get(i + 2) == Some(&'&') {
                    return None;
                }
                let trimmed = current.trim();
                if trimmed.is_empty() {
                    return None;
                }
                segments.push(trimmed.to_owned());
                current.clear();
                i += 2;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }

    if in_single || in_double {
        return None;
    }

    let trimmed = current.trim();
    if trimmed.is_empty() {
        return None;
    }
    segments.push(trimmed.to_owned());
    Some(segments)
}

fn tokenize_shell_segment(segment: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = segment.chars().peekable();

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                current.push(c);
            }
            continue;
        }
        if in_double {
            match c {
                '"' => in_double = false,
                '\\' => {
                    let next = chars.next()?;
                    current.push(next);
                }
                _ => current.push(c),
            }
            continue;
        }

        match c {
            '\'' => {
                in_single = true;
                has_token = true;
            }
            '"' => {
                in_double = true;
                has_token = true;
            }
            '\\' => {
                let next = chars.next()?;
                current.push(next);
                has_token = true;
            }
            c if c.is_whitespace() => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            _ => {
                current.push(c);
                has_token = true;
            }
        }
    }

    if in_single || in_double {
        return None;
    }
    if has_token {
        tokens.push(current);
    }
    Some(tokens)
}

fn program_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn is_safe_read_only_argv(program: &str, args: &[String]) -> bool {
    if SAFE_READ_ONLY_PROGRAMS.contains(&program) {
        // `rg --pre` can execute an external preprocessor command.
        if program == "rg"
            && args
                .iter()
                .any(|a| a == "--pre" || a.starts_with("--pre=") || a == "--pre-glob")
        {
            return false;
        }
        return true;
    }

    match program {
        "git" => is_safe_read_only_git(args),
        "find" => is_safe_read_only_find(args),
        "sed" => is_safe_read_only_sed(args),
        _ => false,
    }
}

fn is_safe_read_only_git(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-C" | "--git-dir" | "--work-tree" => {
                // Global path flag consumes the next argument.
                i += 2;
            }
            a if a.starts_with("--git-dir=") || a.starts_with("--work-tree=") => {
                i += 1;
            }
            "--no-pager" | "-p" | "--paginate" | "--bare" => {
                i += 1;
            }
            a if a.starts_with('-') => {
                // Unknown global git flag (e.g. `-c core.fsmonitor=...`) could execute code.
                return false;
            }
            _ => break,
        }
    }

    let Some(subcommand) = args.get(i).map(String::as_str) else {
        return false;
    };
    let rest = &args[i + 1..];

    match subcommand {
        "status" | "log" | "diff" | "show" | "rev-parse" | "describe" | "ls-files" | "ls-tree" | "cat-file"
        | "grep" | "blame" | "shortlog" => {
            // Block `--output` / `--ext-diff` style side-effect flags on diff/log/show.
            !rest.iter().any(|a| {
                a == "--output"
                    || a.starts_with("--output=")
                    || a == "--ext-diff"
                    || a == "--textconv"
                    || a == "--open-files-in-pager"
                    || a.starts_with("--open-files-in-pager=")
            })
        }
        "branch" => {
            // Only read-only listing flags allowed on `git branch`.
            rest.iter().all(|a| {
                matches!(
                    a.as_str(),
                    "-a" | "--all"
                        | "-r"
                        | "--remotes"
                        | "-v"
                        | "-vv"
                        | "--verbose"
                        | "--list"
                        | "--show-current"
                        | "--merged"
                        | "--no-merged"
                        | "--contains"
                )
            })
        }
        "remote" => rest.is_empty() || rest.iter().all(|a| matches!(a.as_str(), "-v" | "--verbose" | "show")),
        "worktree" => rest.first().map(String::as_str) == Some("list"),
        "tag" => rest.iter().all(|a| matches!(a.as_str(), "-l" | "--list" | "-n")),
        _ => false,
    }
}

fn is_safe_read_only_find(args: &[String]) -> bool {
    !args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fprint" | "-fprint0" | "-fprintf" | "-fls"
        )
    })
}

/// Allow strictly read-only `sed -n '<addr>p' <file...>` slices (no `-i`, no
/// arbitrary sed scripts that could invoke `w` or `e`).
fn is_safe_read_only_sed(args: &[String]) -> bool {
    if args.len() < 2 || args[0] != "-n" {
        return false;
    }
    let script = args[1].as_str();
    // Accept simple line/range print expressions like `1,100p`, `42p`, `10,$p`.
    let Some(addr) = script.strip_suffix('p') else {
        return false;
    };
    if addr.is_empty() {
        return false;
    }
    let valid_addr = addr
        .split(',')
        .all(|part| part == "$" || (!part.is_empty() && part.chars().all(|c| c.is_ascii_digit())));
    if !valid_addr || addr.split(',').count() > 2 {
        return false;
    }
    // Remaining arguments must be plain file paths (no flags like `-i`, `-e`, `-f`).
    args[2..].iter().all(|a| !a.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_auto_approves_everything() {
        assert_eq!(
            auto_approve_reason(Some("yolo"), "run_command", &json!({"CommandLine": "rm -rf /tmp/x"})),
            Some("full-auto")
        );
        assert_eq!(
            auto_approve_reason(Some("YOLO"), "write_file", &json!({"Path": "/Users/x/.ssh/id_rsa"})),
            Some("full-auto")
        );
    }

    #[test]
    fn read_only_tools_are_auto_approved_in_default_plan_and_accept_edits() {
        for mode in [None, Some("default"), Some("plan"), Some("accept-edits")] {
            assert_eq!(
                auto_approve_reason(mode, "read_file", &json!({"Path": "/Users/x/Desktop/notes.md"})),
                Some("read-only tool"),
                "read_file should auto-approve in mode {mode:?}"
            );
            assert_eq!(
                auto_approve_reason(mode, "search_web", &json!({"query": "rust tokio timeout"})),
                Some("read-only tool")
            );
            assert_eq!(
                auto_approve_reason(mode, "grep_search", &json!({"Path": "/Users/x/repo", "Query": "foo"})),
                Some("read-only tool")
            );
            assert_eq!(
                auto_approve_reason(mode, "manage_task", &json!({"Action": "status", "TaskId": "t1"})),
                Some("read-only tool")
            );
            assert_eq!(
                auto_approve_reason(mode, "manage_subagents", &json!({"Action": "list"})),
                Some("read-only tool")
            );
        }
    }

    #[test]
    fn sensitive_credential_paths_are_never_auto_approved_outside_yolo() {
        for path in [
            "/Users/x/.ssh/id_rsa",
            "/Users/x/.ssh/config",
            "/Users/x/.aws/credentials",
            "/Users/x/.gnupg/secring.gpg",
            "/Users/x/project/.env",
            "/Users/x/project/.env.local",
            "/Users/x/.netrc",
            "/Users/x/.npmrc",
        ] {
            assert_eq!(
                auto_approve_reason(Some("default"), "read_file", &json!({"Path": path})),
                None,
                "sensitive path {path} must require interactive approval"
            );
            assert_eq!(
                auto_approve_reason(Some("accept-edits"), "edit_file", &json!({"Path": path})),
                None,
                "editing sensitive path {path} must require interactive approval"
            );
        }

        // Non-secret template env files remain safe to read.
        assert_eq!(
            auto_approve_reason(
                Some("plan"),
                "read_file",
                &json!({"Path": "/Users/x/project/.env.example"})
            ),
            Some("read-only tool")
        );
    }

    #[test]
    fn mutating_task_or_subagent_actions_still_require_approval() {
        assert_eq!(
            auto_approve_reason(
                Some("default"),
                "manage_task",
                &json!({"Action": "kill", "TaskId": "t1"})
            ),
            None
        );
        assert_eq!(
            auto_approve_reason(
                Some("default"),
                "manage_task",
                &json!({"Action": "send_input", "TaskId": "t1", "Input": "y"})
            ),
            None
        );
        assert_eq!(
            auto_approve_reason(Some("default"), "manage_subagents", &json!({"Action": "kill_all"})),
            None
        );
    }

    #[test]
    fn accept_edits_auto_approves_file_edits_but_still_asks_for_mutating_shell() {
        assert_eq!(
            auto_approve_reason(
                Some("accept-edits"),
                "write_file",
                &json!({"Path": "/Users/x/project/src/main.rs"})
            ),
            Some("accept-edits mode")
        );
        assert_eq!(
            auto_approve_reason(
                Some("accept-edits"),
                "edit_file",
                &json!({"Path": "/Users/x/project/src/main.rs"})
            ),
            Some("accept-edits mode")
        );
        // Same edits still require approval in default and plan modes.
        assert_eq!(
            auto_approve_reason(
                Some("default"),
                "edit_file",
                &json!({"Path": "/Users/x/project/src/main.rs"})
            ),
            None
        );
        assert_eq!(
            auto_approve_reason(
                Some("plan"),
                "write_file",
                &json!({"Path": "/Users/x/project/src/main.rs"})
            ),
            None
        );
        // Non-read-only commands still ask in accept-edits.
        assert_eq!(
            auto_approve_reason(
                Some("accept-edits"),
                "run_command",
                &json!({"CommandLine": "cargo test"})
            ),
            None
        );
    }

    #[test]
    fn read_only_shell_commands_are_auto_approved() {
        for cmd in [
            "ls -la /Users/x/project",
            "git status",
            "git -C /Users/x/project log -n 5 --oneline",
            "cd /Users/x/project && rg -n 'PermissionDecision' crates/ | head -n 40",
            "find /Users/x/project/src -name '*.rs' | wc -l",
            "sed -n '1,120p' /Users/x/project/src/lib.rs",
            "git worktree list",
            "git branch -vv",
            "cat /Users/x/project/Cargo.toml | grep version",
        ] {
            assert_eq!(
                auto_approve_reason(Some("plan"), "run_command", &json!({"CommandLine": cmd})),
                Some("read-only shell command"),
                "expected read-only command to auto-approve: {cmd}"
            );
        }
    }

    #[test]
    fn dangerous_or_ambiguous_shell_commands_require_approval() {
        for cmd in [
            "",
            "rm -rf /tmp/foo",
            "echo hi > /tmp/out.txt",
            "cat a.txt >> b.txt",
            "echo $(whoami)",
            "echo `whoami`",
            "ls; rm -rf /tmp/x",
            "ls || rm -rf /tmp/x",
            "ls &",
            "cat /Users/x/.ssh/id_rsa",
            "cat /Users/x/project/.env",
            "git commit -m 'x'",
            "git push origin main",
            "git branch -D feat",
            "git -c core.fsmonitor=evil status",
            "find . -delete",
            "find . -exec rm {} \\;",
            "sed -i 's/a/b/g' foo.txt",
            "sed -n '1w /tmp/out' foo.txt",
            "rg --pre sh foo .",
            "python3 -c 'print(1)'",
            "cd /tmp",
            "FOO=bar ls",
            "ls 'unclosed",
            "ls\nrm -rf /tmp/x",
        ] {
            assert_eq!(
                auto_approve_reason(Some("default"), "run_command", &json!({"CommandLine": cmd})),
                None,
                "dangerous/non-read-only command must require approval: {cmd}"
            );
        }
    }

    #[test]
    fn empty_run_command_args_require_approval() {
        assert_eq!(auto_approve_reason(Some("default"), "run_command", &json!({})), None);
    }
}
