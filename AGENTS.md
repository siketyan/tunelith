# AGENTS.md

This file provides guidance to coding agents (Claude Code and others) when working with code in this repository.

Tunelith is a low-level abstraction over tuners for the Japanese digital broadcasting systems (ISDB-T, ISDB-S,
ISDB-S3). It tunes by broadcasting system, frequency and stream id (TSID / TLV stream id) and keeps no channel list;
channel lists, scanning, EPG and descrambling belong to the layer above. README.md has the device support matrix and
the usage of the `tunelith` command.

## Setup

- The Rust toolchain is pinned in `rust-toolchain.toml`, so rustup picks it up on its own.
- The USB tuners need the IT930x firmware, `it930x-firmware.bin`, which is not in the repository and must never be
  committed, not even inside a test fixture or a USB capture.

## Commands

Rust (workspace of `crates/*`, edition 2024):

- Build: `cargo build`
- Test all: `cargo test --all-targets` (CI runs exactly this, on Linux)
- Test one crate: `cargo test -p tunelith-driver-px4`
- Lint: `cargo clippy --all-targets -- -D warnings` (warnings fail CI)
- Format: `cargo fmt --all` (checked in CI with `--check`)
- Licenses: `cargo deny check licenses` (CI runs it; see Licensing below)

## Architecture

- `tunelith-core` — the public types (`System`, `StreamId`, `TuneParams`, …), the `Driver` / `Device` / `Tuner`
  traits (dyn-compatible, returning `BoxFuture`), `Registry` (gives each device to the first driver that reports
  it, so a model's own driver goes before the generic one), the generic Linux DVB driver with its `Quirks` (`dvb`
  feature, ioctls written after the uapi headers), and the USB transport over nusb (`usb` feature). nusb types stay
  inside `usb.rs`. Chip-level traits (`I2c`, `UsbTransport`) use plain `async fn` / `impl Future` and generics.
- `tunelith-driver-pt4k` — the PT4K (TBS6812): `Quirks` over the generic DVB driver, adding ISDB-S3.
- `tunelith-driver-px4` — the PLEX / e-better / Digibest USB tuners, a port of px4_drv run in user space:
  - `it930x.rs` is the USB bridge; `cxd2856er.rs`, `cxd2858er.rs`, `tc90522.rs`, `r850.rs`, `rt710.rs` the chips,
    each taking `&mut impl I2c` (a TC90522 relays to its tuner through `tuner_bus`, a CXD2856ER through a gate).
  - `board.rs` holds what the models share: the `Device` / `Tuner` over a `Board` trait, and the tuning sequence of
    px4_drv's `ptx_chrdev.c`. `pxmlt.rs`, `px4.rs` and `single.rs` are the boards; `stream.rs` splits the TS of a
    bridge among its tuners.
  - `lib.rs` has the model table (USB product ids) and the firmware search.
- `tunelith-core::proto` — the protocol of tunelithd, generated from `crates/tunelith-core/proto/tunelith/v1/tunelith.proto`
  by rust-protobuf (3.x, MIT; not prost or other Apache-2.0-only runtimes) in `build.rs`: length-prefixed `Envelope`s
  on a control connection, and a data connection per stream carrying the raw bytes after its `Hello`.
- `tunelithd` — the daemon: opens every device at start, shares a tuner among the clients asking for the same
  `TuneParams`, otherwise takes the first free one; each client reads through a bounded channel, losing data rather
  than holding the others up (`DropEvent`).
- `tunelith` — the client of tunelithd (Tokio, Unix domain sockets).
- `tunelith-cli` — the `tunelith` command (`list`, `tune`), through tunelithd or, with `--direct`, the devices.

Frequencies are in kHz. A satellite frequency is the downlink one, before the LNB (`TuneParams::if_frequency_khz`
converts it); a `StreamId` is never a relative TS number.

## Licensing

- `tunelith-core`, `tunelith` and `tunelith-driver-pt4k` are MIT OR Apache-2.0; `tunelith-driver-px4`, `tunelithd`
  and `tunelith-cli` are GPL-2.0-only, as px4_drv is.
- Never copy or translate GPL code, px4_drv included, into the permissive crates: the dependency goes from
  `tunelith-driver-px4` to `tunelith-core`, never the other way. Linux uapi headers may be followed.
- Code ported from elsewhere keeps the original copyright in the file header, and the source goes in the
  `PROVENANCE.md` of the crate. Do not port code without a license (BonDriver_BDA, BDASpecial, …) or under GPLv3.
- Dependencies must be compatible with GPL-2.0: no crate under Apache-2.0 alone (`deny.toml`).

## Git workflow

- Open a pull request per topic from a branch; do not push to `main`.
- Use the Conventional Commits style in Git commits and GitHub PR title.
- Avoid writing long description in Git commits.
- Create multiple commits when the change is large.
- Use English in commit messages and PR description, following `.github/pull_request_template.md`.
