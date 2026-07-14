# Remote interface registration deadlocks in steamos-manager (v26.3.0)

## Summary

While integrating an external TDP/GPU daemon through the `remotes.d`
mechanism, I hit two distinct deadlocks in the user daemon's remote
interface machinery. Both freeze the user daemon before or during
remote registration. A per-interface-bus-name workaround is included
at the end, which also confirms the diagnosis.

Environment:

- steamos-manager 26.3.0 (Arch Linux package, vanilla, no patches)
- Arch Linux, dbus-broker, systemd user session (gamescope session)
- Remote implementation: external Rust daemon using zbus 5, exposing
  `com.steampowered.SteamOSManager1.TdpLimit1` and
  `com.steampowered.SteamOSManager1.GpuPerformanceLevel1` on the
  system bus (structure identical to `examples/basic_remote.rs`)
- No device config for this hardware (GPD Win Max 2), so
  `tdp_limit_manager()` returns the default
  `RemoteInterfaceLimitManager` and no local GPU interfaces are
  instantiated — both interfaces are free for remotes, as designed.

## Bug 1: user daemon deadlocks at startup if a TdpLimit1 remote is already on the bus

### Reproduction

1. Install a `remotes.d` TOML declaring a `[TdpLimit1]` remote.
2. Start the remote daemon; it owns its bus name on the system bus.
3. `systemctl --user restart steamos-manager`.

Result: the unit stays in `activating` forever and is killed by the
systemd start timeout. Last journal lines are the
`create_device_interfaces` warnings; `READY=1` is never sent. The
session bus name is claimed but the object never answers
(`busctl --user introspect com.steampowered.SteamOSManager1 ...`
times out).

If the remote daemon is *not* running when steamos-manager starts,
startup completes normally, and the remote is picked up later through
the NameOwnerChanged auto-add path (which is subject to Bug 2 below).

### Analysis

In `daemon/user.rs`, the startup order is:

1. `TdpManagerService::new(tdp_rx, ...)` — constructs the service,
   but its `run()` loop (the only consumer of the
   `TdpManagerCommand` channel) is not running yet;
2. `create_interfaces(...)` — this calls
   `remote_interface.configure(...)`, which, when the remote's bus
   name already has an owner, proceeds through
   ping → proxy build → `register_tdp_limit1()`;
3. `daemon.add_service(tdp_service)` + `daemon.run()` — only here
   does the `TdpManagerService` start consuming the channel.

`register_tdp_limit1()` sends `TdpManagerCommand::SetProxy(...)` and
then awaits the oneshot reply. Since the channel consumer only starts
in step 3, and step 2 never returns, this is a permanent deadlock:
the daemon blocks inside `create_interfaces` before `READY=1`.

The `autoadd_late_remote` path works precisely because by the time
`load()` runs, the TDP manager service loop is already active.

### Suggested fix

Either start the `TdpManagerService` (or at least its channel
consumer) before `create_interfaces`, or make the
`register_tdp_limit1` hook not block on the reply (fire-and-forget
SetProxy, or defer it).

## Bug 2: two remote interfaces sharing one bus name deadlock the daemon on (late) registration

### Reproduction

1. `remotes.d` TOML declaring **both** `[TdpLimit1]` and
   `[GpuPerformanceLevel1]` with the **same** `bus_name`.
2. Start steamos-manager with the remote daemon *stopped* (avoids
   Bug 1); startup completes normally, `Starting tdp-manager` is
   logged.
3. Start the remote daemon (single connection owning the single bus
   name, serving both interfaces on one object path).

Result: journal shows
`Interface com.steampowered.SteamOSManager1.GpuPerformanceLevel1 loading`
and then nothing. The session object becomes unresponsive
(introspection times out — consistent with the `load_task` holding
the `RemoteInterface1` write guard forever). The TdpLimit1 load is
never even attempted (queued behind the same guard).

### zbus trace of the freeze

With `RUST_LOG=steamos_manager=debug,zbus=trace`, the last activity
of the loading task is:

```
DEBUG steamos_manager::manager::user: Interface com.steampowered.SteamOSManager1.GpuPerformanceLevel1 loading
TRACE zbus::connection::socket: Sending message: Msg { type: MethodCall, serial: 27, path: "/", iface: org.freedesktop.DBus.Peer, member: Ping }
TRACE socket reader: Message received on the socket: Msg { type: MethodReturn, sender: ":1.187", reply-serial: 27 }
TRACE socket reader: Broadcasted to all streams: Ok(...)
[no further activity from this task]
```

The remote answers the ping; the reply is received and broadcast; but
the task never resumes — notably, none of the wire traffic expected
from the subsequent proxy build (AddMatch / GetNameOwner / GetAll,
as seen e.g. for the CecDaemon Config1 proxy earlier in the same
trace) ever appears. The freeze happens with a second `load_task`
(for the other interface, watching the same NameOwnerChanged) blocked
on the same `RemoteInterface1` `get_mut()`.

### Isolation matrix

All tests on the same machine, same remote daemon, late-add path
(daemon started first, remote second):

| remotes.d configuration                          | Result   |
| ------------------------------------------------ | -------- |
| `[TdpLimit1]` only                               | works    |
| `[GpuPerformanceLevel1]` only                    | works    |
| both interfaces, one shared bus name             | deadlock |
| both interfaces, one bus name each, staggered    | works    |

(The cosmetic `WARN Lost signal` right after a successful TdpLimit1
registration might be worth a look too.)

### Workaround (and confirmation of the diagnosis)

Giving each interface its own bus name, and only requesting the
second name after the first interface is visible in the user daemon's
introspection, registers both interfaces reliably
(`RemoteInterfaces` = 2, both usable from the Steam client). This
serializes the `load()` invocations, which is why I believe the root
cause is the concurrent execution of the per-interface load tasks
triggered by a single NameOwnerChanged.

## Notes

Happy to provide the full zbus trace, the remote daemon source, or to
test patches on this hardware.
