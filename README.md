# Tunelith

A low-level, cross-platform abstraction over tuners for the Japanese digital
broadcasting systems: ISDB-T, ISDB-S and ISDB-S3 (4K/8K), written in Rust.

Tunelith tunes by **broadcasting system, frequency and stream id** (TSID for
ISDB-S, TLV stream id for ISDB-S3), and keeps no channel list. Channel
definitions, scanning, EPG and descrambling are left to the layer above.

> [!WARNING]
> Tunelith is at an early stage. The API and the command line will change.

## Device support

| Device | Linux | Windows | macOS | Browser (WebUSB) |
|---|:-:|:-:|:-:|:-:|
| PLEX PX-W3U4 / PX-W3PE4 / PX-W3PE5 | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| PLEX PX-Q3U4 / PX-Q3PE4 / PX-Q3PE5 | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| PLEX PX-MLT5U / PX-MLT5PE / PX-MLT8PE | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| PLEX PX-M1UR | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| PLEX PX-S1UR | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| Digibest ISDB6014 V2.0 (4TS) | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| Digibest ISDB2056 / ISDB2056N | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| Digibest ISDBT2071 | ✅[^untested] | ✅[^os] | ✅[^os] | 🚧 |
| e-better DTV02A-5TS-P | ✅ | ✅[^os] | ✅[^os] | 🚧 |
| PT4K (TBS6812)[^left] | ✅ | 🚧 | — | — |
| Other ISDB tuners with a Linux DVB driver | ✅[^generic] | — | — | — |

✅ supported, 🚧 planned, — not planned.

[^left]: ISDB-S3 is tested with right-hand circular 4K broadcasts only; a
    left-hand one (NHK BS8K) could not be received with the antenna at hand,
    by any tool.
[^untested]: Ported from px4_drv along with the DTV02A-5TS-P, but not yet
    tested on the device itself. Reports are welcome.
[^generic]: Whatever the kernel driver supports, taken as it is. Model-specific
    handling goes in a driver of its own.
[^os]: Builds and passes the tests on the OS in CI, but has not run on a device
    there yet. On Windows, WinUSB must be bound to the device.

