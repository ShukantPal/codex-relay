//! Synchronous, allowlisted `jules` execution inside the Mac's login session.
//!
//! The relay runs as a LaunchAgent in the GUI session, so a `jules` child
//! process inherits the keychain access that SSH sessions are denied. This
//! module exposes exactly one binary (the jules CLI) and a small subcommand
//! allowlist over `POST /v1/jules`. There is no shell involved and no other
//! program can be executed through it. `login` and `logout` are deliberately
//! outside the allowlist: the bridge must never change the CLI's auth state.

use relay_core::Json;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_JULES_BIN: &str = "/Users/shukant/.npm-global/bin/jules";
/// Upper bound for one synchronous execution; the HTTP client must allow a
/// slightly larger read timeout.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(300);
const OUTPUT_CAP: usize = 1024 * 1024;
const MAX_ARGS: usize = 64;
const MAX_ARG_LEN: usize = 16384;
const MAX_ID_LEN: usize = 128;

pub struct ExecRequest {
    pub id: String,
    pub args: Vec<String>,
}

pub struct ExecResult {
    pub id: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

impl ExecResult {
    pub fn to_json(&self) -> Json {
        Json::Object(vec![
            ("id".to_owned(), Json::String(self.id.clone())),
            (
                "exit_code".to_owned(),
                match self.exit_code {
                    Some(code) => Json::Number(code.to_string()),
                    None => Json::Null,
                },
            ),
            ("stdout".to_owned(), Json::String(self.stdout.clone())),
            ("stderr".to_owned(), Json::String(self.stderr.clone())),
            ("truncated".to_owned(), Json::Bool(self.truncated)),
            ("timed_out".to_owned(), Json::Bool(self.timed_out)),
        ])
    }
}

pub fn parse_request(body: &Json) -> Result<ExecRequest, String> {
    let id = body
        .object("id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_ID_LEN)
        .ok_or_else(|| "body must include a non-empty id of at most 128 bytes".to_owned())?;
    let args = match body.object("args") {
        Some(Json::Array(args)) => args
            .iter()
            .map(|arg| arg.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>(),
        _ => None,
    }
    .ok_or_else(|| "body must include args as an array of strings".to_owned())?;
    validate(&args)?;
    Ok(ExecRequest {
        id: id.to_owned(),
        args,
    })
}

/// Only the subcommands the Jules workflow needs are reachable. Everything
/// else, including `login` and `logout`, is rejected.
pub fn validate(args: &[String]) -> Result<(), String> {
    if args.is_empty() || args.len() > MAX_ARGS {
        return Err("args must contain between 1 and 64 entries".to_owned());
    }
    for arg in args {
        if arg.is_empty() || arg.len() > MAX_ARG_LEN {
            return Err("each arg must be between 1 and 16384 bytes".to_owned());
        }
    }
    match args[0].as_str() {
        "new" => Ok(()),
        "remote" => match args.get(1).map(String::as_str) {
            Some("list" | "pull" | "new") => Ok(()),
            _ => Err("remote requires list, pull, or new as its subcommand".to_owned()),
        },
        _ => Err("only the new and remote subcommands may be executed".to_owned()),
    }
}

pub fn run(bin: &str, request: ExecRequest) -> ExecResult {
    run_with_timeout(bin, request, EXEC_TIMEOUT)
}

fn run_with_timeout(bin: &str, request: ExecRequest, timeout: Duration) -> ExecResult {
    let mut child = match Command::new(bin)
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ExecResult {
                id: request.id,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("could not spawn {bin}: {error}"),
                truncated: false,
                timed_out: false,
            };
        }
    };
    // Drain both pipes on separate threads so a chatty child can never block
    // on a full pipe while the parent is waiting for it to exit.
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(|| drain_limited(stdout));
    let stderr_reader = thread::spawn(|| drain_limited(stderr));
    let start = Instant::now();
    let mut exit_code = None;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break false;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break false,
        }
    };
    let (stdout, stdout_truncated) = stdout_reader.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader.join().unwrap_or_default();
    ExecResult {
        id: request.id,
        exit_code: if timed_out { None } else { exit_code },
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: stdout_truncated || stderr_truncated,
        timed_out,
    }
}

