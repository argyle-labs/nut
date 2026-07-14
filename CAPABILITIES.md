# nut — ServiceBackend contract

Pure-Rust plugin (**no bash/compose/provision scripts**) driven by the single
generic `service.*` surface — no per-plugin tools. Runtimes: **host,docker,lxc**.

## Per-plugin code (the only work this repo owns)
- [x] `provider` / `runtimes` / `default_port` / `capabilities` / `data_paths` — declarative descriptor
- [x] `workload_spec(runtime)` — *what* to run; `deploy_target` renders it to a container / LXC / VM (netserver + monitor-only client modes)
- [x] `configure` — render `ups.conf`/`upsd.conf`/`upsmon.conf`/`upssched.conf` idempotently (multi-UPS + monitor-only)
- [x] `status` — health + per-UPS summary (status/charge/runtime/voltage) from the `upsd` client

> Declarative descriptor is implemented and the plugin **registers + loads live**
> in orca today (`service.list` shows it). Lifecycle
> (`workload_spec`/`configure`/`status`) is implemented.

## Diagnostics provider (`nut.__diag.{diagnose,repair}`)
Besides the service backend, nut contributes a `diagnostics` provider that detects
and repairs UPS-side power-loss conditions. Detects the active subsystem
(apcupsd vs NUT) first. Checks:

| id | detects | repair |
|----|---------|--------|
| `apcupsd-active` | apcupsd managing the UPS instead of NUT | migrate to NUT |
| `ups-comms` | `upsd` unreachable / UPS not reporting (`Crit`) | restart/redeploy NUT |
| `battery-thresholds` | `MINSUPPLIES`/timing unsafe vs reported runtime | set safe thresholds |
| `kill-power` | `POWERDOWNFLAG`/kill-power not set | enable kill-power |
| `shutdown-command` | `SHUTDOWNCMD` not force-unmount-aware | **delegates** to unraid `shutdown-timeout` |
| `persistent-event-log` | events log to tmpfs only / no remote | flash (`/boot`) + remote syslog |
| `onbattery-capture` | no ONBATT snapshot before shutdown | install ONBATT capture hook |

Config-writing repairs are `privileged`, non-`automatic` (suggest-then-confirm), and
idempotent. `Ok`/`Info` findings carry no repair.

## Provided generically by orca (NO code here)
- `deploy` — `service.deploy` → `deploy_target.launch(WorkloadSpec)`
- `backup` / `restore` — pluggable `BackupMethod` (tar; **PBS** for Proxmox guests)
- single `service.*` tool surface, exposed over CLI / REST / MCP