The USB driver runs in user space over [nusb](https://github.com/kevinmehall/nusb),
which works on Linux, Windows (WinUSB), macOS and in Chromium browsers
(WebUSB). On Windows, the PT4K is to go through BDA.

A PX-Q model is two boards on one card; Tunelith joins them into one device
of eight tuners, powered together.

## Crates

| Crate | Contents | License |
|---|---|---|
| `tunelith-core` | Public types, the `Driver` / `Device` / `Tuner` traits, `Registry`, the generic Linux DVB driver, the USB transport over nusb, the protocol of tunelithd | MIT OR Apache-2.0 |
| `tunelith` | The client of tunelithd | MIT OR Apache-2.0 |
| `tunelith-driver-pt4k` | PT4K (TBS6812) on top of the generic DVB driver | MIT OR Apache-2.0 |
| `tunelith-driver-px4` | The PLEX / e-better / Digibest USB tuners, ported from px4_drv | GPL-2.0-only |
| `tunelith-cli` | The `tunelith` command and `tunelithd`, the daemon sharing the tuners among programs | GPL-2.0-only |

`tunelith-driver-px4` is a port of [px4_drv](https://github.com/tsukumijima/px4_drv)
and is under its license, GPL-2.0-only; see its [PROVENANCE.md](crates/tunelith-driver-px4/PROVENANCE.md).
A program that links it is under the GPL as well.

## Getting started

### Build

The toolchain is pinned in `rust-toolchain.toml`.

```shell
cargo build --release
```

### Firmware

The USB tuners need the IT930x firmware, `it930x-firmware.bin`, which Tunelith
does not ship. Put it in one of:

- `/lib/firmware/`
- `$XDG_DATA_HOME/tunelith/firmware/` (`~/.local/share/tunelith/firmware/` by default)
- `%ProgramData%\tunelith\firmware\` on Windows

If px4_drv is installed, the file is already in `/lib/firmware/`.

### Permissions (Linux)

The USB tuners are driven through usbfs, which needs write access to the
device node. [`packaging/udev/90-tunelith.rules`](packaging/udev/90-tunelith.rules)
gives it to the `video` group and to the user logged in at the seat:

```shell
sudo install -m644 packaging/udev/90-tunelith.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

If the px4_drv kernel module is loaded, Tunelith takes the device from it when
opening it; the module is to be blacklisted for Tunelith to be the only one
using the device. The DVB tuners need access to `/dev/dvb`, usually through the
`video` group.

## Usage

### tunelithd

tunelithd holds the devices and shares the tuners among programs. A program
asks it for a stream of what to receive, and gets one of a free tuner, or of
the tuner already receiving the same for another program.

```shell
tunelithd
```

It listens on `/run/tunelith/tunelithd.sock` (on Windows, the named pipe
`\\.\pipe\tunelith`), or where `--socket` or `TUNELITH_SOCKET` says, and
lets the members of the `video` group use the tuners, or of the group
`--socket-group` names.

On Linux, systemd runs it either for the whole system or for one user; the
units are in [`packaging/systemd`](packaging/systemd), and in the release
archives.

- **System**: a user of its own runs it, reaching the tuners and sharing them
  through the `video` group.

  ```shell
  sudo install -m644 packaging/systemd/system/tunelithd.service /etc/systemd/system/
  sudo systemctl enable --now tunelithd
  ```

- **User**: it runs as the user, who alone may use it, on
  `$XDG_RUNTIME_DIR/tunelith/tunelithd.sock`. The `tunelith` command looks for
  this socket before that of the system.

  ```shell
  install -Dm644 packaging/systemd/user/tunelithd.service ~/.config/systemd/user/tunelithd.service
  systemctl --user enable --now tunelithd
  ```

Both look `tunelithd` up in `/usr/local/bin`, `/usr/bin` and the like;
`systemctl edit tunelithd` (with `--user` for the user's) points them at one
elsewhere, such as `~/.cargo/bin`. The USB tuners need the udev rule above in
either case.

### The `tunelith` command

The command goes through tunelithd, or opens the devices itself with
`--direct`.

List the devices and their tuners:

```shell
tunelith list
```

Tune and write the stream to stdout:

```shell
# ISDB-T: the frequency in kHz.
tunelith tune --system isdb-t --freq 521143 > out.ts

# ISDB-S: the downlink frequency in kHz, before the LNB converts it, and the TSID.
tunelith tune --system isdb-s --freq 11727480 --stream-id 0x4010 > out.ts

# ISDB-S3: the TLV stream id; add `--polarization left` for a left-hand circular broadcast.
tunelith tune --system isdb-s3 --freq 12034360 --stream-id 0xB110 > out.tlv
```

| Option | Description |
|---|---|
| `--system` | `isdb-t`, `isdb-s` or `isdb-s3` |
| `--freq` | The frequency on air in kHz. For a satellite, the downlink frequency (11727480 for BS-1), which Tunelith converts for the LNB |
| `--stream-id` | The TSID for ISDB-S, the TLV stream id for ISDB-S3; decimal, or hexadecimal with `0x`. Relative TS numbers are not accepted |
| `--polarization` | `right` (default) or `left` |
| `--tuner` | The tuner to use, as `list` shows it; the first free one receiving the system if omitted |
| `--lnb` | Powers the LNB of the antenna |
| `--duration` | Stops after this many seconds |
| `--direct` | Opens the devices directly rather than through tunelithd |
| `--socket` | The socket of tunelithd (or `TUNELITH_SOCKET`); that of the user's tunelithd if there is one, or else the system's |

The stream is MPEG-2 TS for ISDB-T and ISDB-S, and TLV for ISDB-S3.

## Not in scope

- A channel list, channel scanning, service separation and EPG.
- Descrambling (B-CAS / ACAS).
- Loading BonDriver DLLs, recpt1-compatible command lines, a Mirakurun-compatible API.
- The TS obfuscation of the old PLEX models.

## License

Each crate is under the license in the table above. The IT930x firmware is not
part of Tunelith.
