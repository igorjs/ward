# Running wardd as a service

`wardd` serves the CLI over a Unix socket, so it has to be running before `ward` works. The unit files below start it at login and restart it if it exits. Both run as your user, never as root.

## macOS (Homebrew)

```sh
brew install igorjs/ward/ward
brew services start ward
```

Logs go to `$(brew --prefix)/var/log/ward.log`. Check with `ward health`.

## macOS (install.sh)

`packaging/launchd/dev.ward.wardd.plist` is a template. launchd does not expand `~`, so fill in the two placeholders:

```sh
mkdir -p ~/Library/Logs/ward ~/Library/LaunchAgents
sed -e "s|__WARDD_PATH__|$HOME/.ward/bin/wardd|" -e "s|__HOME__|$HOME|g" \
  packaging/launchd/dev.ward.wardd.plist > ~/Library/LaunchAgents/dev.ward.wardd.plist
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/dev.ward.wardd.plist
```

Logs go to `~/Library/Logs/ward/wardd.log`. Stop it with `launchctl bootout "gui/$(id -u)/dev.ward.wardd"`.

## Linux (systemd user unit)

You need read and write access to `/dev/kvm` (`sudo usermod -aG kvm $USER`, then log in again).

```sh
mkdir -p ~/.config/systemd/user
cp packaging/systemd/wardd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now wardd
```

The unit runs `~/.ward/bin/wardd`. If you installed it elsewhere, edit `ExecStart`. To keep it running after you log out, run `loginctl enable-linger $USER`. Logs: `journalctl --user -u wardd`.

## Releases and the Homebrew tap

The release workflow rewrites `packaging/homebrew/ward.rb` with the new version and checksums (`scripts/update-formula.sh`) and uploads it as the `homebrew-formula` artefact. If the `HOMEBREW_TAP_TOKEN` secret is set (a token with write access to `igorjs/homebrew-ward`), it also pushes the file to `Formula/ward.rb` in that repo. Without the secret the push is skipped.
