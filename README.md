<p align="center">
  <img src="assets/icon-256.png" width="120" alt="nut" />
</p>

# nut

Network UPS Tools (NUT) monitors UPS hardware and exposes it over the network.

A first-party [orca](https://github.com/argyle-labs/orca) plugin (service-backend).

This repo is **self-contained** — the steps below run nut **by hand, without orca**. orca automates exactly this (same image, ports, and data) through one generic surface.

---

## Run it without orca

### Docker / Podman

The right image depends on your host/hardware — see the upstream install docs: <https://networkupstools.org/>. Expose port `3493` and persist the config volume.

### Ports & data

| | |
|---|---|
| Default port | `3493` |
| Upstream | <https://networkupstools.org/> |


### Backup & restore

Back up the config/data volume(s) above — that's the whole service state (stop the container first for a clean copy). Restore by putting them back and starting it.

> With orca this is **`service.backup` / `service.restore`** — location-agnostic (docker / podman / lxc / vm), one command regardless of where nut runs. No per-service backup script.

## With orca

orca drives this plugin through the single generic `service.*` surface — no per-plugin tools:

```sh
orca service.deploy nut      # render + launch on any supported runtime
orca service.status nut      # health + rich diagnostics (typed payload)
orca service.backup nut      # location-agnostic backup (tar; PBS on Proxmox)
orca service.configure nut   # apply config via the upstream API
```

## Layout

- `src/` — the plugin (pure Rust): the `ServiceBackend` descriptor + `configure` / `status`.
- [CAPABILITIES.md](CAPABILITIES.md) — the service-backend contract checklist.
- `assets/` — plugin icon.
