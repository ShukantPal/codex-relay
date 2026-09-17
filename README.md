# Zigzag

_Managed by Pal's Muse._

`zigzag` runs on a Mac and retains the latest 1,000 authenticated completion
events. `poller` runs on the orchestrator VM, holds a long-poll request open,
and writes delivered events as JSON Lines. This replaces a five-minute status
poll with normal delivery latency close to one network round trip.

Delivery is at least once: downstream consumers must deduplicate using the
event `id` (or Zigzag epoch and sequence). POST idempotency applies while an
event remains in the durable bounded queue; a replay after eviction is a new
delivery. The queue is durable but bounded; when it overflows, the poller
reports a warning on stderr.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Release signing and deploy (Mac)

Every `cargo build` re-generates the binary's ad-hoc signature (new
identifier, new cdhash), so the keychain treats each rebuild as a different
app and re-prompts for allowlist access. Sign every release build with a
stable certificate identity instead:

```sh
./scripts/sign-release.sh
```

This builds, signs with `Apple Development: Shukant Pal` under the fixed
identifier `com.shukantpal.zigzag`, verifies, and restarts the LaunchAgent.
Run it in an interactive Mac terminal, never over SSH: code signing needs
the login keychain, and restarting the LaunchAgent from SSH puts the daemon
in the wrong macOS security session (its keychain reads hang and `/v1/exec`
stops responding). The script refuses to run over SSH.

The first run after switching to stable signing triggers one keychain
prompt when the daemon first reads the allowlist; choose **Always Allow**
so all future rebuilds keep working with no further prompts.

### Allowlist management

```sh
# Read the current policy (GUI session only)
zigzag config get-allowlist
# Replace the entire policy with the JSON in FILE (GUI session only).
# This REPLACES, not merges: export first, edit, then set.
zigzag config set-allowlist --file /path/to/policy.json
```

Policy JSON shape: `{"bins": {"<name>": {"path": "/abs/path", "commands": [["sub", "..."]], ...}}}`.
`commands` entries are argv prefixes. The `gh` bin also accepts
`"gh_read_repos": ["owner/repo"]` to scope `gh api` / `pr` commands.

## Linux VM build

The poller uses only the Rust standard library. Cross-compile for the VM after
installing the target and a compatible linker:

```sh
rustup target add x86_64-unknown-linux-gnu
cargo build --release --target x86_64-unknown-linux-gnu -p poller
```

For a static binary, if the musl target/toolchain is available:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p poller
```

See [launchd/INSTALL.md](launchd/INSTALL.md) for Mac installation and both
binary command-line interfaces.
