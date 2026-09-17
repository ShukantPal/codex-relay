# Install on the Mac (owner-run)

Do not put the bearer token in a plist, shell history, command line, or agent
prompt. Create the token file with restrictive permissions:

```sh
mkdir -p ~/.codex/relay
umask 077
openssl rand -hex 32 > ~/.codex/relay/relay.token
chmod 600 ~/.codex/relay/relay.token
```

Build the relay, then edit every `/REPLACE/...` path in
`com.shukantpal.codex-relay.plist`. The service resolves the Mac's Tailscale
IPv4 address at startup and binds only that address plus `127.0.0.1` on port
8765. Do not change it to `0.0.0.0`, enable Funnel, or add public forwarding.

Note: the daemon discovers the Tailscale IPv4 address by running
`tailscale ip -4`, so the `tailscale` CLI must be on the daemon's PATH.
If it lives outside the default PATH (e.g. a Nix install), add an
`EnvironmentVariables` -> `PATH` entry to the installed plist.

Copy and bootstrap the reviewed plist:

```sh
cp launchd/com.shukantpal.codex-relay.plist ~/Library/LaunchAgents/
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/com.shukantpal.codex-relay.plist
```

This repository deliberately does not run either command for you.

## Relay

```sh
relay --secret-file ~/.codex/relay/relay.token \
  --state-file ~/.codex/relay/events.json [--port 8765]
```

`--secret-file` can instead be supplied by `RELAY_SECRET_FILE`. The file must
not be group/world readable and its content must be at least 32 bytes. For
testing only, `--tailscale-ip` can set a specific Tailscale IPv4 address;
ordinary operation discovers it using `tailscale ip -4`.

The relay also accepts `--jules-bin` (or the `JULES_BIN` environment
variable), defaulting to `/Users/shukant/.npm-global/bin/jules`.

## Jules bridge

The relay runs as a LaunchAgent inside the Mac's GUI login session, where the
macOS keychain is available. `POST /v1/jules` runs an allowlisted `jules`
subcommand there and returns its output synchronously, which lets callers
without keychain access (such as SSH sessions) drive Jules:

```sh
curl -s http://100.101.237.83:8765/v1/jules \
  -H "Authorization: Bearer $(cat ~/.codex/relay/relay.token)" \
  -H 'Content-Type: application/json' \
  -d '{"id": "list-repos-1", "args": ["remote", "list", "--repo"]}'
```

Only the `new` and `remote` (`list`, `pull`, `new`) subcommands are accepted;
`login` and `logout` are never executed through the bridge, so it cannot
change the CLI's auth state. There is no shell: arguments are passed directly
to the fixed `jules` binary. Execution is capped at 300 seconds and 1 MiB of
captured output per stream. The response looks like:

```json
{"id": "list-repos-1", "exit_code": 0, "stdout": "...", "stderr": "",
 "truncated": false, "timed_out": false}
```

## VM poller

Provision an identical mode-600 token file using the existing secret-delivery
mechanism. With the verified HTTP forward proxy:

```sh
RELAY_PROXY="${HTTPS_PROXY%:*}:3130" poller \
  --relay-url http://100.101.237.83:8765 \
  --secret-file /secure/path/codex-relay.token \
  --state-file /var/lib/muse/codex-relay-cursor.json
```

The poller has `--once` for supervised smoke tests and accepts `--timeout`
(1–55 seconds, default 50). It retries network failures, reports reset/loss
warnings on stderr, and emits exactly one JSON object per stdout line.
