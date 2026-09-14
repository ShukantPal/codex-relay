# Codex relay

`relay` runs on a Mac and retains the latest 1,000 authenticated completion
events. `poller` runs on the orchestrator VM, holds a long-poll request open,
and writes delivered events as JSON Lines. This replaces a five-minute status
poll with normal delivery latency close to one network round trip.

Delivery is at least once: downstream consumers must deduplicate using the
event `id` (or relay epoch and sequence). The queue is durable but bounded;
when it overflows, the poller reports a warning on stderr.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

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
