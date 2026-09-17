use relay_core::{Json, ReadResult, Store, parse_json, read_secret_file};
use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

mod exec;

const MAX_BODY: usize = 64 * 1024;
#[cfg(any(target_os = "macos", test))]
const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;
#[cfg(any(target_os = "macos", test))]
const SESSION_IS_REMOTE: u32 = 0x1000;

struct Config {
    secret_file: PathBuf,
    state_file: PathBuf,
    port: u16,
    tailscale_ip: Option<IpAddr>,
    max_events: usize,
}
struct Server {
    secret: String,
    store: Store,
}
struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

enum ReadRequestError {
    Message(String),
    ExecDenied,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("relay: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("config") {
        return run_config(&arguments[1..]);
    }
    let config = server_config(arguments)?;
    let secret = read_secret_file(&config.secret_file)?;
    let state = Arc::new(Server {
        secret,
        store: Store::open(config.state_file, config.max_events)?,
    });
    let tailnet = config.tailscale_ip.unwrap_or(resolve_tailscale_ip()?);
    let addresses = [
        SocketAddr::new(IpAddr::from([127, 0, 0, 1]), config.port),
        SocketAddr::new(tailnet, config.port),
    ];
    for address in addresses {
        let listener = TcpListener::bind(address)
            .map_err(|error| format!("could not bind {address}: {error}"))?;
        let state = Arc::clone(&state);
        println!("relay listening on http://{address}");
        thread::spawn(move || serve(listener, state));
    }
    loop {
        thread::park();
    }
}

fn server_config(arguments: Vec<String>) -> Result<Config, String> {
    let mut secret_file = env::var_os("RELAY_SECRET_FILE").map(PathBuf::from);
    let mut state_file = env::var_os("RELAY_STATE_FILE").map(PathBuf::from);
    let mut port = 8765;
    let mut tailscale_ip = None;
    let mut max_events = 1000;
    let mut values = arguments.into_iter();
    while let Some(argument) = values.next() {
        let value = |values: &mut std::vec::IntoIter<String>, name: &str| {
            values
                .next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match argument.as_str() {
            "--secret-file" => secret_file = Some(PathBuf::from(value(&mut values, "--secret-file")?)),
            "--state-file" => state_file = Some(PathBuf::from(value(&mut values, "--state-file")?)),
            "--port" => port = value(&mut values, "--port")?.parse().map_err(|_| "--port must be a valid u16".to_owned())?,
            "--tailscale-ip" => {
                let address = value(&mut values, "--tailscale-ip")?
                    .parse()
                    .map_err(|_| "--tailscale-ip must be an IP address".to_owned())?;
                if !is_tailscale_ipv4(address) {
                    return Err("--tailscale-ip must be a Tailscale IPv4 address".to_owned());
                }
                tailscale_ip = Some(address);
            }
            "--max-events" => max_events = value(&mut values, "--max-events")?.parse().map_err(|_| "--max-events must be a positive integer".to_owned())?,
            "--help" | "-h" => return Err("usage: relay --secret-file PATH --state-file PATH [--port 8765] [--max-events 1000]".to_owned()),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let secret_file =
        secret_file.ok_or_else(|| "--secret-file or RELAY_SECRET_FILE is required".to_owned())?;
    let state_file =
        state_file.ok_or_else(|| "--state-file or RELAY_STATE_FILE is required".to_owned())?;
    if max_events == 0 {
        return Err("--max-events must be greater than zero".to_owned());
    }
    Ok(Config {
        secret_file,
        state_file,
        port,
        tailscale_ip,
        max_events,
    })
}

fn run_config(arguments: &[String]) -> Result<(), String> {
    require_gui_login_session()?;
    let file = allowlist_file(arguments)?;
    let contents = std::fs::read_to_string(file)
        .map_err(|error| format!("could not read allowlist file {file}: {error}"))?;
    let policy = exec::Policy::parse(&contents)?;
    exec::store_policy(&policy)?;
    println!("{}", policy.canonical_json());
    Ok(())
}

fn allowlist_file(arguments: &[String]) -> Result<&str, String> {
    let [command, flag, file] = arguments else {
        return Err("usage: relay config set-allowlist --file PATH".to_owned());
    };
    if command != "set-allowlist" || flag != "--file" || file.is_empty() {
        return Err("usage: relay config set-allowlist --file PATH".to_owned());
    }
    Ok(file)
}

#[cfg(any(target_os = "macos", test))]
fn is_local_gui_session(status: i32, attributes: u32) -> bool {
    status == 0
        && attributes & SESSION_HAS_GRAPHIC_ACCESS != 0
        && attributes & SESSION_IS_REMOTE == 0
}

/// Policy updates are intentionally an owner action from the local Aqua
/// session, never an SSH action. Keychain access alone does not establish
/// which terminal invoked this executable, so check the caller's session too.
#[cfg(target_os = "macos")]
fn require_gui_login_session() -> Result<(), String> {
    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
    }

    let mut session_id = 0;
    let mut attributes = 0;
    // `callerSecuritySession` asks macOS about this process's session.
    let status =
        unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &mut session_id, &mut attributes) };
    if is_local_gui_session(status, attributes) {
        Ok(())
    } else {
        Err("set-allowlist must run from Shukant's local macOS GUI login session".to_owned())
    }
}

