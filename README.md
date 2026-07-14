# hhd-steamos-bridge

A small D-Bus daemon that lets a **vanilla steamos-manager** control TDP and
GPU clock through [Handheld Daemon (HHD)](https://github.com/hhd-dev/hhd),
using steamos-manager's official `remotes.d` mechanism.

With this bridge, the Steam client's TDP and GPU clock sliders (gamescope
session) work on handhelds that steamos-manager doesn't support natively —
no patched steamos-manager, no fork, no downstream package to maintain.

```
Steam client ──▶ steamos-manager (vanilla, session bus)
                      │  remotes.d relay
                      ▼
              hhd-steamos-bridge (system bus)
                      │  hhd.steamos CLI
                      ▼
             Handheld Daemon (TDP / GPU control)
```

Tested on a GPD Win Max 2 (Ryzen AI 9 HX 370) running Arch Linux with a
gamescope session, steamos-manager 26.3.0 and HHD with the Adjustor TDP/GPU
plugins enabled.

## How it works

steamos-manager supports *remote interfaces*: an external daemon can
implement selected `com.steampowered.SteamOSManager1.*` interfaces on the
system bus and register them through a TOML file in
`/etc/steamos-manager/remotes.d`. steamos-manager then relays the D-Bus
traffic between the Steam client and the remote. Remotes can only fill
holes — on hardware without a steamos-manager device config, both
`TdpLimit1` and `GpuPerformanceLevel1` are unimplemented locally, so the
bridge provides them.

The bridge implements:

- `com.steampowered.SteamOSManager1.TdpLimit1`
  (`TdpLimit`, `TdpLimitMin`, `TdpLimitMax`)
- `com.steampowered.SteamOSManager1.GpuPerformanceLevel1`
  (`AvailableGpuPerformanceLevels`, `GpuPerformanceLevel`,
  `ManualGpuClock`, `ManualGpuClockMin`, `ManualGpuClockMax`)

Every property read/write is translated into `hhd.steamos steamos-tdp ...`
or `hhd.steamos steamos-gpu ...` calls, following the same mapping as
Bazzite's steamos-manager fork (including the `GpuPerformanceLevel` =
`"manual"` trick so that Steam explicitly re-triggers `auto`, which maps to
`steamos-gpu clear`).

### Why two bus names?

steamos-manager 26.3.0 has two deadlocks in its remote registration path
(see [`docs/UPSTREAM-BUG-REPORT.md`](docs/UPSTREAM-BUG-REPORT.md)):

1. A `TdpLimit1` remote already present on the bus when the user daemon
   starts deadlocks the daemon before it signals `READY=1`.
2. Two remote interfaces sharing one bus name deadlock the daemon when the
   name appears (two concurrent load tasks triggered by the same
   `NameOwnerChanged`).

The bridge works around both by design:

- it exposes each interface on its **own bus name**
  (`com.steampowered.HhdBridge.Tdp` and `com.steampowered.HhdBridge.Gpu`),
  and only requests the GPU name after confirming, via the session bus,
  that `TdpLimit1` has been registered by the daemon — this serializes the
  two registrations;
- the provided systemd **user** unit orders the bridge strictly after
  `steamos-manager.service` (`After=` + `BindsTo=`), so the bus names never
  exist while the daemon is starting up.

## Requirements

- steamos-manager with the `remotes.d` mechanism (26.x; tested with 26.3.0)
  running as your user session daemon
- HHD with working `hhd.steamos` CLI (`hhd.steamos steamos-tdp get` must
  succeed as your regular user; the `/run/hhd/api` socket is world-writable
  by default)
- Rust toolchain to build (`rustc`/`cargo`)

## Installation

```sh
cargo build --release
sudo install -Dm755 target/release/hhd-steamos-bridge /usr/local/bin/hhd-steamos-bridge

# D-Bus policy — EDIT THE FILE FIRST and replace the user= with your username
sudo install -Dm644 dist/com.steampowered.HhdBridge.conf \
    /etc/dbus-1/system.d/com.steampowered.HhdBridge.conf
sudo systemctl reload dbus

# Remote registration (must exist before steamos-manager starts)
sudo install -Dm644 dist/hhd.toml /etc/steamos-manager/remotes.d/hhd.toml

# systemd user unit
sudo install -Dm644 dist/hhd-steamos-bridge.service \
    /etc/systemd/user/hhd-steamos-bridge.service
systemctl --user daemon-reload
systemctl --user enable hhd-steamos-bridge.service

systemctl --user restart steamos-manager   # the bridge follows automatically
```

Then restart the Steam client (it only probes available interfaces at
startup). The TDP and GPU clock sliders should appear in the gamescope
session's performance panel.

## Verification

```sh
systemctl --user status hhd-steamos-bridge
journalctl --user -u hhd-steamos-bridge -f    # logs every TDP/GPU change

busctl --user introspect com.steampowered.SteamOSManager1 \
    /com/steampowered/SteamOSManager1 | grep -E "TdpLimit1|GpuPerformanceLevel1|RemoteInterfaces"
```

`RemoteInterfaces` should list both interfaces. Moving the sliders in Steam
should print `TDP -> N W` / `GPU -> N MHz` in the bridge journal and be
reflected in the HHD overlay.

## Notes and caveats

- `hhd.steamos` does not expose the *current* TDP value (only
  min/max/default), so `TdpLimit` reports the last value written through
  the bridge, initialized to HHD's default TDP.
- If HHD's GPU control is disabled when the bridge starts, only `TdpLimit1`
  is exposed; restart the bridge after enabling it
  (`systemctl --user restart hhd-steamos-bridge`).
- Don't run the bridge manually while the user unit is active — they would
  compete for the bus names.
- The startup ordering provided by the systemd unit is required as long as
  the upstream deadlocks are not fixed.

## Credits

- Based on Valve's `examples/basic_remote.rs` from
  [steamos-manager](https://gitlab.steamos.cloud/holo/steamos-manager) (MIT)
- `hhd.steamos` mapping follows
  [Bazzite's steamos-manager fork](https://github.com/bazzite-org/steamos-manager)
- [Handheld Daemon](https://github.com/hhd-dev/hhd) by hhd-dev

## License

MIT — see [LICENSE](LICENSE).
