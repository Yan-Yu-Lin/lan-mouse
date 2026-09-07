# Hotkey switching (Mac → Hyprland)

This fork's `feature/hotkey-switching-hud` branch adds opt-in hotkey-only switching,
explicit hotkey entry acknowledgements, destination centering, and a Mac HUD.
The existing uinput/keyd and Karabiner F19 keyboard path is retained.

## Behavior

- Right Cmd + Backspace switches using the existing Karabiner bindings.
- `hotkey_only = true` disables automatic Mac edge entry and Linux return barriers.
  This mode currently targets a macOS sender and a Linux receiver.
- Hotkey return centers the Mac cursor; edge/disconnect releases do not center it.
- Entry retries every 75 ms without needing movement. After 3 seconds without a
  matching acknowledgement the Mac recovers local input and displays failure.
- Each hotkey entry has a nonzero serial. Duplicate packets are acknowledged without
  re-centering; stale entries/acknowledgements cannot complete a newer switch.
- Up to 256 initial input events are buffered during entry. Overflow aborts the
  switch and recovers local input, rather than replaying an unbounded backlog.
- The Mac releases capture before bounded remote key cleanup. Timing logs contain
  transition metadata, never typed content.
- The switch hook runs in the capture task, in order: `connecting` (set raw keys),
  `remote` (matching acknowledgement), `local`, `error`, or silent `reset`.
- The HUD never takes focus or clicks. It is a persistent 46×34 pt monochrome
  keyboard badge: green for Mac, red for Linux. It changes on confirmed entry,
  keeping its previous color while connecting. No "Controlling…" text.
- Sound is enabled by default (Linux: Pop, Mac: Tink at 55% app volume).
  Set `LAN_MOUSE_SOUND=0` on the HUD launch agent to mute it. Duplicate states
  and HUD startup do not beep. The last confirmed state survives HUD restarts.

## Configuration

Both daemons must be rebuilt: protocol crate 0.4 introduces additive event ID 12,
`HotkeyEnter(position: u8, serial: u32)`. Existing wire IDs remain unchanged.
Older receivers do not understand hotkey entry and will time out; edge mode keeps
its existing wire format. Switch-mode configuration requires a daemon restart.

Mac (top-level, before any TOML table):

```toml
hotkey_only = true
switch_hook = "/Users/linyanyu/.local/bin/lm-feedback"
```

Remove the old per-client `enter_hook` and `leave_hook` from this deployment: their
asynchronous scripts would compete with the ordered switch hook. Keep the existing
client, fingerprints, release chord, and port.

Linux (top-level):

```toml
hotkey_only = true
receive_enter_hook = "/home/arthur/.local/bin/lm-center-hyprland"
```

Keep `capture_backend = "layer-shell"` and `emulation_backend = "uinput"`.
The receive hook runs once per new hotkey entry, before acknowledging readiness.
It must succeed within one second. `lm-center-hyprland.py` selects the focused,
awake monitor and uses its scale, rotation and origin to compute logical center.
The [Hyprland cursor dispatcher](https://wiki.hypr.land/Configuring/Basics/Dispatchers/#cursor)
is `hl.dsp.cursor.move({ x, y })`. `--dry-run` prints coordinates without moving.

## Components and builds

- `lm-switch` → `~/.local/bin/lm-switch`
- `lm-feedback` → `~/.local/bin/lm-feedback`
- `swiftc -O tools/switching/LanMouseHUD.swift -o <staging>/LanMouseHUD`
  → `~/.local/lib/lan-mouse/LanMouseHUD`, persistent LaunchAgent.
- Linux helper → `~/.local/bin/lm-center-hyprland` (PEP 723 script, uses `uv`).
- Mac daemon: `cargo build --release --no-default-features`. Preserve the app's
  existing `Lan Mouse Local Signing` certificate and launchd identity.
- Linux daemon: `cargo build --release --no-default-features --features
  layer_shell_capture,wlroots_emulation,uinput_emulation`.

Do not run both old and new daemons together. Back up app binary, signing identity,
configuration, switch script and Linux service before replacing them. Restore all
of those together to roll back; the original switch script depends on the old hooks.

## Verification

Run Rust tests for the daemon and protocol crate, and
`uv run tools/switching/test_center.py`. Run clippy for each deployed feature set;
full workspace checks also require GTK development libraries.

Live checks: edge movement remains local; hotkey centers each destination; normal
keys/Caps layers still work; rapid toggles end in the correct state; failed entry
returns local controls; red/green badge and sound agree with actual destination.
Live checks affect input, so coordinate them with the person using the keyboard.