#[cfg(not(target_os = "macos"))]
fn require_gui_login_session() -> Result<(), String> {
    Err("set-allowlist must run from Shukant's local macOS GUI login session".to_owned())
}

fn resolve_tailscale_ip() -> Result<IpAddr, String> {
    let output = Command::new("tailscale").args(["ip", "-4"]).output().map_err(|error| format!("could not run tailscale ip -4: {error}; use --tailscale-ip only for explicit test/development overrides"))?;
    if !output.status.success() {
        return Err("tailscale ip -4 failed; relay will not bind broadly".to_owned());
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "tailscale ip -4 produced non-UTF-8 output".to_owned())?;
    stdout
        .lines()
        .next()
        .ok_or_else(|| "tailscale ip -4 returned no address".to_owned())?
        .parse()
        .map_err(|_| "tailscale ip -4 returned an invalid address".to_owned())
}

fn is_tailscale_ipv4(address: IpAddr) -> bool {
    matches!(address, IpAddr::V4(address) if address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
}

fn serve(listener: TcpListener, state: Arc<Server>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let _ = handle(stream, state);
                });
            }
            Err(error) => eprintln!("relay accept error: {error}"),
        }
    }
}

fn handle(mut stream: TcpStream, state: Arc<Server>) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(ReadRequestError::ExecDenied) => {
            denied(&mut stream, "")?;
            return Ok(());
        }
        Err(ReadRequestError::Message(error)) => {
            reply(
                &mut stream,
                400,
                Json::Object(vec![("error".to_owned(), Json::String(error))]),
            )?;
            return Ok(());
        }
    };
    if !authorized(
        request
            .headers
            .get("authorization")
            .map(String::as_str)
            .unwrap_or(""),
        &state.secret,
    ) {
        reply(&mut stream, 401, error("unauthorized"))?;
        return Ok(());
    }
    match (
        request.method.as_str(),
        request.target.split('?').next().unwrap_or(""),
    ) {
        ("POST", "/v1/events") => post(&mut stream, &state, request.body),
        ("GET", "/v1/events") => get(&mut stream, &state, &request.target),
        ("POST", "/v1/exec") => exec_request(&mut stream, request.body),
        _ => reply(&mut stream, 404, error("not_found")),
    }
}

