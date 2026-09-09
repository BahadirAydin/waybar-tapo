# waybar-tapo

A small Rust status and control helper for a TP-Link Tapo L900 light strip.
It prints one line of Waybar JSON and keeps credentials in the local Waybar
configuration rather than the repository.

## Build and install

```sh
cargo test --locked
cargo build --release --locked
install -Dm755 target/release/waybar-tapo ~/.local/bin/waybar-tapo
```

Create `~/.config/waybar/tapo.json` with mode 600:

```json
{"device_ip":"192.0.2.10","username":"you@example.com","password":"secret"}
```

An existing controller file can be imported without overwriting local settings:

```sh
waybar-tapo --import-config /path/to/config.json
```

Use `waybar-tapo --set-ip ADDRESS` after a DHCP address change.

## Commands

Running without arguments returns status. `color` switches between white and an
1800K-style warm orange; `warm` selects the warm color directly. The L900 only
accepts 2500–6500K in color-temperature mode, so 1800K is approximated in RGB
as HSV `(24, 100)`. `toggle` changes power, and
`brighter`/`dimmer` adjust brightness by five percentage points. Brightness is
kept between 1 and 100 percent. Requests are serialized so fast scrolling does
not queue changes.

The companion Waybar configuration and setup script live in the
[dotfiles repository](https://github.com/BahadirAydin/dotfiles) under
`.config/waybar`.
