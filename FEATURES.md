# usbpd features and roadmap

This file is the single source of truth for what this usbpd fork does, what is broken and what is planned. [CHANGELOG.md](CHANGELOG.md) records what changed and when; this file records the current state.

This repository is our fork of [elagil/usbpd](https://github.com/elagil/usbpd), shaped for Portwright. Its API changes freely; upstream compatibility is not a goal. State as of 3 Nov 2026, based on upstream main `b1f0bb9`.

## How to use this file

- **Status** of each item:
  - ✅ done
  - 🚧 in progress or partially done (the note says what is missing)
  - 🐛 implemented, but with known bugs
  - 📋 planned
  - 💡 idea, not committed to
- **IDs** (`PRL-2`, `VDM-1`) are stable: never renumber or reuse one. New items take the next free number of their area (`tpm work claim AREA --repo usbpd`). Use the ID with a short slug in commit messages and CHANGELOG entries (`usbpd PRL-2 chunked-tx`).
- When finishing work, update the item in the same commit. Don't delete done items.

## Crates

| Crate | Role |
|---|---|
| `usbpd` | Protocol layer, sink and source policy engines, dual-role port, message types, VDM hook. `no_std`, no allocation. |
| `usbpd-traits` | The `Driver` trait a PHY implements (embassy-stm32's UCPD driver in our embassy fork). |

## Protocol layer (`PRL`)

- ✅ **PRL-1 SOP-aware messaging**: `Driver::receive` returns the SOP of each frame and `Driver::transmit` takes one. MessageIDCounter and stored MessageID per SOP; GoodCRC goes out on the SOP and with the revision of the message it acknowledges; only a GoodCRC on our own SOP acknowledges a transmission. Cable SOP headers clear the Cable Plug and data-role bits. A Cable Reset (`DriverRxError::CableReset`) resets the cable SOPs only. Waits for a specific SOP message drop cable traffic (`usbpd/src/protocol_layer/mod.rs`).
- ✅ **PRL-2 Chunked TX**: extended messages over 26 bytes go out in chunks, per PD 3.2 Sec. 6.12.2.1.3. No Chunk Request after chunk 0 means the partner does not chunk, and the message counts as sent. A wrong chunk number, a timeout later on or another message in between gives `TxError::ChunkedTransmissionFailed`. Golden test against a captured 40-byte `EPR_Source_Capabilities`.
- ✅ **PRL-3 Driver detach**: `DriverRxError::Detached` and `DriverTxError::Detached` end a policy engine run with `RunResult::Detached`. The engine is then back in its start state and runs again on the next attach.
- 🚧 **PRL-4 Chunked RX**: multi-chunk extended messages are assembled with Chunk Requests. Missing:
  - one assembly at a time, not per SOP;
  - a multi-chunk message that starts while we wait for GoodCRC is not parked.
- 📋 **PRL-5 PD 2.0 mode**: N_RETRY 3 and PD 2.0 behaviour toward a revision 2.0 partner.
- 📋 **PRL-6 Collision avoidance**: SinkTxOk/SinkTxNG (Rp switching on the source, waiting on the sink), PD 3.2 Sec. 5.7. Portwright tracks it as portwright FW-54.
- 📋 **PRL-7 Cable plug role**: send SOP' and SOP'' messages as a cable plug (Cable Plug bit set), and send a Cable Reset. Needed for e-marker emulation (portwright API-27).

## Vendor defined messages (`VDM`)

- ✅ **VDM-1 Total VDM decoding**: VDM field conversions never panic on reserved values; they decode to `Reserved`/`Other` variants. `VendorDefined` (header plus up to six VDOs) parses and serializes (`usbpd/src/protocol_layer/message/data/vendor_defined.rs`).
- ✅ **VDM-2 VDM DPM hook**: `crate::vdm::VdmDevicePolicyManager`, part of `SinkDpm` and `SourceDpm`. In Ready, every VDM on any SOP goes to `vdm_received`, which replies, ignores or answers `Not_Supported` (the default). `Event::SendVdm(Sop, VendorDefined)` sends one. An unacknowledged VDM on a cable SOP calls `vdm_not_sent` instead of soft resetting the partner. The discovery and mode state machines are the DPM's (`usbpd/src/vdm.rs`).
- 📋 **VDM-3 VDM helpers**: typed builders and parsers for Discover Identity (ID header, Cert Stat, Product, product type VDOs), Discover SVIDs and Discover Modes responses.

## Messages (`MSG`)

- 🚧 **MSG-1 Extended and status messages**: Extended_Control and EPR capabilities are done. Missing: Status, Alert, Source/Sink_Capabilities_Extended, Revision, Source_Info, Manufacturer_Info, Battery_Status, PPS_Status.
- 📋 **MSG-2 BIST, Data_Reset, Enter_USB**.

## Sink (`SNK`)

- ✅ **SNK-1 Sink policy engine**: fixed, PPS (with keep-alive), AVS and EPR (entry, keep-alive), Get_Source_Cap, Get_Sink_Cap, soft and hard reset (`usbpd/src/sink/`).
- 🚧 **SNK-2 Wait handling**: SinkRequestTimer after `Wait` works; the rest of the `Wait` cases are incomplete.

## Source (`SRC`)

- ✅ **SRC-1 Source policy engine**: SPR and EPR source caps, Request, PPS, EPR entry, Get_Sink_Cap (`usbpd/src/source/`). Covered by host tests only.
- 📋 **SRC-2 Source cable discovery**: as VCONN source, Discover Identity to SOP' (DiscoverIdentityTimer) before offering more than 3 A or EPR. Today the EPR cable check is a DPM boolean.

## Dual role (`DRP`)

- ✅ **DRP-1 Dual-role port**: `DualRolePort` with PR_Swap, DR_Swap and VCONN_Swap through DPM hooks (`usbpd/src/dual_role.rs`). Starts over in its initial role after a detach.
- 🚧 **DRP-2 Fast role swap**: partly implemented; the detection is not wired in.

## Observation (`OBS`)

- 📋 **OBS-1 Observation hooks**: a tap on every sent and received frame (SOP, direction, outcome) and per-timer overrides, for Portwright's PD log and fault injection.