fn post(stream: &mut TcpStream, state: &Server, body: Vec<u8>) -> Result<(), String> {
    let body = match String::from_utf8(body) {
        Ok(body) => body,
        Err(_) => {
            reply(
                stream,
                400,
                error("body_must_be_an_object_with_nonempty_id"),
            )?;
            return Ok(());
        }
    };
    let payload = match parse_json(&body) {
        Ok(Json::Object(fields)) => Json::Object(fields),
        _ => {
            reply(
                stream,
                400,
                error("body_must_be_an_object_with_nonempty_id"),
            )?;
            return Ok(());
        }
    };
    if payload
        .object("id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        reply(
            stream,
            400,
            error("body_must_be_an_object_with_nonempty_id"),
        )?;
        return Ok(());
    }
    match state.store.add(payload) {
        Ok((event, duplicate)) => reply(
            stream,
            if duplicate { 200 } else { 201 },
            Json::Object(vec![
                ("duplicate".to_owned(), Json::Bool(duplicate)),
                ("event".to_owned(), event.response_json()),
            ]),
        ),
        Err(_) => reply(stream, 500, error("could_not_persist_event")),
    }
}

fn exec_request(stream: &mut TcpStream, body: Vec<u8>) -> Result<(), String> {
    let request = match parse_exec_request(&body) {
        Ok(request) => request,
        Err(denial) => return reply(stream, 200, denial),
    };
    // Prompts can be sensitive, so logs contain only this minimal routing data.
    eprintln!(
        "exec id={} bin={} subcommand={}",
        request.id,
        request.bin,
        request.args.first().map(String::as_str).unwrap_or("")
    );
    let policy = match require_gui_login_session().and_then(|_| exec::load_policy()) {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("exec policy read failed: {message}");
            return reply(stream, 500, error("could_not_read_execution_policy"));
        }
    };
    let path = match policy_path_or_denial(&policy, &request) {
        Ok(path) => path,
        Err(denial) => return reply(stream, 200, denial),
    };
    let result = exec::run(path, request);
    reply(stream, 200, result.to_json())
}

fn parse_exec_request(body: &[u8]) -> Result<exec::ExecRequest, Json> {
    let parsed = std::str::from_utf8(body)
        .ok()
        .and_then(|text| parse_json(&text).ok());
    let Some(parsed) = parsed else {
        return Err(denial_json(""));
    };
    let denied_id = exec::request_id(&parsed);
    exec::parse_request(&parsed).map_err(|_| denial_json(&denied_id))
}

fn denied(stream: &mut TcpStream, id: &str) -> Result<(), String> {
    let (status, body) = denial_response(id);
    reply(stream, status, body)
}

fn policy_path_or_denial<'a>(
    policy: &'a exec::Policy,
    request: &exec::ExecRequest,
) -> Result<&'a str, Json> {
    policy
        .allowed_path(&request.bin, &request.args)
        .ok_or_else(|| denial_json(&request.id))
}

fn denial_response(id: &str) -> (u16, Json) {
    (200, denial_json(id))
}

fn denial_json(id: &str) -> Json {
    Json::Object(vec![
        ("id".to_owned(), Json::String(id.to_owned())),
        ("error".to_owned(), Json::String("denied".to_owned())),
    ])
}

fn get(stream: &mut TcpStream, state: &Server, target: &str) -> Result<(), String> {
    let (after, timeout, epoch) = match get_query(target) {
        Ok(query) => query,
        Err(message) => {
            reply(stream, 400, error(&message))?;
            return Ok(());
        }
    };
    let result = state
        .store
        .read(after, &epoch, Duration::from_secs(timeout));
    let result = match result {
        Ok(result) => result,
        Err(_) => {
            reply(stream, 500, error("could_not_read_events"))?;
            return Ok(());
        }
    };
    reply(stream, 200, read_json(result))
}

fn get_query(target: &str) -> Result<(u64, u64, String), String> {
    let query = query(target)?;
    let after = query.get("after").map_or(Ok(0), |value| {
        value
            .parse::<u64>()
            .map_err(|_| "after must be a non-negative integer".to_owned())
    })?;
    let timeout = query.get("timeout").map_or(Ok(50), |value| {
        value
            .parse::<u64>()
            .map_err(|_| "timeout must be an integer".to_owned())
    })?;
    if timeout > 55 {
        return Err("timeout must be between 0 and 55".to_owned());
    }
    Ok((
        after,
        timeout,
        query.get("epoch").cloned().unwrap_or_default(),
    ))
}

