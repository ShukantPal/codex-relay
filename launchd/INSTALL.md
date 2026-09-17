# Install on the Mac (owner-run)

Do not put the bearer token in a plist, shell history, command line, or agent
prompt. Create the token file with restrictive permissions:

```sh
mkdir -p ~/.codex/zigzag
umask 077
openssl rand -hex 32 > ~/.codex/zigzag/zigzag.token
chmod 600 ~/.codex/zigzag/zigzag.token
```

Build Zigzag, then edit every `/REPLACE/...` path in
`com.shukantpal.zigzag.plist`. The service resolves the Mac's Tailscale
IPv4 address at startup and binds only that address plus `127.0.0.1` on port
8765. Do not change it to `0.0.0.0`, enable Funnel, or add public forwarding.

Note: the daemon discovers the Tailscale IPv4 address by running
`tailscale ip -4`, so the `tailscale` CLI must be on the daemon's PATH.
If it lives outside the default PATH (e.g. a Nix install), add an
`EnvironmentVariables` -> `PATH` entry to the installed plist.

Copy and bootstrap the reviewed plist:

```sh
cp launchd/com.shukantpal.zigzag.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.shukantpal.zigzag.plist
```

This repository deliberately does not run either command for you.

## Zigzag

```sh
zigzag --secret-file ~/.codex/zigzag/zigzag.token \
  --state-file ~/.codex/zigzag/events.json [--port 8765]
```

`--secret-file` can instead be supplied by `ZIGZAG_SECRET_FILE`. The file must
not be group/world readable and its content must be at least 32 bytes. For
testing only, `--tailscale-ip` can set a specific Tailscale IPv4 address;
ordinary operation discovers it using `tailscale ip -4`.

## Exec endpoint

Zigzag runs as a LaunchAgent inside the Mac's GUI login session, where the
macOS keychain is available. Its `POST /v1/exec` endpoint executes only a
binary and argv prefix named in the Keychain-held policy. SSH sessions cannot
read or modify that policy.

Shukant installs or updates it from his GUI login session by putting the JSON
policy in a file and running:

```sh
zigzag config set-allowlist --file /secure/path/exec-allowlist.json
```

Zigzag verifies that it is in a local graphical macOS session before either
reading or writing the policy, and refuses those operations from SSH. This
keeps Keychain policy access within the owner GUI-login session.
The command validates the policy, stores canonical JSON in the `zigzag` /
`exec-allowlist` Keychain item, then prints the stored normalized policy for
confirmation. macOS may prompt once to grant this specific Zigzag binary
"Always Allow" access to the Keychain item; accept that prompt only after
verifying the binary is the reviewed Zigzag build.

For an allowed operation, a caller uses the configured binary short name
rather than a caller-supplied path:

```sh
curl -s http://100.101.237.83:8765/v1/exec \
  -H "Authorization: Bearer $(cat ~/.codex/zigzag/zigzag.token)" \
  -H 'Content-Type: application/json' \
  -d '{"id": "list-repos-1", "bin": "jules", "args": ["remote", "list", "--repo"]}'
```

There is no shell or PATH lookup: arguments are passed directly to the absolute
path from policy. Execution is capped at 300 seconds and 1 MiB of captured
output per stream. A successful response looks like:

```json
{"id": "list-repos-1", "exit_code": 0, "stdout": "...", "stderr": "",
 "truncated": false, "timed_out": false}
```

Unknown binary names, disallowed prefixes, and malformed request bodies all
return the same opaque denial response (with the submitted id when usable):

```json
{"id": "list-repos-1", "error": "denied"}
```

## VM poller

Provision an identical mode-600 token file using the existing secret-delivery
mechanism. With the verified HTTP forward proxy:

```sh
ZIGZAG_PROXY="${HTTPS_PROXY%:*}:3130" poller \
  --zigzag-url http://100.101.237.83:8765 \
  --secret-file /secure/path/zigzag.token \
  --state-file /var/lib/muse/zigzag-cursor.json
```

The poller has `--once` for supervised smoke tests and accepts `--timeout`
(1–55 seconds, default 50). It retries network failures, reports reset/loss
warnings on stderr, and emits exactly one JSON object per stdout line.
