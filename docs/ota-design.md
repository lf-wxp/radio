
# OTA Firmware Update — Technical Design

> Status: **Phase 1 in progress** — partition table + flash hand-off shipped
> Author: esp-radio maintainers
> Last updated: 2026-06-27
> Tracking: Roadmap item *"OTA firmware update via HTTP/HTTPS"*

This document captures the design, the open questions and the actionable
work-breakdown for adding **Over-The-Air firmware updates** to the
`esp-radio` project. It is intentionally written *before* coding begins so
that the implementation can be picked up incrementally without re-doing the
investigation.

---

## 1. Goals & Non-Goals

### 1.1 Goals
- Allow the device to fetch a new application image from a configurable
  HTTP(S) URL and boot into it on the next reset.
- Provide UI feedback (progress %, success/failure) during the update.
- Support **safe rollback**: if the new image fails to mark itself valid
  within N boots, the bootloader reverts to the previous slot automatically.
- Be triggerable from the existing input devices (rotary-encoder long
  press) **and** from a future companion app via Wi-Fi.

### 1.2 Non-Goals (deferred)
- Delta updates (`esp_delta_ota`-style).
- Code signing / secure boot. (Will be tracked separately once the
  bootloader supports it on `esp-hal` targets.)
- Background download while audio is playing — first cut blocks the radio
  task during download.

---

## 2. Background — Current State of the Project

| Concern                          | Current state                                                          |
| -------------------------------- | ---------------------------------------------------------------------- |
| Bootloader                       | `esp-bootloader-esp-idf 0.5.0` (already in `Cargo.toml`)               |
| Partition layout                 | **`partitions.csv` shipped 2026-06-27** (ota_0 / ota_1 / otadata)      |
| Flash access                     | `esp-storage 0.9.0`; handle threaded WiFi → presets, OTA borrows it    |
| Network stack                    | `embassy-net` + esp-radio Wi-Fi (provisioned via `wifi_provision`)     |
| HTTP client                      | **None.** `picoserve` is server-only.                                  |
| TLS                              | None.                                                                  |
| App descriptor (`esp_app_desc`)  | Emitted by `esp-bootloader-esp-idf::esp_app_desc!` (already used).     |
| OTA primitives                   | `esp-bootloader-esp-idf::ota::Ota` + `OtaUpdater` (no need to re-roll) |

The two structural blockers were:

1. ~~No partition table capable of holding two app slots.~~ ✅ Resolved: see `partitions.csv` at the repo root.
2. ~~Flash handle ownership.~~ ✅ Resolved: see § 4.3. The flash
   handle is already passed `WifiProvisioner` → `PresetStore` (via
   `into_flash()`); OTA further borrows it from `PresetStore` via the
   new `pause()` / `resume()` API.

---

## 3. Partition Table

### 3.1 Layout (proposed `partitions.csv`)

```csv
# Name,   Type, SubType, Offset,   Size,     Flags
nvs,      data, nvs,     0x9000,   0x6000,
phy_init, data, phy,     0xf000,   0x1000,
otadata,  data, ota,     0x10000,  0x2000,
ota_0,    app,  ota_0,   0x20000,  0x1E0000,
ota_1,    app,  ota_1,   0x200000, 0x1E0000,
storage,  data, nvs,     0x3E0000, 0x20000,
```

- 4 MiB flash assumed (ESP32-C6 typical). Adjust `storage` offset/size on
  larger boards.
- `nvs` block reserved for credentials; **migration plan** (§ 7.2)
  guarantees existing devices keep their Wi-Fi config.
- Two ~1.875 MiB app slots leave headroom for the current firmware
  (≈ 1.1 MiB release build) plus growth.

### 3.2 Tooling

- Generate via `espflash partition-table` or rely on
  `esp-bootloader-esp-idf::PartitionTable` runtime parser.
- Wire into `cargo make build-release` & `cargo make run-release` via
  `--partition-table partitions.csv` flag (espflash 4.x supports it).

---

## 4. Architecture

### 4.1 Module layout

```
src/
├── ota/
│   ├── mod.rs        // public API: trigger, progress channel, errors
│   ├── http.rs       // streaming HTTP(S) downloader (reqwless wrapper)
│   ├── writer.rs     // chunked writer over PartitionTable + esp-storage
│   └── verify.rs     // app_desc parsing + magic/CRC sanity checks
├── bin/radio/
│   ├── tasks.rs      // new ota_task() consuming RadioCommand::StartOta
│   └── ui.rs         // progress overlay
```

### 4.2 Data flow

