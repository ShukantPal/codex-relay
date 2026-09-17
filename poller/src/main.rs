use relay_core::{Json, parse_json, read_secret_file};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

struct Config {
    zigzag_url: String,
    secret_file: PathBuf,
    state_file: PathBuf,
    proxy: Option<String>,
    timeout: u64,
    once: bool,
}
struct Cursor {
    epoch: String,
    after: u64,
}
struct ZigzagUrl {
    authority: String,
    host: String,
    port: u16,
    base_path: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("poller: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = config(env::args().skip(1).collect())?;
    let secret = read_secret_file(&config.secret_file)?;
    let zigzag = parse_url(&config.zigzag_url)?;
    let mut cursor = load_cursor(&config.state_file)?;
    loop {
        match poll(
            &zigzag,
            config.proxy.as_deref(),
            &secret,
            &cursor,
            config.timeout,
        ) {
            Ok(message) => {
                let epoch = required_string(&message, "epoch")?.to_owned();
                let reset = required_bool(&message, "reset")?;
                let lost = required_bool(&message, "lost")?;
                let next = required_number(&message, "next")?;
                if reset {
                    eprintln!("Zigzag epoch changed; resetting cursor");
                }
                if lost {
                    eprintln!("WARNING: Zigzag retention was exceeded; some events were lost");
                }
                let events = match message.object("events") {
                    Some(Json::Array(events)) => events,
                    _ => return Err("Zigzag response missing events array".to_owned()),
                };
                for event in events {
                    let Json::Object(mut fields) = event.clone() else {
                        return Err("Zigzag response contains a non-object event".to_owned());
                    };
                    fields.push(("zigzag_epoch".to_owned(), Json::String(epoch.clone())));
                    println!("{}", Json::Object(fields).to_json());
                }
                std::io::stdout()
                    .flush()
                    .map_err(|error| format!("could not flush output: {error}"))?;
                cursor = Cursor { epoch, after: next };
                save_cursor(&config.state_file, &cursor)?;
                if config.once {
                    return Ok(());
                }
            }
            Err(error) => {
                eprintln!("poll failed: {error}; retrying in 2s");
                if config.once {
                    return Err(error);
                }
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn config(arguments: Vec<String>) -> Result<Config, String> {
    let mut zigzag_url = None;
    let mut secret_file = env::var_os("ZIGZAG_SECRET_FILE").map(PathBuf::from);
    let mut state_file = None;
    let mut proxy = env::var("ZIGZAG_PROXY")
        .ok()
        .filter(|value| !value.is_empty());
    let mut timeout = 50;
    let mut once = false;
    let mut values = arguments.into_iter();
    while let Some(argument) = values.next() {
        let value = |values: &mut std::vec::IntoIter<String>, name: &str| {
            values
                .next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match argument.as_str() {
            "--zigzag-url" => zigzag_url = Some(value(&mut values, "--zigzag-url")?), "--secret-file" => secret_file = Some(PathBuf::from(value(&mut values, "--secret-file")?)),
            "--state-file" => state_file = Some(PathBuf::from(value(&mut values, "--state-file")?)), "--proxy" => proxy = Some(value(&mut values, "--proxy")?),
            "--timeout" => timeout = value(&mut values, "--timeout")?.parse().map_err(|_| "--timeout must be an integer".to_owned())?, "--once" => once = true,
            "--help" | "-h" => return Err("usage: poller --zigzag-url http://HOST:PORT --secret-file PATH --state-file PATH [--proxy URL] [--timeout 50] [--once]".to_owned()), _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=55).contains(&timeout) {
        return Err("--timeout must be between 1 and 55".to_owned());
    }
    Ok(Config {
        zigzag_url: zigzag_url.ok_or_else(|| "--zigzag-url is required".to_owned())?,
        secret_file: secret_file
            .ok_or_else(|| "--secret-file or ZIGZAG_SECRET_FILE is required".to_owned())?,
        state_file: state_file.ok_or_else(|| "--state-file is required".to_owned())?,
        proxy,
        timeout,
        once,
    })
}

fn parse_url(input: &str) -> Result<ZigzagUrl, String> {
    let rest = input.strip_prefix("http://").ok_or_else(|| {
        "Zigzag URL must use http:// (the tailnet is the transport boundary)".to_owned()
    })?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() || authority.contains('@') {
        return Err("invalid Zigzag URL authority".to_owned());
    }
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        (
            host.to_owned(),
            port.parse()
                .map_err(|_| "invalid Zigzag URL port".to_owned())?,
        )
    } else {
        (authority.to_owned(), 80)
    };
    if host.is_empty() {
        return Err("invalid Zigzag URL host".to_owned());
    }
    let base_path = if path.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}/", path.trim_matches('/'))
    };
    Ok(ZigzagUrl {
        authority: authority.to_owned(),
        host,
        port,
        base_path,
    })
}

fn poll(
    zigzag: &ZigzagUrl,
    proxy: Option<&str>,
    secret: &str,
    cursor: &Cursor,
    timeout: u64,
) -> Result<Json, String> {
    let target = format!(
        "{}v1/events?after={}&epoch={}&timeout={timeout}",
        zigzag.base_path,
        cursor.after,
        encode(&cursor.epoch)
    );
    let (connection, request_target) = if let Some(proxy) = proxy {
        let proxy = parse_url(proxy)?;
        (
            TcpStream::connect((proxy.host.as_str(), proxy.port))
                .map_err(|error| format!("could not connect to proxy: {error}"))?,
            format!("http://{}{}", zigzag.authority, target),
        )
    } else {
        (
            TcpStream::connect((zigzag.host.as_str(), zigzag.port))
                .map_err(|error| format!("could not connect to Zigzag: {error}"))?,
            target,
        )
    };
    connection
        .set_read_timeout(Some(Duration::from_secs(timeout + 15)))
        .map_err(|error| error.to_string())?;
    connection
        .set_write_timeout(Some(Duration::from_secs(15)))
        .map_err(|error| error.to_string())?;
    let mut connection = connection;
    connection.write_all(format!("GET {request_target} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {secret}\r\nAccept: application/json\r\nConnection: close\r\n\r\n", zigzag.authority).as_bytes()).map_err(|error| format!("could not write poll: {error}"))?;
    read_response(&mut connection)
}

fn read_response(stream: &mut TcpStream) -> Result<Json, String> {
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .map_err(|_| "could not read HTTP status".to_owned())?;
    let code = status
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "malformed HTTP status".to_owned())?
        .parse::<u16>()
        .map_err(|_| "malformed HTTP status".to_owned())?;
    let mut length = None;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|_| "could not read response headers".to_owned())?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .ok_or_else(|| "malformed HTTP response header".to_owned())?;
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "invalid response content length".to_owned())?,
            );
        }
    }
    let mut body = Vec::new();
    match length {
        Some(length) => {
            body.resize(length, 0);
            reader
                .read_exact(&mut body)
                .map_err(|_| "short HTTP response body".to_owned())?;
        }
        None => {
            reader
                .read_to_end(&mut body)
                .map_err(|_| "could not read HTTP response body".to_owned())?;
        }
    }
    if code != 200 {
        return Err(format!("Zigzag returned HTTP {code}"));
    }
    parse_json(std::str::from_utf8(&body).map_err(|_| "Zigzag response was not UTF-8".to_owned())?)
}

