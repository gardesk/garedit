# garedit

Native text editor for the gardesk suite.

## Prerequisites

`garedit` uses X11 + Cairo/Pango via `gartk`. A local build environment needs:

- `pkg-config`
- `glib-2.0`
- `gobject-2.0`
- `gio-2.0`
- `cairo`
- `pango`
- `pangocairo`
- `xcb` (for linking)

## Build

From this directory:

```bash
cargo build --release
```

## Run

- Foreground:

```bash
./target/release/garedit /etc/nixos/configuration.nix --line 12 --column 1
```

- Daemon + control:

```bash
./target/release/garedit --daemon
./target/release/gareditctl open /etc/nixos/configuration.nix
./target/release/gareditctl status
```

- Auto-start daemon from control tool:

```bash
./target/release/gareditctl --start-daemon open /etc/nixos/configuration.nix
```

## Session + Recovery

- Session snapshot: `~/.local/state/garedit/session.json`
- Autosave dir: `~/.local/state/garedit/autosave/`
- Palette commands:
  - `save session`
  - `restore session`
  - `clear session data`

## Release Checklist

Run:

```bash
scripts/release-smoke.sh
```

Then perform manual GUI checks:

1. Startup + file open/save.
2. Search prompt/edit flow (`Ctrl+F`, `F3`, `Shift+F3`).
3. IPC control (`open/show/hide/toggle/status/quit`).
4. Session restore with a dirty tab.
