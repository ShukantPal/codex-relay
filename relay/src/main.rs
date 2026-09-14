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

const MAX_BODY: usize = 64 * 1024;

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

fn main() {
    if let Err(error) = run() {
        eprintln!("relay: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = config(env::args().skip(1).collect())?;
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

fn config(arguments: Vec<String>) -> Result<Config, String> {
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
        Err(error) => {
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

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader
        .read_line(&mut first)
        .map_err(|_| "could not read request line".to_owned())?;
    let mut parts = first.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "malformed request line".to_owned())?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| "malformed request line".to_owned())?
        .to_owned();
    if parts.next().is_none() {
        return Err("malformed request line".to_owned());
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|_| "could not read headers".to_owned())?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or_else(|| "malformed header".to_owned())?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map_or(Ok(0), |value| {
        value
            .parse::<usize>()
            .map_err(|_| "invalid content length".to_owned())
    })?;
    if length > MAX_BODY {
        return Err("request body too large".to_owned());
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|_| "short request body".to_owned())?;
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
}