```mermaid
sequenceDiagram
    participant UI
    participant CmdChan as Command channel
    participant OTA as ota_task
    participant Net  as embassy-net
    participant Flash as PartitionTable / FlashStorage
    participant Boot  as esp-bootloader

    UI->>CmdChan: RadioCommand::StartOta(url)
    CmdChan->>OTA: poll
    OTA->>Net: TCP connect + HTTP GET
    loop chunked download (4 KiB)
        Net-->>OTA: bytes
        OTA->>Flash: write to inactive slot
        OTA-->>UI: progress = bytes / total
    end
    OTA->>OTA: parse esp_app_desc, verify magic
    OTA->>Boot: set_pending_verify(slot)
    OTA-->>UI: status = ok, restart in 3s
    OTA->>OTA: software_reset()
```

### 4.3 Flash-handle sharing — "pause / resume" hand-off

**Decision (2026-06-27, revised):** the original draft proposed a
global `Mutex<NoopRawMutex, FlashStorage>` shared by every writer. We
rejected that in favour of an explicit single-owner hand-off because
the project already threads the flash handle through unique owners:

```
FlashStorage::new(FLASH)
  └─► WifiProvisioner::new(flash)
        └─► provisioner.into_flash()  ──►  PresetStore::open(flash)
              └─► (long-lived owner inside radio_control_task)
```

OTA happens at most a handful of times per device-month, while the
preset store writes once per tune session. Forcing every preset write
to acquire a mutex just to support a rare OTA is the wrong trade-off.

Instead the preset store gains a small "pause" API:

```rust
impl PresetStore<'d> {
    pub fn pause(self) -> (FlashStorage<'d>, PausedPresetStore);
}
impl PausedPresetStore {
    pub fn resume<'d>(self, flash: FlashStorage<'d>) -> PresetStore<'d>;
}
```

…paired with a `RadioState.ota_in_progress` interlock so the radio
control task suspends `last_tuned` debounce flushes while the handle
is loaned out (see `flush_last_tuned_if_due` in `tasks.rs`).

Refactor surface (actual diff that landed):

- `presets.rs` (+~80 LoC): `pause`, `resume`, `PausedPresetStore`,
  documentation.
- `state.rs` (+~30 LoC): `ota_in_progress` field +
  `publish_ota_in_progress` helper.
- `tasks.rs` (+10 LoC): `flush_last_tuned_if_due` short-circuits when
  `RADIO_STATE.ota_in_progress` is set.

This is roughly a quarter of the size of the original `Mutex`-based
proposal and keeps the steady-state lock-free.

---

## 5. Public API Sketch

```rust
// src/ota/mod.rs
pub struct OtaUpdater<'a> {
    flash: &'a Mutex<NoopRawMutex, FlashStorage>,
    pt:    PartitionTable<'static>,
}

#[derive(defmt::Format)]
pub enum OtaError {
    Connect, Http(u16), Truncated, BadMagic,
    AppDescMismatch { found: heapless::String<32> },
    Flash, NoFreeSlot,
}

#[derive(defmt::Format, Clone, Copy)]
pub enum OtaProgress {
    Connecting,
    Downloading { written: u32, total: u32 },
    Verifying,
    Switching,
    Done,
    Failed(OtaError),
}

impl<'a> OtaUpdater<'a> {
    pub async fn run(
        &mut self,
        stack: &Stack<'_>,
        url: &str,
        progress: &Channel<NoopRawMutex, OtaProgress, 4>,
    ) -> Result<(), OtaError> { /* … */ }
}
```

`RadioCommand::StartOta(heapless::String<256>)` is added to the existing
command channel; the response funnels into `RadioState.ota_progress`.

---

## 6. Dependencies to Add

| Crate                  | Version       | Notes                                                        |
| ---------------------- | ------------- | ------------------------------------------------------------ |
| `reqwless`             | `0.13`        | `no_std`, async, supports streaming bodies.                  |
| `embedded-tls`         | `0.18`        | Required only if HTTPS is enabled (feature `tls`).           |
| `embedded-io-async`    | already in    | Re-exported by reqwless; no new entry.                       |
| `crc`                  | `3.x`         | Optional integrity check before commit.                      |
| `heapless`             | already in    | For URL & error strings.                                     |

> Open question: `reqwless` + `embassy-net` + esp-radio 0.18 stack has had
> some flakiness around DNS retries. We will land an integration test in
> `examples/` first.

---

## 7. Risks & Mitigations

### 7.1 Bricked device on bad image
- **Risk:** new image hangs early → no `mark_app_valid_cancel_rollback`.
- **Mitigation:** rely on `esp-bootloader-esp-idf` rollback; require the
  application to call `Ota::mark_current_valid()` only after the UI thread
  has rendered a frame and Wi-Fi is up.

### 7.2 Existing device migration (Wi-Fi credentials)

