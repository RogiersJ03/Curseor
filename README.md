# Curseor

Curseor is a Windows cursor program. Hiding the pointer is the only feature for now; other cursor features may be added later. It requires Windows 11.

## Build

```
cargo build --release
```

Start `target\release\curseor.exe`. There is no installer.

## Settings

The default settings file is `%APPDATA%\Curseor\config.toml`. If `%USERPROFILE%\.config\Curseor\config.toml` already exists, Curseor uses that file instead, and tray changes write only that active file. Edits apply while Curseor is running. `show_tray` is not in the tray menu; set `show_tray = true` in the active settings file to bring the icon back. The log is `%APPDATA%\Curseor\curseor.log`. Remove the `Curseor` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` before deleting the program, or turn off Start with Windows first.

## Tray

The tray menu can enable Curseor, pause it, hide while typing, hide when idle, choose how long to wait, start with Windows, open the settings file, or quit. Pause lasts only until quit or sign-out and is not written to the settings file. Enabled is the switch that is saved. Windows may keep the icon in the tray overflow; Win+B focuses the notification area when the pointer is hidden.

## Limits

Programs that draw their own cursors, including many games and design tools, can still show a pointer. Games that recenter the pointer will not stay hidden. If Curseor is killed while the pointer is hidden, the pointer stays blank until the next start. If hiding fails partway, Curseor restores the pointer immediately, stays shown, and waits about 2 seconds before trying again instead of retrying on every repeated key.
