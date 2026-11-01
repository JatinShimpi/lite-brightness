# Lite Brightness

**A tiny Twinkle Tray alternative for Windows 10 / 11.** One tray icon to control the brightness of your **laptop screen and external monitors**, plus support for a **keyboard volume knob / dial** (tested on the **Ant Esports MK800**) that changes brightness instead of volume.

Written in Rust with the raw Win32 API. No Electron, no runtime, no installer.

|             | Lite Brightness | Twinkle Tray (Electron) |
| ----------- | --------------- | ----------------------- |
| Exe size    | ~260 KB         | n/a                     |
| RAM in tray | ~14 MB          | ~300 MB (measured on my PC) |

## Features

- **One flyout for every display**: click the tray icon and get a slider per screen (laptop panel + external monitors), like Twinkle Tray.
- **Multi-monitor setups:** every connected monitor that supports DDC/CI gets its own slider and power button. The dial button cycles through each monitor, the laptop, all displays together, then volume. (Tested with a laptop plus one external monitor. Setups with two or more external monitors are untested, so please report any problems.)
- **External monitor brightness over DDC/CI**, no need to touch the buttons on the back of the monitor.
- **Laptop panel brightness over WMI.**
- **Turn any display off and on** from the flyout, including the laptop screen.
- **Keyboard dial / volume knob controls brightness.** Turn the knob to dim or brighten, press it to switch which display is controlled.
- **Dial button actions** you can pick from the tray menu: switch display, lock the PC, turn screens off, lock + screens off, or do nothing.
- **Dial target cycle:** monitor, laptop, all displays (kept in sync), then normal Windows volume.
- **Only reacts to your external keyboard.** The laptop's own volume and mute keys keep working as volume keys.
- **Small on-screen popup** that shows the level while you turn the dial.
- **Run at startup** toggle, single instance, high-DPI aware.
- Every feature is a toggle in the tray right-click menu. Settings are a plain text file.

## Works with

- Windows 10 and Windows 11
- Laptop panels that support WMI brightness (most do)
- External monitors with DDC/CI enabled (tested on a **Lenovo L24-4C**)
- Keyboard volume knobs that send the standard media keys (tested on the **Ant Esports MK800**). Other keyboards with a knob that sends the standard volume keys should work after you set their VID (see below), but I haven't tested them.

## Monitor compatibility

Lite Brightness talks to monitors in two ways only:

- **DDC/CI** for external monitors. This runs over the video cable, so HDMI, DisplayPort, DVI, VGA and USB-C/Thunderbolt display connections all work, as long as the monitor has DDC/CI turned on in its menu.
- **WMI** for the built-in laptop panel.

These do **not** work:

- Monitors with no DDC/CI support, or with it turned off.
- Monitors that control brightness over USB instead of the video cable (Apple Studio Display, LG UltraFine, some Eizo models).
- Some docks, KVM switches, cheap adapters and DisplayLink adapters that block DDC/CI.
- Laptop panels that don't expose WMI brightness.

If a monitor shows "no brightness control", try a direct cable and turn DDC/CI on in its menu. I've only tested a Lenovo L24-4C and one laptop panel, so reports from other hardware are welcome.

## Install

1. Download `lite-brightness.exe` from the [Releases](../../releases) page, or [build it yourself](#build).
2. Put it anywhere and run it. A sun icon appears in the tray.
3. Right-click the icon and tick **Run at startup** if you want it to start with Windows.

## Use

- **Left-click** the tray icon: brightness sliders and power buttons for each display. The mouse wheel over a slider also works.
- **Right-click** the tray icon: settings menu (dial on/off, dial button action, target display, turn a display off, run at startup, refresh displays, exit).
- **Dial:** turn to change brightness in steps of 5. Press to run the chosen button action (by default it cycles the target display).

## Using a different keyboard

The app only reacts to the keyboard whose device name contains the text in `device=`. The default is `VID_0E6A` (the MK800).

To use another keyboard:

1. Find its vendor ID: Device Manager, open the keyboard, Details tab, Hardware Ids. You'll see something like `VID_046D`.
2. Edit `%APPDATA%\lite-brightness\settings.txt` and set `device=VID_046D`.
3. Restart Lite Brightness.

If you want the dial to react to every keyboard, untick **Only the external keyboard's dial** in the tray menu.

## Settings file

`%APPDATA%\lite-brightness\settings.txt`

```
dial=1
button=0
ext_only=1
device=VID_0E6A
```

- `dial`: 1 = the dial controls brightness
- `button`: 0 switch display, 1 lock PC, 2 screens off, 3 lock + screens off, 4 nothing
- `ext_only`: 1 = only the external keyboard's dial
- `device`: which keyboard counts as the external one

## Build

You need Rust (stable) and the MSVC toolchain.

```
git clone https://github.com/JatinShimpi/lite-brightness
cd lite-brightness
cargo build --release
```

The exe is at `target/release/lite-brightness.exe`.

## How it works

- **External monitors:** `GetPhysicalMonitorsFromHMONITOR` plus `SetMonitorBrightness` (DDC/CI). Power uses VCP code `0xD6`.
- **Laptop panel:** WMI `WmiMonitorBrightnessMethods.WmiSetBrightness` in `root\wmi`. Turning the laptop screen off and on uses `DisplaySwitch.exe /external` and `/extend`.
- **Dial:** a low-level keyboard hook (`WH_KEYBOARD_LL`) swallows `VK_VOLUME_UP`, `VK_VOLUME_DOWN` and `VK_VOLUME_MUTE`. A Raw Input listener tells it which physical keyboard sent the key.
- Slow hardware calls run on a worker thread so typing never stalls.

## Troubleshooting

- **A monitor has no slider or says "no brightness control":** turn on DDC/CI in the monitor's own on-screen menu, then right-click the tray icon and choose Refresh displays.
- **Icon missing after logging in:** Lite Brightness retries until the taskbar is ready. If it still doesn't show, check `%TEMP%\lb_crash.txt`.
- **Laptop brightness doesn't change:** check `%TEMP%\lb_wmi.txt`.
- **Dial does nothing:** check that `device=` matches your keyboard, or untick the external-keyboard-only option.

## Contributing

Issues and pull requests are welcome. The whole app is one file, `src/main.rs`. Ideas: dimming below the hardware minimum, per-display presets, hotkeys, an installer.

## License

MIT, see [LICENSE](LICENSE).

---

*Keywords: low ram twinkletray alternative, low RAM Twinkle Tray alternative, lightweight twinkletray alternative, twinkletray alternative, Twinkle Tray alternative, twinkle tray lightweight, twinkletray replacement, lightweight brightness control Windows, external monitor brightness, DDC/CI, laptop brightness slider, multi monitor brightness, tray brightness app, volume knob brightness, keyboard dial brightness, volume dial brightness, Ant Esports MK800, MK800 volume knob, Windows 11 brightness, Windows 10 brightness, Lenovo monitor brightness, low RAM, portable, Rust, Win32, WMI, no Electron.*