- The radio's WiFi credentials are stored in the **last sector of the
  flash chip** (`0x3F_F000`), set by
  `wifi_provision::storage::DEFAULT_STORAGE_OFFSET`.
- The new `partitions.csv` places `storage` at `0x3E_0000`–`0x400000`,
  which **includes** that exact sector; existing devices therefore
  retain their saved Wi-Fi config across the partition-table change
  with **no migration step required**.
- The radio's preset record (`0x3E_0000`, see
  `presets::DEFAULT_PRESET_OFFSET`) likewise lives inside the new
  `storage` partition and is preserved.
- **Caveat:** the *first* flash with the new partition table still
  needs to write the bootloader + partition table itself, so it is a
  one-shot full re-flash. Subsequent firmware bumps are the regular
  OTA path with no end-user friction.

### 7.3 HTTPS certificate management
- Embedding a CA bundle is heavyweight; pinning a single certificate is
  fragile.
- **Mitigation (phase 1):** ship HTTP-only with a documented warning;
  HTTPS gated behind `--features ota-tls` and a single pinned root cert.

### 7.4 Flash wear during repeated retries
- Each failed attempt erases the inactive slot.
- **Mitigation:** require `Content-Length`; refuse to begin if length > slot.
  Cap automatic retries at 3 per session.

### 7.5 Concurrent flash access
- `wifi_provision` and the preset store both write to the `storage`
  partition; the OTA writer needs the *whole* flash handle to populate
  an inactive app slot.
- **Mitigation:** see § 4.3. The `pause` / `resume` hand-off plus the
  `ota_in_progress` interlock keeps every writer serialised without
  introducing a runtime mutex on the hot path.

---

## 8. Work Breakdown (estimated, ordered)

| #  | Task                                                                          | Est. (h) | Status   |
| -- | ----------------------------------------------------------------------------- | -------- | -------- |
| 1  | Add `partitions.csv` + flash hand-off (`pause`/`resume`) + state interlock    | 4        | ✅ done  |
| 2  | Skipped — `esp-bootloader-esp-idf` already provides `Ota` / `OtaUpdater`      | 0        | n/a      |
| 3  | New `src/ota/writer.rs` thin wrapper over `OtaUpdater::next_partition`        | 2        | pending  |
| 4  | New `src/ota/http.rs` — reqwless wrapper, header parsing, retry policy        | 6        | pending  |
| 5  | New `src/ota/verify.rs` — SHA-256 check + `esp_app_desc` sanity               | 3        | pending  |
| 6  | Hook `RadioCommand::StartOta` into `tasks.rs`, progress channel               | 3        | pending  |
| 7  | UI overlay (Slint) for progress %, success/failure                            | 4        | pending  |
| 8  | `mark_app_valid_cancel_rollback` on healthy boot                              | 2        | pending  |
| 9  | HTTPS feature flag + pinned-cert support (`embedded-tls`)                     | 6        | deferred |
| 10 | E2E hardware test: flash A → OTA to B → reboot → OTA back to A                | 4        | pending  |
| 11 | Docs: README updates, migration note, troubleshooting                         | 2        | pending  |
|    | **Total (excluding deferred TLS)**                                            | **24**   |          |

> ≈ 3 working days for a single engineer with the hardware on hand —
> down from the original 5-day estimate now that we get to skip the
> `Mutex` refactor and reuse the upstream OTA primitives.

---

## 9. Open Questions

1. Should the firmware advertise its current version on the local network
   (mDNS TXT record) so a companion app can detect upgrades automatically?
2. Do we want progressive rollout via a *manifest* (`latest.json` listing
   per-board URLs and SHA-256), or is a single hard-coded URL good enough
   for v1?
3. Where does the build pipeline publish artifacts? (GitHub Releases is
   the obvious candidate; needs CI changes outside this repo.)

---

## 10. Decision Log

- **2026-06-25** — Implementation deferred. This document is the canonical
  reference; revisit before starting any OTA-related coding work.
- **2026-06-27** — Phase 1 landed (#11-1):
  - Added `partitions.csv` (4 MiB layout with `ota_0` / `ota_1`).
  - Replaced the proposed `Mutex<FlashStorage>` with an explicit
    `PresetStore::pause()` / `resume()` hand-off + `ota_in_progress`
    interlock; ~30 % of the original LoC budget.
  - Decided to reuse `esp-bootloader-esp-idf::ota::Ota` /
    `OtaUpdater` instead of writing a custom `src/ota/writer.rs`,
    cutting Phase 2 effort from 4 h to 2 h.
  - Documented that existing devices keep their Wi-Fi credentials
    across the partition-table change (the credential sector at
    `0x3F_F000` already falls inside the new `storage` partition).
