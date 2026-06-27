# `tools/ota-serve`

> Tiny single-file HTTP dev server for esp-radio OTA development.

## Usage

From the repo root:

```bash
cargo make ota-serve
```

Prints the LAN URL and a terminal QR code, then waits for the device
to fetch `/firmware.bin`. Paste the URL into the web console's OTA
card or the printed `curl` snippet.

Override the defaults if needed:

```bash
cargo make ota-serve -- --port 9000 --bind 127.0.0.1
```

## Why not plain `cargo build` / `cargo run`?

The parent project's `.cargo/config.toml` hard-codes the embedded
RISC-V target plus `-Z stack-protector=all` and `[unstable]
build-std`. Cargo merges those into any child invocation and the host
stable toolchain rejects the unstable bits. The `cargo make` task
sets `RUSTUP_TOOLCHAIN`, `CARGO_BUILD_TARGET`, and `RUSTFLAGS`
environment overrides on the command line, which is the only portable
way around the inheritance — list-typed config fields like
`rustflags` and `unstable.build-std` cannot be cleared from a child
config file.

If you really want to invoke `cargo` directly, replicate the env
block from `[tasks.ota-serve.env]` in `Makefile.toml`:

```bash
RUSTUP_TOOLCHAIN=stable \
CARGO_BUILD_TARGET=$(rustc -vV | sed -n 's/host: //p') \
RUSTFLAGS="" \
cargo run --manifest-path tools/ota-serve/Cargo.toml --release -- --help
```