fn load_cursor(path: &Path) -> Result<Cursor, String> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let value =
                parse_json(&contents).map_err(|error| format!("invalid cursor file: {error}"))?;
            Ok(Cursor {
                epoch: required_string(&value, "epoch")?.to_owned(),
                after: required_number(&value, "next")?,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Cursor {
            epoch: String::new(),
            after: 0,
        }),
        Err(error) => Err(format!(
            "could not read cursor file {}: {error}",
            path.display()
        )),
    }
}

fn save_cursor(path: &Path, cursor: &Cursor) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create cursor directory: {error}"))?;
    let temporary = parent.join(format!(
        ".poller-{}-{}.tmp",
        std::process::id(),
        cursor.after
    ));
    let result = (|| -> Result<(), String> {
        let mut output = private_file(&temporary)
            .map_err(|error| format!("could not create temporary cursor: {error}"))?;
        output
            .write_all(
                Json::Object(vec![
                    ("epoch".to_owned(), Json::String(cursor.epoch.clone())),
                    ("next".to_owned(), Json::number(cursor.after)),
                ])
                .to_json()
                .as_bytes(),
            )
            .map_err(|error| format!("could not write cursor: {error}"))?;
        output.write_all(b"\n").map_err(|error| error.to_string())?;
        output.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("could not replace cursor: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
#[cfg(not(unix))]
fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn required_string<'a>(value: &'a Json, field: &str) -> Result<&'a str, String> {
    value
        .object(field)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("Zigzag response missing string {field}"))
}
fn required_number(value: &Json, field: &str) -> Result<u64, String> {
    value
        .object(field)
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("Zigzag response missing integer {field}"))
}
fn required_bool(value: &Json, field: &str) -> Result<bool, String> {
    value
        .object(field)
        .and_then(Json::as_bool)
        .ok_or_else(|| format!("Zigzag response missing boolean {field}"))
}
fn encode(input: &str) -> String {
    input
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zigzag-poller-{name}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn cursor_round_trip_is_private_and_restart_safe() {
        let file = path("cursor");
        let expected = Cursor {
            epoch: "epoch-1".to_owned(),
            after: 42,
        };
        save_cursor(&file, &expected).unwrap();
        let actual = load_cursor(&file).unwrap();
        assert_eq!(actual.epoch, expected.epoch);
        assert_eq!(actual.after, expected.after);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o077, 0);
        }
        let _ = fs::remove_file(file);
    }

    #[test]
    fn missing_cursor_starts_at_the_beginning() {
        let file = path("missing-cursor");
        let cursor = load_cursor(&file).unwrap();
        assert!(cursor.epoch.is_empty());
        assert_eq!(cursor.after, 0);
    }

    #[test]
    fn zigzag_url_and_timeout_validation_reject_unsafe_inputs() {
        assert!(parse_url("https://zigzag.example").is_err());
        assert!(parse_url("http://user@zigzag.example").is_err());
        assert!(
            config(vec![
                "--zigzag-url".to_owned(),
                "http://100.101.237.83:8765".to_owned(),
                "--secret-file".to_owned(),
                "/token".to_owned(),
                "--state-file".to_owned(),
                "/cursor".to_owned(),
                "--timeout".to_owned(),
                "56".to_owned(),
            ])
            .is_err()
        );
    }
}