fn drain_limited(mut pipe: impl Read) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                // Past the cap, keep draining into the void so the child is
                // never blocked on a full pipe.
                if kept.len() < OUTPUT_CAP {
                    let room = OUTPUT_CAP - kept.len();
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                    if n > room {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (kept, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn allowlist_accepts_the_workflow_subcommands() {
        assert!(validate(&args(&["remote", "list", "--repo"])).is_ok());
        assert!(validate(&args(&["remote", "list", "--session"])).is_ok());
        assert!(validate(&args(&["remote", "pull", "--session", "123"])).is_ok());
        assert!(validate(&args(&["remote", "new", "--repo", "a/b"])).is_ok());
        assert!(validate(&args(&["new", "--repo", "a/b", "do the thing"])).is_ok());
    }

    #[test]
    fn allowlist_rejects_everything_else() {
        assert!(validate(&[]).is_err());
        assert!(validate(&args(&["login"])).is_err());
        assert!(validate(&args(&["logout"])).is_err());
        assert!(validate(&args(&["remote"])).is_err());
        assert!(validate(&args(&["remote", "delete"])).is_err());
        assert!(validate(&args(&["version"])).is_err());
        assert!(validate(&args(&["--help"])).is_err());
        assert!(validate(&args(&[""])).is_err());
        assert!(validate(&args(&["new", &"x".repeat(MAX_ARG_LEN + 1)])).is_err());
    }

    #[test]
    fn request_parsing_requires_an_id_and_string_args() {
        let body = Json::Object(vec![
            ("id".to_owned(), Json::String("req-1".to_owned())),
            (
                "args".to_owned(),
                Json::Array(vec![
                    Json::String("remote".to_owned()),
                    Json::String("list".to_owned()),
                    Json::String("--repo".to_owned()),
                ]),
            ),
        ]);
        let request = parse_request(&body).unwrap();
        assert_eq!(request.id, "req-1");
        assert_eq!(request.args.len(), 3);
        assert!(parse_request(&Json::Object(vec![])).is_err());
        let no_id = Json::Object(vec![(
            "args".to_owned(),
            Json::Array(vec![Json::String("new".to_owned())]),
        )]);
        assert!(parse_request(&no_id).is_err());
        let bad_args = Json::Object(vec![
            ("id".to_owned(), Json::String("req-1".to_owned())),
            (
                "args".to_owned(),
                Json::Array(vec![Json::Number("7".to_owned())]),
            ),
        ]);
        assert!(parse_request(&bad_args).is_err());
        let disallowed = Json::Object(vec![
            ("id".to_owned(), Json::String("req-1".to_owned())),
            (
                "args".to_owned(),
                Json::Array(vec![Json::String("login".to_owned())]),
            ),
        ]);
        assert!(parse_request(&disallowed).is_err());
    }

    #[test]
    fn execution_captures_output_and_exit_code() {
        let request = ExecRequest {
            id: "echo-1".to_owned(),
            args: args(&["new"]),
        };
        let result = run_with_timeout("/bin/echo", request, Duration::from_secs(10));
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, "new\n");
        assert!(!result.timed_out);
        assert!(!result.truncated);
    }

    #[test]
    fn execution_reports_a_missing_binary_without_panicking() {
        let request = ExecRequest {
            id: "missing-1".to_owned(),
            args: args(&["new"]),
        };
        let result = run_with_timeout(
            "/nonexistent/jules-test-binary",
            request,
            Duration::from_secs(10),
        );
        assert_eq!(result.exit_code, None);
        assert!(result.stderr.contains("could not spawn"));
        assert!(!result.timed_out);
    }

    #[test]
    fn execution_kills_a_child_that_outlives_the_timeout() {
        let request = ExecRequest {
            id: "sleep-1".to_owned(),
            args: args(&["30"]),
        };
        let start = Instant::now();
        let result = run_with_timeout("/bin/sleep", request, Duration::from_millis(300));
        assert!(result.timed_out);
        assert_eq!(result.exit_code, None);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn drain_limited_caps_output_but_keeps_draining() {
        let big: Vec<u8> = (0..OUTPUT_CAP + 100).map(|i| (i % 251) as u8).collect();
        let (kept, truncated) = drain_limited(&big[..]);
        assert_eq!(kept.len(), OUTPUT_CAP);
        assert!(truncated);
        let (kept, truncated) = drain_limited(&b"hello"[..]);
        assert_eq!(kept, b"hello");
        assert!(!truncated);
    }
}
