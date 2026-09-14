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
