# chatterux

Linux daemon that suppresses chatter (unwanted double key presses) on worn mechanical
keyboards. It follows the same principle as
[KeyboardChatterBlocker](https://github.com/FreneticLLC/KeyboardChatterBlocker):
a key press is ignored if it occurs less than N milliseconds after the previous release
of the same key. The repeats and the release of that press are ignored as well.

## How it works

For each selected keyboard, chatterux:

1. creates a virtual `uinput` keyboard (named `chatterux: <original name>`) that mirrors
   the capabilities of the physical keyboard;
2. takes exclusive control of the physical keyboard (`EVIOCGRAB`), after waiting for all
   its keys to be released;
3. forwards every event to the virtual keyboard, except detected chatter.

Since filtering happens at the evdev level, it works with X11, Wayland and the console.
Hot-plugged keyboards are detected automatically.

## Building

```sh
cargo build --release
```

## Trying it out

```sh
sudo ./target/release/chatterux --list-devices   # detected devices (* = filtered)
sudo ./target/release/chatterux --dry-run        # logs chatter without blocking anything
sudo ./target/release/chatterux -c dist/config.toml
```

`--dry-run` helps choose a threshold: type normally and watch which presses would be
blocked, and with what delay.

## Installing as a service

```sh
sudo install -Dm755 target/release/chatterux /usr/local/bin/chatterux
sudo install -Dm644 dist/config.toml /etc/chatterux/config.toml
sudo install -Dm644 dist/chatterux.service /etc/systemd/system/chatterux.service
sudo systemctl daemon-reload
sudo systemctl enable --now chatterux
journalctl -u chatterux -f
```

After editing `/etc/chatterux/config.toml`: `sudo systemctl reload chatterux`
(`SIGHUP` signal). Thresholds apply immediately. A device that is already grabbed but no
longer selected stays filtered until the service is restarted.

## Configuration

See [`dist/config.toml`](dist/config.toml), the commented example:

| Key                 | Purpose                                                           |
|---------------------|-------------------------------------------------------------------|
| `threshold_ms`      | global threshold in ms (default 50, 0 = disabled)                 |
| `[keys]`            | per-key thresholds: `KEY_SPACE = 100`, `e = 0`, `BTN_LEFT = 30`…  |
| `devices.include`   | names (substring) or `/dev/input/by-id/…` paths; empty = all keyboards |
| `devices.exclude`   | devices to ignore, takes precedence over `include`                |
| `devices.mice`      | also filter mouse buttons                                         |

The log level is set with `RUST_LOG` (`debug` also shows dropped releases).

## Troubleshooting

If a key gets stuck or the keyboard stops responding, stop the service
(`sudo systemctl stop chatterux`): the kernel releases the grab immediately and the
physical keyboard works directly again.