fn read_json(result: ReadResult) -> Json {
    Json::Object(vec![
        ("epoch".to_owned(), Json::String(result.epoch)),
        ("reset".to_owned(), Json::Bool(result.reset)),
        ("lost".to_owned(), Json::Bool(result.lost)),
        (
            "events".to_owned(),
            Json::Array(
                result
                    .events
                    .iter()
                    .map(|event| event.response_json())
                    .collect(),
            ),
        ),
        ("next".to_owned(), Json::number(result.next)),
    ])
}

fn read_request(stream: &mut TcpStream) -> Result<Request, ReadRequestError> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader
        .read_line(&mut first)
        .map_err(|_| ReadRequestError::Message("could not read request line".to_owned()))?;
    let mut parts = first.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| ReadRequestError::Message("malformed request line".to_owned()))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| ReadRequestError::Message("malformed request line".to_owned()))?
        .to_owned();
    if parts.next().is_none() {
        return Err(ReadRequestError::Message(
            "malformed request line".to_owned(),
        ));
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|_| ReadRequestError::Message("could not read headers".to_owned()))?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or_else(|| ReadRequestError::Message("malformed header".to_owned()))?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map_or(Ok(0), |value| {
        value
            .parse::<usize>()
            .map_err(|_| ReadRequestError::Message("invalid content length".to_owned()))
    })?;
    if length > MAX_BODY {
        if method == "POST" && target.split('?').next() == Some("/v1/exec") {
            return Err(ReadRequestError::ExecDenied);
        }
        return Err(ReadRequestError::Message(
            "request body too large".to_owned(),
        ));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|_| {
        if method == "POST" && target.split('?').next() == Some("/v1/exec") {
            ReadRequestError::ExecDenied
        } else {
            ReadRequestError::Message("short request body".to_owned())
        }
    })?;
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

