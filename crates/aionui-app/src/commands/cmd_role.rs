//! `aioncore role` — the agent-facing CLI for the resident role a conversation
//! runs as.
//!
//! Shaped after `cmd_conversation.rs`: the same three runtime headers, an
//! envelope on every failure, and no panic on a malformed env var. The caller
//! never names a role; the backend takes it from the token-bound conversation.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use aionui_api_types::{RoleCliEnvelope, RoleCliErrorCode};
use serde_json::{Map, Value};

use crate::cli::{RoleArgs, RoleCommand};
use crate::commands::role_capabilities::{
    self, CURATE_FIELDS, FOLLOW_UP_FIELDS, Fields, PROGRESS_FIELDS, REMEMBER_FIELDS,
};

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";

pub(crate) async fn run_role(args: RoleArgs) -> ExitCode {
    match run_role_inner(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

async fn run_role_inner(args: RoleArgs) -> Result<(), ExitCode> {
    match args.command {
        // Static contract; needs no runtime env.
        RoleCommand::Capabilities => print_json(&RoleCliEnvelope::success(
            role_capabilities::data(),
            "role capabilities",
        )),
        RoleCommand::Show => show().await,
        RoleCommand::Remember => write("role remember", "/api/runtime/role/remember", &REMEMBER_FIELDS).await,
        RoleCommand::Curate => write("role curate", "/api/runtime/role/curate", &CURATE_FIELDS).await,
        RoleCommand::Progress => write("role progress", "/api/runtime/role/progress", &PROGRESS_FIELDS).await,
        RoleCommand::FollowUp => write("role follow-up", "/api/runtime/role/follow-up", &FOLLOW_UP_FIELDS).await,
        RoleCommand::Unknown(path) => Err(unknown_command(path)),
    }
}

async fn show() -> Result<(), ExitCode> {
    let command = "role show";
    let env = runtime_env(command)?;
    let response = reqwest::Client::new()
        .get(env.url("/api/runtime/role"))
        .headers(env.headers(command)?)
        .send()
        .await
        .map_err(|error| transport_error(command, "ROLE_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

async fn write(command: &str, path: &str, fields: &Fields) -> Result<(), ExitCode> {
    let env = runtime_env(command)?;
    let body = read_stdin_fields(command, fields)?;
    let response = reqwest::Client::new()
        .post(env.url(path))
        .headers(env.headers(command)?)
        .json(&body)
        .send()
        .await
        .map_err(|error| transport_error(command, "ROLE_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

struct RuntimeEnv {
    base_url: String,
    user_id: String,
    conversation_id: String,
    runtime_token: String,
}

impl RuntimeEnv {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// Fallible on purpose: a malformed `AIONUI_*` value must come back as an
    /// envelope, not a panic (same reasoning as `cmd_session.rs`).
    fn headers(&self, command: &str) -> Result<reqwest::header::HeaderMap, ExitCode> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("x-aionui-user-id", &self.user_id),
            ("x-aionui-conversation-id", &self.conversation_id),
            ("x-aionui-runtime-token", &self.runtime_token),
        ] {
            let parsed = value.parse().map_err(|_| {
                // The NAME only — a token must never reach stdout or stderr.
                transport_error(
                    command,
                    "ROLE_CLI_HEADER_INVALID",
                    format!("environment variable for {name} is not a valid header value"),
                )
            })?;
            headers.insert(name, parsed);
        }
        Ok(headers)
    }
}

fn runtime_env(command: &str) -> Result<RuntimeEnv, ExitCode> {
    Ok(RuntimeEnv {
        base_url: required_env(command, ENV_BASE_URL)?,
        user_id: required_env(command, ENV_USER_ID)?,
        conversation_id: required_env(command, ENV_CONVERSATION_ID)?,
        runtime_token: required_env(command, ENV_RUNTIME_TOKEN)?,
    })
}

fn required_env(command: &str, name: &'static str) -> Result<String, ExitCode> {
    std::env::var(name).map_err(|_| {
        transport_error(
            command,
            "ROLE_CLI_ENV_MISSING",
            format!("missing required environment variable: {name}"),
        )
    })
}

fn read_stdin_fields(command: &str, fields: &Fields) -> Result<Value, ExitCode> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| schema_error(command, "ROLE_CLI_STDIN_READ_FAILED", error.to_string()))?;
    let value = if input.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(&input)
            .map_err(|error| schema_error(command, "ROLE_CLI_STDIN_JSON_INVALID", error.to_string()))?
    };
    checked_fields(fields, value).map_err(|message| schema_error(command, "ROLE_CLI_SCHEMA_VALIDATION_FAILED", message))
}

/// Check stdin against the command's fields before spending a round trip, so a
/// typo comes back as `schema_validation_failed` locally.
fn checked_fields(fields: &Fields, value: Value) -> Result<Value, String> {
    let Some(object) = value.as_object() else {
        return Err("stdin JSON must be an object".to_owned());
    };
    for (key, field) in object {
        if !fields.required.contains(&key.as_str()) && !fields.optional.contains(&key.as_str()) {
            return Err(format!("unknown stdin field: {key}"));
        }
        if fields.whole_numbers.contains(&key.as_str()) {
            if !field.is_u64() {
                return Err(format!("stdin field must be a whole number: {key}"));
            }
        } else if !field.is_string() {
            return Err(format!("stdin field must be a string: {key}"));
        }
    }
    for key in fields.required {
        if !object.contains_key(*key) {
            return Err(format!("missing required stdin field: {key}"));
        }
    }
    Ok(value)
}

async fn print_response(command: &str, response: reqwest::Response) -> Result<(), ExitCode> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| transport_error(command, "ROLE_CLI_HTTP_RESPONSE_FAILED", error.to_string()))?;
    if !status.is_success() {
        eprintln!(
            "ROLE_CLI_HTTP_STATUS_ERROR command={command} status={status}: runtime bridge returned non-success status"
        );
        // The body is already an envelope carrying a stable error code.
        println!("{text}");
        return Err(ExitCode::from(3));
    }
    println!("{text}");
    Ok(())
}

fn transport_error(command: &str, stderr_code: &'static str, message: String) -> ExitCode {
    print_failure(command, stderr_code, RoleCliErrorCode::TransportUnavailable, message)
}

fn schema_error(command: &str, stderr_code: &'static str, message: String) -> ExitCode {
    print_failure(command, stderr_code, RoleCliErrorCode::SchemaValidationFailed, message)
}

fn unknown_command(path: Vec<OsString>) -> ExitCode {
    let suffix = path
        .into_iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let command = if suffix.is_empty() {
        "role".to_owned()
    } else {
        format!("role {suffix}")
    };
    schema_error(&command, "ROLE_CLI_UNKNOWN_COMMAND", "unknown role command".to_owned())
}

fn print_failure(command: &str, stderr_code: &'static str, code: RoleCliErrorCode, message: String) -> ExitCode {
    eprintln!("{stderr_code} command={command}: {message}");
    let _ = print_json(&RoleCliEnvelope::<Value>::failure(code, message, command));
    ExitCode::from(2)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), ExitCode> {
    let rendered = serde_json::to_string_pretty(value).map_err(|_| ExitCode::from(1))?;
    let mut stdout = io::stdout();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| ExitCode::from(1))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn env(user_id: &str, conversation_id: &str, runtime_token: &str) -> RuntimeEnv {
        RuntimeEnv {
            base_url: "http://127.0.0.1:1/".to_owned(),
            user_id: user_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            runtime_token: runtime_token.to_owned(),
        }
    }

    #[test]
    fn well_formed_runtime_env_produces_all_three_headers() {
        let env = env("user_1", "conv_1", "tok_1");
        let headers = env.headers("role show").unwrap();
        assert_eq!(headers.get("x-aionui-user-id").unwrap(), "user_1");
        assert_eq!(headers.get("x-aionui-conversation-id").unwrap(), "conv_1");
        assert_eq!(headers.get("x-aionui-runtime-token").unwrap(), "tok_1");
        assert_eq!(env.url("/api/runtime/role"), "http://127.0.0.1:1/api/runtime/role");
    }

    #[test]
    fn a_malformed_env_value_yields_an_exit_code_instead_of_panicking() {
        for broken in ["bad\nvalue", "bad\rvalue", "bad\0value"] {
            assert!(env(broken, "conv_1", "tok_1").headers("role show").is_err());
            assert!(env("user_1", broken, "tok_1").headers("role show").is_err());
            assert!(env("user_1", "conv_1", broken).headers("role show").is_err());
        }
    }

    #[test]
    fn remember_takes_a_note_and_nothing_else() {
        assert!(checked_fields(&REMEMBER_FIELDS, json!({ "note": "the user reads on a phone" })).is_ok());
        assert!(checked_fields(&REMEMBER_FIELDS, json!({})).is_err());
        assert!(checked_fields(&REMEMBER_FIELDS, json!({ "note": "x", "role": "other" })).is_err());
        assert!(checked_fields(&REMEMBER_FIELDS, json!({ "note": 3 })).is_err());
        assert!(checked_fields(&REMEMBER_FIELDS, json!(["note"])).is_err());
    }

    #[test]
    fn curate_takes_the_whole_memory() {
        assert!(checked_fields(&CURATE_FIELDS, json!({ "memory": "- uses pnpm\n- reads on a phone\n" })).is_ok());
        assert!(checked_fields(&CURATE_FIELDS, json!({ "note": "x" })).is_err());
        assert!(checked_fields(&CURATE_FIELDS, json!({})).is_err());
    }

    #[test]
    fn follow_up_takes_a_note_and_a_whole_number_of_minutes() {
        assert!(
            checked_fields(
                &FOLLOW_UP_FIELDS,
                json!({ "note": "check the deploy", "after_minutes": 30 })
            )
            .is_ok()
        );
        assert!(checked_fields(&FOLLOW_UP_FIELDS, json!({ "note": "check the deploy" })).is_err());
        assert!(checked_fields(&FOLLOW_UP_FIELDS, json!({ "note": "x", "after_minutes": "30" })).is_err());
        assert!(checked_fields(&FOLLOW_UP_FIELDS, json!({ "note": "x", "after_minutes": 1.5 })).is_err());
        assert!(checked_fields(&FOLLOW_UP_FIELDS, json!({ "note": "x", "after_minutes": -5 })).is_err());
    }

    #[test]
    fn progress_takes_a_task_and_may_leave_the_state_out_to_close_it() {
        assert!(checked_fields(&PROGRESS_FIELDS, json!({ "task": "weekly-report", "state": "drafted" })).is_ok());
        assert!(checked_fields(&PROGRESS_FIELDS, json!({ "task": "weekly-report" })).is_ok());
        assert!(checked_fields(&PROGRESS_FIELDS, json!({ "state": "drafted" })).is_err());
    }
}
