# MouseBattery

A tiny Windows tray app that shows the battery level of a Logitech wireless mouse. No G HUB needed.

- Battery level in the tray, as a number or a battery symbol
- Hover for the mouse's name, charge and state
- Right-click to refresh, switch the icon style, start with Windows, or exit

It works with Logitech mice that report their battery over HID++ 2.0. It has only been tested
with a PRO X Wireless, so other models may not work.

## Building

Needs [Rust](https://rustup.rs) on Windows.

```
cargo build --release      # target/release/MouseBattery.exe
```

## Credits and license

The HID++ protocol logic is ported from
[LGSTrayBattery](https://github.com/andyvorld/LGSTrayBattery) by andyvorld (GPL-3.0),
rewritten in Rust and modified in November 2026.

MouseBattery is licensed under [GPL-3.0-only](LICENSE) and comes with no warranty.
It is not affiliated with Logitech.