fn query(target: &str) -> Result<HashMap<String, String>, String> {
    let Some((_, raw)) = target.split_once('?') else {
        return Ok(HashMap::new());
    };
    raw.split('&')
        .filter(|value| !value.is_empty())
        .map(|item| {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            Ok((percent_decode(key)?, percent_decode(value)?))
        })
        .collect()
}
fn percent_decode(input: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut chars = input.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let high = chars
                .next()
                .ok_or_else(|| "invalid URL encoding".to_owned())?;
            let low = chars
                .next()
                .ok_or_else(|| "invalid URL encoding".to_owned())?;
            bytes.push((hex(high)? << 4) | hex(low)?);
        } else if byte == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| "invalid URL encoding".to_owned())
}
fn hex(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("invalid URL encoding".to_owned()),
    }
}
fn authorized(supplied: &str, secret: &str) -> bool {
    let expected = format!("Bearer {secret}");
    let mut difference = expected.len() ^ supplied.len();
    for (index, left) in expected.bytes().enumerate() {
        difference |= (left ^ supplied.as_bytes().get(index).copied().unwrap_or(0)) as usize;
    }
    difference == 0
}
fn error(message: &str) -> Json {
    Json::Object(vec![("error".to_owned(), Json::String(message.to_owned()))])
}
fn reply(stream: &mut TcpStream, code: u16, value: Json) -> Result<(), String> {
    let body = value.to_json();
    let reason = match code {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    stream.write_all(format!("HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_get_query_is_a_bad_request() {
        assert_eq!(
            get_query("/v1/events?after=not-a-number").unwrap_err(),
            "after must be a non-negative integer"
        );
        assert_eq!(
            get_query("/v1/events?epoch=%ZZ").unwrap_err(),
            "invalid URL encoding"
        );
    }

    #[test]
    fn bearer_comparison_requires_the_full_token() {
        assert!(authorized(
            &format!("Bearer {}", "x".repeat(32)),
            &"x".repeat(32)
        ));
        assert!(!authorized("Bearer x", &"x".repeat(32)));
        assert!(!authorized(
            &format!("Bearer {}suffix", "x".repeat(32)),
            &"x".repeat(32)
        ));
    }

    #[test]
    fn explicit_bind_address_must_be_tailscale_ipv4() {
        assert!(is_tailscale_ipv4("100.101.237.83".parse().unwrap()));
        assert!(!is_tailscale_ipv4("0.0.0.0".parse().unwrap()));
        assert!(!is_tailscale_ipv4("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn allowlist_updater_accepts_only_its_exact_arguments() {
        let valid = vec![
            "set-allowlist".to_owned(),
            "--file".to_owned(),
            "/secure/policy.json".to_owned(),
        ];
        assert_eq!(allowlist_file(&valid).unwrap(), "/secure/policy.json");
        for invalid in [
            vec![],
            vec!["set-allowlist".to_owned()],
            vec!["set-allowlist".to_owned(), "--file".to_owned()],
            vec![
                "set-allowlist".to_owned(),
                "--other".to_owned(),
                "/secure/policy.json".to_owned(),
            ],
            vec![
                "set-allowlist".to_owned(),
                "--file".to_owned(),
                "".to_owned(),
            ],
        ] {
            assert!(allowlist_file(&invalid).is_err());
        }
    }

    #[test]
    fn gui_session_check_rejects_remote_or_non_graphical_sessions() {
        assert!(is_local_gui_session(0, SESSION_HAS_GRAPHIC_ACCESS));
        assert!(!is_local_gui_session(
            0,
            SESSION_HAS_GRAPHIC_ACCESS | SESSION_IS_REMOTE
        ));
        assert!(!is_local_gui_session(0, 0));
        assert!(!is_local_gui_session(-1, SESSION_HAS_GRAPHIC_ACCESS));
    }

    #[test]
    fn exec_denials_are_opaque_and_never_include_policy_data() {
        let expected = r#"{"id":"request-1","error":"denied"}"#;
        let invalid_json =
            match parse_exec_request(br#"{"id":"request-1","bin":"jules","args":["new"]"#) {
                Err(denial) => denial,
                Ok(_) => panic!("malformed request was accepted"),
            };
        assert_eq!(invalid_json.to_json(), r#"{"id":"","error":"denied"}"#);
        let malformed_schema =
            match parse_exec_request(br#"{"id":"request-1","bin":"jules","args":"new"}"#) {
                Err(denial) => denial,
                Ok(_) => panic!("malformed request was accepted"),
            };
        assert_eq!(malformed_schema.to_json(), expected);

        let policy = exec::Policy::parse(
            r#"{"bins":{"jules":{"path":"/private/configured-binary","commands":[["new"]]}}}"#,
        )
        .unwrap();
        for body in [
            br#"{"id":"request-1","bin":"unknown","args":["new"]}"#.as_slice(),
            br#"{"id":"request-1","bin":"jules","args":["login"]}"#.as_slice(),
        ] {
            let request = parse_exec_request(body).unwrap();
            let denial = policy_path_or_denial(&policy, &request)
                .unwrap_err()
                .to_json();
            assert_eq!(denial, expected);
            assert!(!denial.contains("configured-binary"));
            let (status, response) = denial_response(&request.id);
            assert_eq!(status, 200);
            assert_eq!(response.to_json(), expected);
        }
    }

    #[test]
    fn oversized_exec_request_is_an_opaque_denial_before_body_allocation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "POST /v1/exec HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY + 1
            )
            .unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        assert!(matches!(
            read_request(&mut server),
            Err(ReadRequestError::ExecDenied)
        ));
    }

    #[test]
    fn truncated_exec_request_is_an_opaque_denial() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "POST /v1/exec HTTP/1.1\r\nContent-Length: 1\r\n\r\n"
            )
            .unwrap();
        });
        let (mut server, _) = listener.accept().unwrap();
        client.join().unwrap();
        assert!(matches!(
            read_request(&mut server),
            Err(ReadRequestError::ExecDenied)
        ));
    }
}
