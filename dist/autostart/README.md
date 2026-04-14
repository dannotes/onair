# Auto-start onair on system boot

Per-OS templates for running onair as a background service that starts on login.

| File | OS | Mechanism |
|---|---|---|
| `macos/com.dannotes.onair.plist` | macOS | launchd LaunchAgent (per-user) |
| `linux/onair.service` | Linux | systemd user unit |
| `windows/install-startup.ps1` | Windows | Shortcut in Startup folder (no admin needed) |
| `windows/uninstall-startup.ps1` | Windows | Removes the shortcut |

---

## macOS

```bash
# 1. Make sure onair is installed somewhere — Homebrew is easiest.
brew install dannotes/onair/onair

# 2. Verify the binary path. Most people on Apple Silicon get /opt/homebrew/bin/onair.
which onair

# 3. Copy the launchd plist into your user LaunchAgents folder.
cp dist/autostart/macos/com.dannotes.onair.plist ~/Library/LaunchAgents/

# 4. If `which onair` showed a different path than /opt/homebrew/bin/onair,
#    open the plist and update the ProgramArguments string.

# 5. Load it.
launchctl load ~/Library/LaunchAgents/com.dannotes.onair.plist

# Logs go to /tmp/onair.log
tail -f /tmp/onair.log
```

To remove:
```bash
launchctl unload ~/Library/LaunchAgents/com.dannotes.onair.plist
rm           ~/Library/LaunchAgents/com.dannotes.onair.plist
```

---

## Linux (systemd)

```bash
# 1. Install onair (your distro's package manager, or build from source).
which onair

# 2. Copy the unit into your user systemd folder.
mkdir -p ~/.config/systemd/user
cp dist/autostart/linux/onair.service ~/.config/systemd/user/

# 3. If `which onair` returned something other than /usr/local/bin/onair,
#    edit ExecStart in the unit to match.

# 4. Enable + start.
systemctl --user daemon-reload
systemctl --user enable --now onair.service

# Watch logs.
journalctl --user -u onair -f
```

If you want onair to keep running even after you log out (rare for a personal indicator):
```bash
sudo loginctl enable-linger $USER
```

To remove:
```bash
systemctl --user disable --now onair.service
rm ~/.config/systemd/user/onair.service
```

---

## Windows

```powershell
# 1. Install onair (Scoop is easiest).
scoop bucket add onair https://github.com/dannotes/scoop-onair
scoop install onair

# 2. Run the install script (no admin needed).
.\dist\autostart\windows\install-startup.ps1
```

This creates a shortcut at `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\onair.lnk` that runs onair minimized on every login.

To remove:
```powershell
.\dist\autostart\windows\uninstall-startup.ps1
```

---

## Verify it's running

After installing, restart (or `launchctl kickstart` / `systemctl restart` / kill+relaunch) and open:

```
http://localhost:9876
```

You should see the dashboard. The bulb info bar should show your bulb connected. If not, check the logs.
