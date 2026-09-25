# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/siketyan/tunelith/compare/v0.2.0...v0.2.1) - 2026-09-25

### Added

- *(pt4k)* support Windows through the BDA driver ([#24](https://github.com/siketyan/tunelith/pull/24))

### Fixed

- *(cli)* stop waiting on a tuner that gives out nothing ([#27](https://github.com/siketyan/tunelith/pull/27))
- *(cli)* pass over the devices and tuners --direct cannot open ([#25](https://github.com/siketyan/tunelith/pull/25))

## [0.2.0](https://github.com/siketyan/tunelith/compare/v0.1.0...v0.2.0) - 2026-09-25

### Added

- tell why tunelithd failed with an error code ([#23](https://github.com/siketyan/tunelith/pull/23))
- run tunelithd with systemd, for the system or a user ([#12](https://github.com/siketyan/tunelith/pull/12))
- *(cli)* ship tunelithd with the tunelith command ([#10](https://github.com/siketyan/tunelith/pull/10))
- support WebUSB through tunelith-wasm ([#17](https://github.com/siketyan/tunelith/pull/17))

### Fixed

- *(px4)* keep the timer at 1 ms on Windows while a device is open ([#22](https://github.com/siketyan/tunelith/pull/22))

### Other

- *(tunelith)* add a README, examples and rustdoc to the client ([#13](https://github.com/siketyan/tunelith/pull/13))
- *(wasm)* add E2E tests on a mock device and a real one ([#18](https://github.com/siketyan/tunelith/pull/18))
- name the e-better rebrands of the Digibest tuners ([#15](https://github.com/siketyan/tunelith/pull/15))

## [0.1.0](https://github.com/siketyan/tunelith/releases/tag/v0.1.0) - 2026-09-25

### Added

- build for Windows and macOS ([#4](https://github.com/siketyan/tunelith/pull/4))
- add tunelithd and its client ([#3](https://github.com/siketyan/tunelith/pull/3))
- *(px4)* port the PX-MLT series driver from px4_drv
- add core types, generic DVB driver, PT4K driver and CLI

### Other

- release with release-plz and dist ([#5](https://github.com/siketyan/tunelith/pull/5))
- add AGENTS.md, a PR template and CI ([#1](https://github.com/siketyan/tunelith/pull/1))
