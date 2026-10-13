# FileHunter

**Beta.** Every file on your disks, cloud storage and other computers, in one window.
Instant browsing and search, folder sizes, previews, copy anything anywhere.
Windows 10/11 and macOS. Runs entirely on your machine, no telemetry.

**[Download the latest release](https://github.com/lamver/filehunter/releases/latest)**

> The interface is in Russian for now. English is planned.

## Features

- **A catalog of all your disks.** Each disk is scanned once in the background at low
  priority, so it does not slow the system down. After that the catalog updates itself
  from system events: files created, changed or deleted, drives plugged in or removed.
- **Browse like in Explorer, only faster.** Every folder shows its full size and file
  count right away. Tabs, path and search history, image and text previews.
- **Search by name** in the open folder and all its subfolders. Filters: `ext:pdf`,
  `size:>1gb`, `type:dir`, `*` and `?` wildcards, `-word` to exclude.
- **S3 storage**: AWS, MinIO, Cloudflare R2 and other S3-compatible services. Only the
  object list goes into the catalog; file contents are not downloaded.
- **Your other computers.** Pair them with a one-time code; traffic is end-to-end
  encrypted. On the same network computers connect directly. Over the internet they
  also try to connect directly, and fall back to a relay if that fails. For each device
  you choose which disks and folders it can see, and whether it can only view or also
  make changes.
- **Tasks**: copy and move between disks, S3 and computers. Delete to the Recycle Bin
  or permanently. Download from a URL with resume (Ctrl+V with a link on the clipboard).
  A task log with pause, cancel and retry. System folders are protected.
- **Updates from inside the app**: «О программе» (About) → «Проверить обновления». The update signature is
  verified before it is installed.

## Not there yet

- Duplicate finder: the engine is done, the screen is being redesigned.
- Search by file contents and by meaning.
- Linux.

## Install

**Windows.** Download `FileHunter_x.y.z_x64-setup.exe`, or `*_portable.exe` if you want
to run it without installing. The app is not signed by Microsoft yet, so SmartScreen
will show a warning: click "More info" → "Run anyway". It needs WebView2, which comes
with Windows 11 and an updated Windows 10.

**macOS** (Intel and Apple Silicon, macOS 11+). Download `FileHunter_x.y.z_universal.dmg`
and drag the app into Applications. It is not signed by Apple yet. If macOS says the app
is damaged or comes from an unidentified developer, run this in Terminal:

```bash
xattr -dr com.apple.quarantine /Applications/FileHunter.app
```

## Privacy

- The catalog, history and settings are stored only on your computer.
- S3 keys are kept in the system password store: Windows Credential Manager or the
  macOS Keychain. They are never sent to your other computers.
- The app goes online only when you ask it to: opening S3, connecting to your computers,
  downloading a URL. Once a day it also checks GitHub for updates.
- Connections between computers over the internet use the public servers of the
  [iroh](https://iroh.computer) project. They see only device IDs and encrypted data.

## Building from source

You need [Rust](https://rustup.rs) (stable). On Windows you also need the Visual Studio
Build Tools.

```bash
cargo build --release -p filehunter
```

The app is built at `target/release/filehunter.exe` on Windows and `target/release/filehunter`
on macOS.

To try changes next to an installed copy, run `dev.bat` (Windows) or `./dev.sh` (macOS).
It builds the current code and starts it with its own data folder, tabs and network name,
so both can run at the same time. On first run it copies the installed copy's index, so
the disks are not scanned again. Installers are built by GitHub Actions (`.github/workflows/release.yml`) for each
`v*` tag.

Project layout:

```
crates/core   — core: index, scanning, search, duplicates, S3
crates/cli    — command-line tool for benchmarks and debugging
app/src-tauri — the app: window, file watching, tasks, peer connections, updates
app/ui        — the interface (plain HTML, CSS and JavaScript, no bundler)
```

## License

[FSL-1.1-MIT](LICENSE.md), Copyright 2026 Valerij Frolov.

You may read the code, build it for yourself, change it for your needs, and use it at
home and at work. One thing is not allowed: building a competing commercial product on
top of it. Two years after release, every version becomes plain MIT.

Third-party libraries and their licenses are listed in [NOTICE](NOTICE).
