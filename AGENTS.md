# Working on usbpd

This is our fork of [elagil/usbpd](https://github.com/elagil/usbpd) (`origin` = romixlab/usbpd, `upstream` = elagil with pushes disabled). It is shaped for Donguru (typec_dongle), which uses it through `[patch.crates-io]`. The API may change or break whenever Donguru needs it; upstream compatibility is not a goal. Upstreaming a small, clean fix is a bonus and goes through the person, never directly from a session.

## FEATURES.md is the source of truth

[FEATURES.md](FEATURES.md) lists what works, what is broken and what is planned, by stable area IDs (`PRL-2`, `VDM-1`). Update the item in the same commit as the code. New items take an id from `tpm work claim AREA --repo usbpd`.

## CHANGELOG.md records every change

Each change gets a line under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md), with the FEATURES id where there is one. Breaking changes say what a downstream crate (typec_dongle, our embassy fork) has to change.

## Layout

- `usbpd-traits/`: the `Driver` trait, `Sop`, `RxFrame` and the driver errors. Our embassy fork's UCPD driver implements it.
- `usbpd/src/protocol_layer/`: message IDs per SOP, retries, GoodCRC, chunked RX and TX, resets. `message/` holds the message types and their parsers.
- `usbpd/src/sink/`, `usbpd/src/source/`: the policy engines and their device policy manager (DPM) traits.
- `usbpd/src/dual_role.rs`: the dual-role port on top of both engines.
- `usbpd/src/vdm.rs`: the VDM hook shared by both engines.
- `usbpd/src/dummy/`: test driver, timer and DPMs (tests only).
- `examples/`: STM32 examples, excluded from the workspace. They need the `embassy` submodule, and an embassy whose UCPD driver implements the current `Driver` trait.
- `USB_PD_R3_2 V1.1 2024-10.md`: the specification. Code comments cite it as "PD 3.2 Sec. x.y" or "Tab. x.y".

## Commands

- `just test`: the workspace tests (`cargo nextest`, then the doctests). `just test --changed` tests only what changed against main.
- `just lint`: nightly rustfmt check (the `rustfmt.toml` options are nightly only) and clippy with warnings as errors.
- `just build`: everything CI builds, the firmware examples included (needs the embassy submodule).

## Code conventions

- `no_std`, no allocation, no floats, no formatting in the library. Tests use `std`.
- Nothing received from the partner may panic: parsers return errors or `Reserved`/`Other` variants.
- Cite the specification section for every protocol rule.
- Features: `epr` (default) gates EPR; `defmt`, `log` and `serde` are optional.

## Tests

Host tests run against `DummyDriver` and `DummyTimer`: inject received frames (with their SOP), run a policy engine step, then check the transmitted frames. Test data is our own or captured from our own hardware.

## Commits

Conventional commits (`feat:`, `fix:`, `chore:`), `!` for breaking changes. No version bumps on session branches; `tpm land` releases.
