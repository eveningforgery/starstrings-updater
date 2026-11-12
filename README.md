# StarStrings Updater

[![CI](https://github.com/eveningforgery/starstrings-updater/actions/workflows/ci.yml/badge.svg)](https://github.com/eveningforgery/starstrings-updater/actions/workflows/ci.yml)

Keeps [MrKraken's StarStrings](https://github.com/MrKraken/StarStrings) up to date in your Star Citizen folder, by itself, without anything running in the background.

StarStrings has to be re-installed after every Star Citizen patch or the game shows missing text. This does that for you: whenever a new StarStrings release is published, it lands in your `LIVE` folder within the hour.

- **Invisible.** No window, no tray icon, no notifications.
- **Nothing stays running.** Windows starts it for a moment when you sign in and once an hour; it checks, and exits.
- **Small.** One 430 KB program, no dependencies, no administrator rights.

For Windows 10 and 11. It has been tested on Windows 11; if something misbehaves on Windows 10, please [open an issue](https://github.com/eveningforgery/starstrings-updater/issues/new/choose).

## Install

1. Download `starstrings-updater.exe` from the [latest release](https://github.com/eveningforgery/starstrings-updater/releases/latest).
2. Double-click it and confirm.

That's it. It finds your Star Citizen folder, installs the current StarStrings into `LIVE`, and keeps it current from then on. You can delete the downloaded file afterwards.

Windows may show "Windows protected your PC" the first time, because the program is new and not code-signed. Choose **More info**, then **Run anyway**.

Each release lists the SHA-256 checksum of the program. To check your download, run `Get-FileHash .\starstrings-updater.exe` in PowerShell and compare the result.

If the game isn't found automatically you are asked to pick your `StarCitizen` folder (the one that contains `LIVE`).

## Remove

**Windows Settings > Apps > Installed apps > StarStrings Updater > Uninstall.**

You are asked whether to keep the StarStrings files in the game (they just stop being updated) or to undo them as well. Undoing removes what the updater installed; if it had replaced a `global.ini` you already had, that file is put back instead.

Two things can remain afterwards:

- A copy of any file of yours that the updater replaced and did not put back. It is never deleted, and the uninstaller tells you where it is.
- One small file in your temp folder. A program cannot delete itself while it runs, so the uninstaller moves itself there; Windows' disk clean-up removes it.

## How it works

There is no service and no background process. The installer registers one task with **Windows Task Scheduler**, which is already running on every PC. The task starts the updater:

- one minute after you sign in, and
- once an hour (a check missed while the PC was off or asleep is made up when it's back).

Each check is a single conditional web request to the StarStrings release on GitHub: "has this file changed since the copy I have?". Almost always the answer is no, and the updater exits. Measured on a normal PC, such a check takes about 0.2 seconds, uses no measurable CPU time, and writes nothing to disk.

When the answer is yes, it downloads the release (about 2.5 MB), verifies it, and swaps the new `global.ini` in atomically.

GitHub cannot notify a home PC when a release happens, so something on the PC has to ask. Asking through Task Scheduler means that "something" exists for a fraction of a second per hour instead of sitting in memory all day.

## What it changes on your PC

Everything it touches, in full:

| Where | What |
| --- | --- |
| `<StarCitizen>\LIVE\Data\Localization\english\global.ini` | Replaced with the one from the latest StarStrings release. |
| `<StarCitizen>\LIVE\USER.cfg` | The line `g_language = english` is added if it is missing. Nothing else in the file is touched; the file is created if you don't have one. |
| `%LOCALAPPDATA%\StarStringsUpdater\` | The program, a small state file, a log of changes, and a one-time copy of any `global.ini` that was there before it first replaced one. If that copy cannot be made, your file is not replaced. |
| Task Scheduler | One task, named "StarStrings Updater" followed by your account's ID, running as you with normal rights. |
| Installed apps | One entry, so you can uninstall it like anything else. |

It never asks for administrator rights: the RSI Launcher already makes the game folder writable for your account.

If you had another language set in `USER.cfg`, the installer switches it to `english` (StarStrings is an English pack and does nothing otherwise) and remembers the old value so the uninstaller can restore it.

## Safety

The updater downloads a file from the internet and writes into your game folder unattended, so it is deliberately strict about it:

- It only ever writes `.ini` files under `Data\Localization\`. Anything else in the downloaded archive is ignored, and paths that try to leave that folder are rejected. `USER.cfg` is never taken from the archive.
- HTTPS only, using Windows' own networking and certificate checks. It refuses to be redirected to an unencrypted address.
- Every extracted file must match the size and checksum recorded in the archive, and nothing is written unless the whole archive checks out. A broken or half-finished download can't replace a good file.
- A release may hold at most 32 files and 128 MB, and a download gets two minutes. A bad release can waste neither your disk nor your time.
- It loads Windows' own libraries from the Windows system folder only, so a file planted next to the program in your Downloads folder cannot ride along.
- No telemetry. The only thing it ever contacts is the StarStrings release download on GitHub, and it sends nothing about you or your PC.
- It does not update itself. The program you installed is the program that runs.

The whole thing is roughly 3,800 lines of Rust, with as many lines of tests, and two small dependencies besides Microsoft's Windows bindings.

## Good to know

- **PTU.** Off by default (MrKraken advises against custom strings on PTU, because new builds add text faster than packs can follow). To include it: `starstrings-updater install --ptu`. To stop again: `starstrings-updater install --live-only`.
- **Game running.** If the game has the file open when an update arrives, the update is simply tried again at the next check. Updates take effect the next time you start the game.
- **Edited files.** A newer StarStrings release replaces `global.ini`, so manual edits to it don't survive an update. If you maintain your own merged pack, this tool isn't for you.
- **A deleted `global.ini`** (for example after a game repair) is put back at the next check.
- **Offline.** Nothing happens, and nothing is logged; it tries again next hour.
- **"Cannot be installed from here."** Start the program from File Explorer, not from inside another app. Some apps (packaged apps, like those from the Microsoft Store) keep whatever they start in a private corner of your user folder, where Windows could not find the updater again; the installer notices and stops rather than leave you with an installation that does nothing.
- **Is it working?** Double-click the program again: it brings everything up to date and shows what is installed. The log of changes is at `%LOCALAPPDATA%\StarStringsUpdater\updater.log`.

## Problems?

- **The updater misbehaves:** [open an issue](https://github.com/eveningforgery/starstrings-updater/issues/new/choose) and include what the window says when you double-click the program (Ctrl+C copies its text).
- **Wrong or missing text in the game:** that is part of StarStrings itself; please report it to the [StarStrings project](https://github.com/MrKraken/StarStrings/issues).
- **Something that could put other players at risk:** please report it privately, as described in [SECURITY.md](SECURITY.md).

## Command line

```
starstrings-updater                  Install (or, if installed, update and show the status)
starstrings-updater install [--path <folder>] [--ptu | --live-only] [--quiet]
starstrings-updater uninstall [--remove-strings] [--quiet]
starstrings-updater run              Check now (this is what the scheduled task runs)
starstrings-updater status           Show what is installed
```

`--quiet` suppresses all dialogs, for scripted installs. Exit codes: `0` success, `1` failure, `2` wrong usage, `3` cancelled, `70` internal error.

The installed copy is not on your `PATH`. To run it from PowerShell, for example to see the status:

```
& "$env:LOCALAPPDATA\StarStringsUpdater\starstrings-updater.exe" status | Out-Host
```

## Building and testing

You need [Rust](https://rustup.rs) and the Visual Studio C++ build tools.

```
cargo build --release
cargo test
cargo test -- --include-ignored
```

`cargo test` runs the unit tests and an end-to-end suite that drives the real executable against a local stand-in for GitHub and sandboxed game folders. It cannot find or touch a real installation.

`--include-ignored` adds the system tests. Those register a real scheduled task and "Installed apps" entry under a development name, check them, and remove them again; one of them has Task Scheduler run a sandboxed copy against the real StarStrings release.

Pushing a tag such as `v0.2.0` builds the program on GitHub and prepares a draft release with the program, its checksum and the licence notices; nothing is published until someone presses "Publish release".

## Credits and disclaimer

StarStrings is made by [MrKraken](https://github.com/MrKraken/StarStrings). This project only automates the installation steps from its README and does not include or redistribute the pack; please read that README for what the pack changes and for Cloud Imperium's position on community localization files.

This is an unofficial fan project. It is not affiliated with MrKraken or with the Cloud Imperium group of companies. Star Citizen is a trademark of Cloud Imperium Rights LLC.

Released under the [MIT License](LICENSE). The program is built with a few open-source libraries; their licences are in `THIRD-PARTY-NOTICES.txt`, attached to every release.
