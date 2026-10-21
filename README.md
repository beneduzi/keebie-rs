# keebie-rs

A small, reliable Linux daemon that turns a second keyboard (a numpad, a macro
pad, an old PS/2 board — anything that shows up as `/dev/input/event*`) into a
macro keyboard: every key press runs a shell command, switches layer, or fires a
script.

This is a Rust rewrite of [Keebie](https://github.com/daisyUniverse/Keebie) by
Elisha Shaddock, keeping its configuration format and ideas, and fixing the parts
that made it fragile in daily use.

```
numpad key  ──►  evdev (grabbed)  ──►  layer lookup  ──►  detached command
```

## Why a rewrite

The original Keebie is a great idea held together by a `for event in read_loop()`
loop. In practice it blocks, leaks grabs and dies on surprises. This version
keeps the same `config` + `layers/*.json` files and changes the engine:

| | Keebie (Python) | keebie-rs |
|---|---|---|
| running commands | `os.system()` **inside** the event loop | `spawn()` detached with `setsid`, never blocks |
| grab cleanup | none — a crash leaves the keyboard grabbed | `Drop` guard + `sigaction` handlers, always releases |
| device unplug | crash / dead daemon | hotplug: retries every second, re-grabs when it returns |
| config writes | rewrite in place | temp file + atomic `rename` |
| reload | restart the script | `SIGHUP` reloads `config` + active layer |
| key repeats | reprocessed | filtered (`value == 1` only) |
| supervision | manual / `~/.config/autostart` | systemd **user** unit with `Restart=always` |
| footprint | Python + evdev | single ~300 KB RSS static-ish binary |

## Install

```sh
git clone https://github.com/beneduzi/keebie-rs.git
cd keebie-rs
cargo build --release

# systemd user service
systemctl --user link "$PWD/keebie.service"
systemctl --user enable --now keebie.service
```

Logs:

```sh
journalctl --user -u keebie -f
```

## Configuration

`--config-dir` (default `~/Keebie`) must contain:

```
~/Keebie/
├── config              # line 1: device path, line 2: active layer file
├── layers/
│   └── default.json
└── scripts/            # optional, for script: and py: actions
```

`config`:

```
/dev/input/by-id/usb-GASIA_USB_Keyboard-event-kbd
default.json
```

`layers/default.json` maps evdev key names to actions:

```json
{
  "KEY_KP0": "alacritty",
  "KEY_KP1": "git status && git diff",
  "KEY_KP2": "script:deploy.sh staging",
  "KEY_KP3": "py:report.py",
  "KEY_ESC": "layer:default"
}
```

Action prefixes:

| value | behavior |
|---|---|
| `layer:<name>` | switch to `layers/<name>.json` (created if missing) and persist it in `config` |
| `script:<file> [args]` | `bash <config-dir>/scripts/<file> [args]` |
| `py:<file> [args]` | `python3 <config-dir>/scripts/<file> [args]` |
| anything else | `sh -c "<value>"` |

Commands are started detached (`setsid`, stdin `/dev/null`, stdout/stderr
inherited), so a long-running app never stalls the keypad.

Key repeat is ignored, and only key *presses* trigger actions — releases are
consumed by the grab.

## CLI

```
keebie [OPTIONS]

      --config-dir <DIR>    config directory [default: ~/Keebie]
      --device <DEV>        by-id name, absolute path, or "auto" (from config)
      --layer <FILE>        start on a specific layer
      --list-layers         list layers and exit (does not touch the device)
      --log-level <LEVEL>   trace|debug|info|warn|error [default: info]
```

## Requirements

- Linux with evdev (`/dev/input/event*`)
- Read access to the device (group `input`, or a udev rule)
- X11 for GUI commands fired from macros (`xdotool`, `wmctrl`, …)

## Credits

- [daisyUniverse/Keebie](https://github.com/daisyUniverse/Keebie) — the original
  Python tool, its config format and the layer/script/py action syntax. This
  project would not exist without it.
- `evdev` crate for the input layer.

## License

MIT
