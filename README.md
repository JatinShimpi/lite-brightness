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
- **Rebindable keys, like a game's controls screen.** Every action (brightness up, brightness down, switch display, lock PC, screens off, lock + screens off) has two key slots. Click a slot and press any key or combo (Ctrl / Alt / Shift / Win + key, F13–F24, media keys or the dial itself). If a key is already used, it moves to the new action.
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
- **Right-click** the tray icon: settings menu (dial on/off, key bindings, target display, turn a display off, run at startup, refresh displays, exit).
- **Dial:** turn to change brightness in steps of 5. Press to switch the target display (default keys; change them in **Key bindings...**).
- **Key bindings window:** click a box, press a key. Esc cancels, Delete or Backspace clears, right-click clears. **Reset to defaults** puts the dial back on brightness and switch display.
- **Clash check:** before a key is saved, Lite Brightness checks whether another app or Windows has already registered it as a global shortcut, and also checks a list of common shortcuts (Ctrl+C, Alt+Tab, Win+D, ...) and plain typing keys. If there's a clash it tells you why, and you press the same key again to use it anyway. Shortcuts that live only inside one app (for example a game's own keys) can't be detected.

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
ext_only=1
device=VID_0E6A
bind_up=175+0,0+0
bind_down=174+0,0+0
bind_cycle=173+0,0+0
bind_lock=0+0,0+0
bind_off=0+0,0+0
bind_lockoff=0+0,0+0
```

- `dial`: 1 = the brightness up / down keys are active
- `ext_only`: 1 = media keys (the dial) only count from the external keyboard. Normal keyboard shortcuts work from any keyboard.
- `device`: which keyboard counts as the external one
- `bind_*`: two key slots per action, each `virtual-key-code+modifiers` (modifiers: 1 Alt, 2 Ctrl, 4 Shift, 8 Win, added together; `0+0` = empty). Easier to set from the Key bindings window.

Old settings files with `button=` are converted automatically the first time.

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
- **Dial and key bindings:** a low-level keyboard hook (`WH_KEYBOARD_LL`) swallows any bound key (by default `VK_VOLUME_UP`, `VK_VOLUME_DOWN` and `VK_VOLUME_MUTE`) and runs its action. A Raw Input listener tells it which physical keyboard sent a media key.
- Slow hardware calls run on a worker thread so typing never stalls.

## Troubleshooting

- **A monitor has no slider or says "no brightness control":** turn on DDC/CI in the monitor's own on-screen menu, then right-click the tray icon and choose Refresh displays.
- **Icon missing after logging in:** Lite Brightness retries until the taskbar is ready. If it still doesn't show, check `%TEMP%\lb_crash.txt`.
- **Laptop brightness doesn't change:** check `%TEMP%\lb_wmi.txt`.
- **Dial does nothing:** check that `device=` matches your keyboard, or untick the external-keyboard-only option.

## Contributing

Issues and pull requests are welcome. The whole app is one file, `src/main.rs`. Ideas: dimming below the hardware minimum, per-display presets, an installer.

## License

MIT, see [LICENSE](LICENSE).

---

*Keywords: low ram twinkletray alternative, low RAM Twinkle Tray alternative, lightweight twinkletray alternative, twinkletray alternative, Twinkle Tray alternative, twinkle tray lightweight, twinkletray replacement, lightweight brightness control Windows, external monitor brightness, DDC/CI, laptop brightness slider, multi monitor brightness, tray brightness app, volume knob brightness, keyboard dial brightness, volume dial brightness, Ant Esports MK800, MK800 volume knob, Windows 11 brightness, Windows 10 brightness, Lenovo monitor brightness, low RAM, portable, Rust, Win32, WMI, no Electron.*

*More keywords: remap volume knob, remap keyboard knob Windows, keyboard knob brightness Windows 11, volume knob to brightness, change volume knob function, reprogram volume dial, volume wheel brightness, media keys brightness, custom key bindings brightness, brightness hotkey Windows, brightness keyboard shortcut Windows 10, brightness shortcut external monitor, hotkey external monitor brightness, keyboard shortcut monitor brightness, global hotkey brightness, rebind keyboard dial, knob lock PC shortcut, turn off monitor hotkey, turn off screen shortcut Windows, lock PC with keyboard knob, F13 F24 brightness, Ctrl Alt brightness shortcut, Twinkle Tray hotkeys alternative, Twinkle Tray volume knob, Twinkle Tray lightweight alternative, Monitorian alternative, ClickMonitorDDC alternative, Dimmer alternative, f.lux alternative brightness, PangoBright alternative, brightness slider tray Windows, control second monitor brightness, adjust external monitor brightness from PC, change monitor brightness without buttons, DDC/CI brightness tool, DDC CI Windows app, monitor brightness app small, lightweight brightness app, brightness app low memory, tiny brightness utility Windows, open source brightness control Windows, free brightness control Windows, laptop and monitor brightness together, sync brightness all monitors, dual monitor brightness, Ant Esports MK800 knob software, MK800 software, mechanical keyboard knob software, keyboard rotary encoder brightness, rotary knob brightness Windows.*
