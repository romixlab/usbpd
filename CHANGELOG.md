# Changelog

All notable changes to the `usbpd` and `usbpd-traits` crates are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [100.0.0] - 2026-11-03

Our fork (romixlab/usbpd) for Donguru, typec_dongle FW-59. The API breaks on purpose.

### Versions

- Both crates move to one workspace version on a forked line, 100.0.0, so they never collide with elagil's crates.io releases. Dependents patch them in with `usbpd = "100"` and `usbpd-traits = "100"` (typec_dongle today asks for `"3.0.0"` and `"2.0.0"`; our embassy fork's `usbpd-traits` requirement changes too).

### `usbpd-traits`: changed (breaking)

- `Driver::receive` returns an `RxFrame { sop, len }` and `Driver::transmit` takes a `Sop` (`usbpd PRL-1 sop-aware-messaging`). A driver that only handles SOP returns `Sop::Sop` and ignores the argument; our embassy fork's UCPD driver has to follow.
- New `DriverRxError::CableReset`, `DriverRxError::Detached` and `DriverTxError::Detached` (`usbpd PRL-3 driver-detach`). Matches on these enums need the new arms.

### `usbpd`: added

- Message IDs per SOP: SOP' and SOP'' traffic no longer disturbs the SOP counters, GoodCRC goes out on the SOP it acknowledges, and a Cable Reset resets the cable SOPs only. `Message` has an `sop` field (`usbpd PRL-1 sop-aware-messaging`).
- Chunked TX for extended messages over 26 bytes. EPR source caps with more than 6 PDOs now go out correctly (`usbpd PRL-2 chunked-tx`).
- `RunResult::Detached`: on a driver detach the sink and the source reset and return; call `run()` again on the next attach (`usbpd PRL-3 driver-detach`).
- `crate::vdm`: VDMs on any SOP go to the new `VdmDevicePolicyManager::vdm_received`, and `Event::SendVdm` sends one, in both engines (`usbpd VDM-2 vdm-dpm-hook`).
- `VendorDefined` message type: header plus up to six VDOs, with parser and serializer (`usbpd VDM-1 total-vdm-decoding`).
- AGENTS.md, CLAUDE.md and FEATURES.md; `just test` (nextest) and `just lint`.

### `usbpd`: changed (breaking)

- `SinkDpm` and `SourceDpm` require `VdmDevicePolicyManager`. An empty `impl VdmDevicePolicyManager for MyDpm {}` keeps the old behaviour (`Not_Supported`).
- `Data::VendorDefined` holds a `VendorDefined` instead of a `(VdmHeader, Vec<u32, 7>)` tuple.
- VDM enums have `Reserved(u8)` or `Other(u8)` variants and never panic on reserved values (`usbpd VDM-1 total-vdm-decoding`).
- The source `Event` is no longer `Copy`.
- `TxError` has `Detached` and `ChunkedTransmissionFailed`, and `RxError` has `Detached`.
- The `defmt` feature also enables `usbpd-traits/defmt`.

### Known limitations

- The firmware examples build against upstream embassy, whose UCPD driver implements the old `Driver` trait. They need our embassy fork.

## [3.1.0] — `usbpd`

### Added

- `epr` feature gate. The EPR protocol paths compile out when the feature is disabled, saving flash and RAM. Enabled by default.
  The EPR timer variants of `usbpd::timers::TimerType` require the feature.

## [3.0.0] — `usbpd`

### Added

- SPR and EPR source mode (`usbpd/src/source/`), first public release with it.
- Dual-role port (DRP) support (`usbpd/src/dual_role.rs`), with a DRP example.
- `uom-ng` for compile-time evaluation of unit conversion factors. Quantities from
  the original `uom` crate are distinct types. Note: `uom-ng` is a fork used for an
  important feature for now, and may be replaced in the future.

### Known limitations

- `Wait` message handling is incomplete.
- Message-type coverage is incomplete.
- The SPR/EPR source-mode policy engine is new in this release and is covered by
  host tests only.

## `usbpd-traits`

### [2.0.0] — 2026-02-09

### Changed

- `Driver::wait_for_vbus` takes `&mut self`.

### Added

- `Driver::HAS_AUTO_RETRY`: skip the protocol-layer retry loop when the PHY
  retries transmission automatically.

## `usbpd`

### [2.0.0] — 2026-02-09

### Added

- EPR sink mode: EPR messages, chunked extended messages, EPR mode entry/exit,
  EPR source capability handling.
- `serde` derives on sink capabilities.

### Changed

- Policy engine restructured into a module with dedicated host tests, plus many
  spec compliance fixes.
- Extended messages are sent using chunked mode.
- Requires `usbpd-traits` 2.0.

## [1.1.3] — 2025-08-14

- Refactor of PDO request naming and flexibility.

## [1.1.2] — 2025-08-10

- Documentation updates.

## [1.1.1] — 2025-08-03

- Version bump for `usbpd-traits` 1.1.0.

## [1.1.0] — 2025-08-03

- Transition from `futures` to `embassy-futures`.
- PPS periodic request as keep-alive.
- Fallible message parsing (parsing error type).
- `thiserror` error types.

## [1.0.3] — 2025-07-03

- Added `serde` support.

## [1.0.2] — 2025-06-23

- Made message types public, fixed `log` feature compilation.

## [1.0.1] — 2025-06-15

- CI-only change. Published alongside `usbpd-traits` 1.0.1, which adds the
  optional `Driver::HAS_AUTO_GOOD_CRC` constant.

## [1.0.0] — 2025-05-10

- First stable release.
- Split into two crates: `usbpd` and `usbpd-traits`.
