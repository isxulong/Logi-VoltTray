# LogiVoltTray

**English** | [简体中文](README.md)（中文版为主位版本；两份 README 如有不一致即为缺陷，请到 [issues](https://github.com/isxulong/Logi-VoltTray/issues) 报告——一致性门禁见 [docs/experience/02-collaboration-playbook.md](docs/experience/02-collaboration-playbook.md) E44）

> **UI quotation convention（G-03）**: the tray UI is Chinese-only. Where this file quotes what the UI literally shows, the quotation follows the「中文字面量（英文意译）」convention — the on-screen Chinese string appears as-is, with an English gloss in parentheses (e.g. 100% shows 「满」 ("full"), not a literal "FULL"). These UI quotations share one source with the semantic ID table in [docs/experience/02-collaboration-playbook.md](docs/experience/02-collaboration-playbook.md) (E44); changes to a UI string update both READMEs and that table in the same change.

> **Translation scope（G-06，2026-10-31）**: this file is an abridged translation of the Chinese README. User-visible surfaces — features, menu entries, configuration keys, release information — are kept in full sync with the Chinese version under the E44 gate; internal-governance content (experience-base details, reconciliation mechanics, orchestration rules) is summarized or omitted. The acceptance criterion is paragraph-level structural alignment with the Chinese README; mechanical count checks (entry / line counts) are a candidate observation metric only, not a gate.

Show your Logitech device's battery level right on the Windows tray icon: the live battery percentage of your keyboard / mouse is drawn directly on the system tray icon — no windows to open, ever.

[![GitHub Release](https://img.shields.io/github/v/release/isxulong/Logi-VoltTray)](https://github.com/isxulong/Logi-VoltTray/releases)

Logitech's official software has no tray battery number — this app fills the gap: the battery percentage is painted straight onto the system tray icon, no window, no settings page.

<!-- TODO (item T-10, implementation order unverified item ⑧): embed the tray
     battery-number demo GIF here (≤3 s, looping, <2 MB, at
     docs/assets/volttray-demo.gif). Recording depends on the maintainer's
     physical Windows machine; once recorded, replace this comment with:
     ![Tray battery number demo](docs/assets/volttray-demo.gif) -->

**Download**: see the [latest release](https://github.com/isxulong/Logi-VoltTray/releases/latest) — grab `volttray-tray.exe` (see the Release page for the actual artifact size) and you're done; see the [Releases page](https://github.com/isxulong/Logi-VoltTray/releases) for older versions. Windows 10 / 11 (x64), double-click to run — no installer, no admin rights needed.

**Integrity check (optional but recommended)**: every release ships `volttray-tray.exe.sha256.txt`; compare it with `Get-FileHash .\volttray-tray.exe -Algorithm SHA256` in PowerShell. Since v0.3.23, releases additionally ship a `volttray-tray.exe.minisig` minisign signature (the pipeline self-verifies each signature with the public key right after signing — a failed verification fails the release). The public key is `minisign.pub` at the repo root; verify with `minisign -V -p minisign.pub -m volttray-tray.exe -x volttray-tray.exe.minisig` — output containing "Signature and comment signature verified" means the signature is valid.

> **An unsigned EXE may be blocked by Windows SmartScreen on first run** (the publisher has no reputation built up yet). If the blue "Windows protected your PC" prompt appears: click "**More info**" → "**Run anyway**" to allow it; only allow this one EXE of this program — do not loosen the system's blocking of other unknown programs based on this.

## Features

- **Tray battery display**: the battery percentage is drawn directly on the tray icon (100% shows 「满」 ("full"); genuinely unknown shows "?"; an offline device with a last-known reading shows that number grayed out, and "--" only appears when no reading was ever obtained). One device is displayed by default; the right-click menu lists one row per device — click to pin which one the tray shows.
- **Broad read channels**: devices behind HID++ 2.0 unified receivers (Unifying / Bolt / Lightspeed) and legacy HID++ 1.0 devices (direct register reads) are supported (verified on physical hardware); Bluetooth direct connection and wired connections are experimental channels (not verified on physical hardware) — see the "Experimental features (not verified on hardware)" section below; the battery source falls back automatically level by level (unified battery → battery status → voltage conversion → HID power descriptor).
- **Real device names**: shows real model names like "MX Keys" (read via the HID++ device-name feature or the receiver pairing-name register); if unreadable it falls back to a clean placeholder like 「USB Receiver · 槽位 1」 ("USB Receiver · Slot 1") — never a bare "?" as a name.
- **Tiered battery colors**: the icon color changes by battery tier — by default ≤20% red, ≤50% yellow, above that green; the number of tiers (2–5), thresholds, and colors are all customizable. Colors are not fixed per device type.
- **Low-battery blinking alert**: when the battery drops below the threshold the icon blinks at 1 Hz; the threshold is adjusted with a slider between 5%–50% (default 20%, step 5%), and threshold / toggle changes take effect immediately without waiting for the next polling round. Left-click the tray icon to stop the current blinking (right-click opens the menu, it does not stop blinking); the stop persists until the low-battery condition clears and the alert re-arms automatically; flipping the blink toggle from "off" back to "on" resumes blinking immediately; charging devices never blink or notify.
- **Fully customizable appearance**: icon shape (rounded / square / transparent, no background), background (dark / light / custom color), font, text size (40–160%), background opacity (20–100%) — every change in the settings window applies instantly.
- **Low-battery and stale-reading notifications**: Windows notifications fire when the battery crosses the threshold or a reading goes stale. The staleness threshold is anchored to each device's polling tier (minimum 90 s; the longer the tier, the longer the threshold — guaranteeing that two consecutive failed polls are required before a reading is declared stale). When stale, the last known battery is shown grayed out with a "battery display is stale" notification — no more false "device offline" alarms during normal use; sleep doesn't count as stale. The menu item "Notification mute (audio only)" only mutes the notification sound — the popup still fires, and icon blinking is not governed by it (that's the "Low-battery blinking alert" toggle). After startup, the first successful battery read immediately evaluates the low-battery condition and notifies if needed, without waiting for the next polling round.
- **Battery reading lag disclosure**: the battery number you see may be an old reading — the lag is usually no more than 2 hours (the longest polling tier is 7200 s = 2 h), and in extreme cases (device disconnected, sleep not yet detected, etc.) it can be longer; judge freshness by the reading's age. "Refresh now" in the right-click menu forces a query round at any time. The stats page KPI card already shows "updated X min ago" (with a warning badge when the reading has not refreshed for over 2 hours), and device readings in the right-click menu carry an "(X min ago)" age suffix, so you can judge freshness directly.
- **Start-with-Windows toggle**: just check / uncheck it in the right-click menu — it writes to the HKCU Run registry key (no admin needed). Changes on the menu, in settings, or in the config file all sync to the registry; write failures fall back automatically with a warning, so the checkmark can never diverge from the actual autostart state.
- **Adjustable polling interval**: five tiers — 300 / 600 / 1800 / 3600 / 7200 seconds (default 1800 s = 30 min; the longest tier is 7200 s = 2 h), changeable from the settings window dropdown. Per-device timers since v0.3.16: charging devices refresh every 15 s, unplugging is noticed within at most one charging tier; normal devices follow your tier; sleeping devices are not scheduled at all — zero queries. One device entering charge mode no longer raises the query frequency of other devices (the trade-off: while one device is charging, noticing that it was unplugged can take up to that device's own tier duration). Device enumeration refreshes automatically once per polling cycle (30 s is only the re-enumeration gate floor, not a routine every-30-seconds enumeration) — newly paired devices and receiver plug/unplug are picked up automatically.
- **Zero battery drain for sleeping mice**: after repeated query timeouts mark a mouse as asleep, the program stops issuing any active queries to it (sending queries to a sleeping mouse is what actually drains its battery); waking is detected via the system's passive Raw Input listener at zero radio cost. While asleep, the tray shows the last known battery grayed out — never as "unknown".
- **Device diagnostics export**: the right-click menu item "Export diagnostics…" exports a redacted diagnostics JSON (since v0.3.15 it opens a Save-As dialog — you choose the location and file name, the default name carries a local timestamp; the program never deletes or cleans up any exported file). Send this file when reporting "can't read" issues. The export contains: the full event stream of device discovery, handshakes, battery queries, and state changes; the most recent device enumeration snapshot; and up to the last 4096 lines of the run log (about 1.8 hours of context; the cap is `LOG_TAIL_MAX_LINES`) — which record settings applied (itemized diffs), blinking start / stop (including the reason), notifications sent / muted / failed, and more, so you usually don't need to dig through logs separately.
- **Single-instance guard**: a named mutex ensures only one instance runs; a duplicate launch pops an "already running" message and exits — you'll never get a stack of tray icons.
- **Frictionless interaction**: double-click the EXE to start; left-click the icon to stop low-battery blinking; the right-click menu covers every common action (refresh now, toggles, settings, diagnostics, exit); clicking Exit quits immediately — the IO thread waits in interruptible slices — commands can interrupt it at any time, never blocking the UI.

### Experimental features (not verified on physical hardware)

The features below have their core logic locked by unit tests and their Windows-side wiring verified by CI (windows-latest) builds, but their behavior on a physical machine is not yet verified; if something doesn't match the description, please export a diagnostics file via "Export diagnostics…" and report it.

- **Last-known battery shown immediately after restart** (since v0.3.16): each device's last battery reading is persisted to `%LOCALAPPDATA%\LogiVoltTray\last_known.json` (stored separately from the config file, not synced with roaming profiles; atomic writes, corrupt files fall back automatically without blocking startup). After a restart the last known battery is shown grayed out immediately (a virtual entry that issues no device queries — a menu row only appears if data exists); a final flush happens at shutdown, so a crash or power loss still preserves the most recent state. Device physical identity is the primary key, so identically named devices can't be confused across restarts.

- **Battery drain statistics and export** (since v0.3.16): read / sleep / charge events are appended line by line to `%LOCALAPPDATA%\LogiVoltTray\stats.jsonl` (10 MB per-file cap with rotation; total with archives capped at 20 MB; when nearing the cap a "stats history is about to rotate, consider exporting first" prompt appears before rotating). "Export battery stats…" from the right-click menu uses the same system Save-As dialog as diagnostics export: CSV is mandatory; you can also opt into a self-contained HTML visualization report (single file, zero external links, viewable offline by double-clicking — battery curves, daily drain comparison, duration breakdown, segment details, and 7-day active drain rate) and a raw JSON event sequence, plus optional anonymization (device keys shown as D1, D2…; real device names kept). Statistics attribute time into active / sleeping / charging / unknown buckets on a timeline; anomalies like time going backwards or gaps over 7 days are honestly flagged, and negative drain rates are never produced. The repo ships a `volttray/tools/recheck.py` recomputation script (pure Python standard library) for troubleshooting and acceptance: it independently recomputes the exported CSV / HTML / JSON against stats.jsonl to confirm the statistics are correct.

- **Per-device polling timers** (since v0.3.16): the new logic — charging devices refresh every 15 s, sleeping devices unscheduled with zero queries — is not yet verified on hardware; see the "Adjustable polling interval" feature above.
- **Bluetooth direct and wired device reading**: the code path is in place but not yet verified on physical hardware; if reads fail, please export a diagnostics file via "Export diagnostics…" and send it to us.

## Quick start

1. Download the EXE from the [Releases page](https://github.com/isxulong/Logi-VoltTray/releases) and put it anywhere.
2. Double-click to run — a battery icon appears in the tray; the number is the current battery of the default device.
3. **Right-click menu**:
   - Device section at top: one row per recognized device (with battery / charging / sleeping / offline status); click to pin it as the tray display;
   - "Refresh now (current interval N min / N h)": runs one battery query round immediately;
   - "Low-battery blinking alert", "Notification mute (audio only)", "Start with Windows": three instant toggles;
   - "Settings…", "Export diagnostics…", "Export battery stats…", "Check for updates (current vX)", "Exit" (the authoritative menu list and order are in `menu.rs`; the tray UI is Chinese-only, so these labels are translations; "Check for updates" landed in v0.3.24 with the dynamic "(current v{X})" suffix).
4. **Settings window** (changes apply instantly, no OK / Cancel buttons):
   - Appearance: icon shape, background, font color ("Auto" = tiered by battery, "Custom" = fixed color), font, text size, background opacity;
   - Tiers: tier count, thresholds, colors (color picker);
   - Polling interval: five-tier dropdown;
   - Low-battery alert: blink toggle + threshold slider (5–50%), changes effective immediately without waiting for a polling cycle.

## Config file

The config file lives at `%APPDATA%\LogiVoltTray\config.toml` — flat key-value pairs, no sections, auto-generated on first run.

- **Hot reload**: manual edits are picked up and applied within ~5 seconds of saving (polling by modification time, no device queries involved); changes from the settings window also write back to this file in real time — consistent both ways.
- **Fault tolerance**: unknown keys are ignored with a diagnostics warning; out-of-range values are clamped or snapped to the nearest legal tier — startup is never refused because of them.
- **Two more data files** (under `%LOCALAPPDATA%\LogiVoltTray\`, since v0.3.16, not synced with roaming profiles): `last_known.json` (last known battery, for instant display after restart) and `stats.jsonl` (battery-drain event stream, 10 MB rotating cap).

## Logs and diagnostics

Two things you'll use when troubleshooting "device not detected / tray shows ?":

- **Run log** `%APPDATA%\LogiVoltTray\logi-volttray.log`: records from the very first startup phase (including config load warnings and startup failure reasons) — even "double-click does nothing" instant-exit scenarios leave a trace. Settings applied (itemized diffs), blinking start/stop (with reason), notification dispatch / mute / failure events are all logged too.
- **Diagnostics export**: right-click menu "Export diagnostics…" → Save-As dialog → choose location, saves `LogiVoltTray-diag-export-<timestamp>.json` (since v0.3.15; earlier versions wrote to the config directory and popped the path): a redacted diagnostics event stream (serial numbers and the like masked; every channel's and every device slot's handshake and battery query results are recorded) + the most recent device enumeration snapshot + up to the last 4096 lines of the run log (about 1.8 hours of context). Please attach this file when reporting issues.
- **Privacy disclosure (since v0.3.20)**: starting with v0.3.20, diagnostics export automatically replaces the local username in exported text (the `C:\Users\<name>` path segment) with `<USER>`, and device names with "DeviceN" in order of appearance. Files exported by earlier versions may contain the local username and device names — please check and remove them before submitting. Also note (since v0.3.23): with the run-log tail cap raised to 4096 lines, an export carries roughly 20× as much run log as before (about 1.8 hours of context). The redaction rules are unchanged (username paths and device names are still covered), but the file is longer — before submitting to a public issue, check that the recorded operation details are all fine to make public.
- **Feedback channel**: please report issues via [issues](https://github.com/isxulong/Logi-VoltTray/issues) (a bug-report template guides you; attach the JSON file produced by "Export diagnostics…"). If the exported file came from a version before v0.3.20 it may contain the local username — please delete that field before submitting.

## Design philosophy

- **Zero mouse battery cost**: after sleep detection there are zero active queries; waking relies purely on passive system-level listening (Raw Input), and the polling interval can be stretched to 2 hours — the program's own drain is negligible, and it will never wake a mouse just to read its battery.
- **Single HID IO thread**: all device reads and writes are serialized on one IO thread, eliminating multi-thread contention over the same HID channel; cross-thread communication goes through channels only — the GUI never touches devices directly.
- **Pure protocol codec**: the `volttray-protocol` crate does no I/O and doesn't depend on system time — pure framing / deframing and mapping logic, thoroughly verifiable with golden samples and proptest.

## Building from source

The main project is in the `volttray/` subdirectory (Rust workspace), Windows only.

```powershell
git clone https://github.com/isxulong/Logi-VoltTray.git
cd Logi-VoltTray\volttray
cargo build --release --locked -p volttray-tray
```

The output is `volttray\target\release\volttray-tray.exe`. Requires Rust stable (the repo's `rust-toolchain.toml` pins 1.90.0; `rustup` installs it automatically) and an MSVC build environment.

**Cloud builds**: pushing to `main` triggers CI (windows-latest): `cargo fmt --check` (mismatch fails; the author fixes locally), `clippy -D warnings`, full tests, `cargo build --release --locked` with the EXE uploaded as an artifact; the installers on the Releases page come from this pipeline. Without a local Rust environment, fork and push — then grab the EXE from the cloud.

## Experience knowledge base

[docs/experience](https://github.com/isxulong/Logi-VoltTray/tree/main/docs/experience) accumulates this project's real engineering lessons from actual iterations, collaboration and process methodology, the release quality gate checklist, and onboarding guidance for newcomers (including newly joined AIs): if you want to understand the hardware-proven lessons behind a design decision, what to check item by item before a release, or how to get up to speed quickly, start from the navigation table in [docs/experience/README.md](https://github.com/isxulong/Logi-VoltTray/blob/main/docs/experience/README.md).

## License

MIT (see the `LICENSE` file at the repo root). Logitech-related trademarks belong to their respective owners; this project is not affiliated with Logitech in any way.
