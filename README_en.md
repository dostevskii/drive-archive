# drive-archive

[한국어](README.md) · **English**

> Index the files scattered across your external drives, and find which drive holds what — **without plugging any of them in**

When project files live across several external drives, "which drive was that file on?" becomes a real problem. Short of plugging each one in and looking, there is no way to know.

drive-archive records the file and folder listing of an external drive **automatically, every time it is connected**. From then on the drive can sit in a desk drawer — a search on your computer tells you which drive it is on.

```
$ drive-archive search "2024 branding"

  [PROJECT-A]  work/2024/branding_renewal/          (folder, 2024-11-03)
  [PROJECT-A]  work/2024/branding_renewal/final.psd  (1.2 GB, 2024-11-03)
  [BACKUP-02]  archive/2024branding_backup.zip       (3.4 GB, 2024-12-20)

  → Connect the PROJECT-A drive.
```

---

## Browse it in your browser

Run `drive-archive serve` and the index opens in a browser. No Claude in the loop, and no drive needs to be connected.

![Main screen](docs/screen-index.png)

How many folders each drive holds, and whether it is plugged in right now, is visible at a glance. Drives that are not connected are dimmed.

> The drive names and files in these screenshots are sample data.

| Search | Browsing folders |
|---|---|
| ![Search results](docs/screen-search.png) | ![Folder browsing](docs/screen-browse.png) |

The search box takes any text, including CJK input. Pick a drive, press `Enter`, and you walk into it folder by folder; the `..` row at the top takes you back. **Which drive an item is on, and where**, stays on the bottom line at all times.

Move around with the mouse, the arrow keys, or VIM keys (`hjkl`). `ESC` enters NORMAL mode, and `i` or `/` returns you to the search box.

The server binds to `127.0.0.1` only. The index holds your full file names and paths, so there is no reason to expose it to other machines on the network.

---

## What it does

**It uses no resources when idle.**
This is not a background program. Windows Task Scheduler starts it only when it detects an "external drive connected" event; it runs briefly and exits completely. With no resident process, idle memory and CPU usage are zero.

**Changes are picked up automatically.**
Reconnect a drive and it diffs against the previous index — new, modified, and deleted files are all reconciled.

**It plugs into Claude.**
An MCP (Model Context Protocol) server is built in, so you can just ask in Claude Desktop or Claude Code.

> "Where are last year's branding project files on my external drives?"
> → Claude searches the index and answers: "They are on the PROJECT-A drive."

**It also works in a browser.**
One `serve` command opens a local web page. Search and folder browsing work entirely from the keyboard, and the page and font are embedded in the executable, so there is nothing else to install.

**File contents are never stored.**
Only names, paths, sizes, and modification dates are recorded. The database stays small, and none of your file contents are copied anywhere.

---

## Requirements

| Item | Requirement |
|---|---|
| OS | **Windows 11 only** |
| File system | No restriction (see below) |
| Connection | USB |

This program was built on Windows 11 and runs only there. Windows 10 and earlier, macOS, and Linux are not supported.

### File systems

The file system does not matter. **If the drive gets a letter in Windows Explorer, it gets indexed.**

Formats Windows reads out of the box:

| Format | Supported natively |
|---|---|
| NTFS | Yes |
| exFAT | Yes |
| FAT32 / FAT | Yes |
| ReFS | Yes |
| UDF | Yes |
| **HFS+ / APFS** (Mac formats) | **Needs a third-party driver** |
| **ext4 / Btrfs** (Linux formats) | **Needs a third-party driver** |

A drive formatted on macOS or Linux will not mount on a stock Windows install at all — it never even gets a drive letter. Install a driver such as Paragon HFS+ for Windows so Explorer can open it, and drive-archive will index it **with no code changes.** The format name is recorded exactly as that driver reports it.

The format of each drive is stored in the index and shown by `drive-archive drives`.

> If the drive has also been used on a Mac, macOS system folders like `.Spotlight-V100` and `.Trashes` come along with it. Those are skipped automatically.

---

## Install

### 1. Download

Grab `drive-archive.exe` from the [Releases page](https://github.com/dostevskii/drive-archive/releases) and put it wherever you like
(e.g. `C:\Tools\drive-archive\drive-archive.exe`).

### 2. Run the install command

Open PowerShell in that folder:

```powershell
.\drive-archive.exe install
```

That single line does all of this:

- Registers a Task Scheduler job so drives are indexed on connection
- Registers the MCP server with Claude Desktop and Claude Code

Registering the scheduled task needs administrator rights, so a UAC prompt appears. Click Yes.

Claude's config files are edited in place: existing entries are preserved and only a `drive-archive` entry is added. Quit Claude completely and reopen it for the change to take effect.

### 3. Connect a drive

Plug in an external drive and indexing starts on its own. Check progress with:

```powershell
.\drive-archive.exe drives
```

> Indexing speed depends on the number of files. A 1.8 TB NTFS drive with 22,661 entries took about 5 seconds; a 3.6 TB exFAT drive with 171,139 entries took about 10 minutes. It runs at low priority so it stays out of your way, and every run after the first only reconciles what changed — much faster.

### Uninstall

```powershell
.\drive-archive.exe uninstall
```

---

## Usage

### Search

```powershell
drive-archive search <keyword>
```

| Option | Description |
|---|---|
| `--drive <label>` | Search within one drive only |
| `--dirs-only` | Match folders only (useful for whole projects) |
| `--limit <n>` | Cap the number of results (default 50) |
| `--json` | JSON output (for scripts and MCP) |

```powershell
# Only folders whose name contains "portfolio"
drive-archive search portfolio --dirs-only

# Only inside the BACKUP-02 drive
drive-archive search .psd --drive BACKUP-02
```

### Drive list

```powershell
drive-archive drives
```

Shows every registered drive's label, file system, capacity, last connection time, and **whether it is connected right now**.

```
PROJECT-A  [connected (L:)]
  22661 entries · NTFS · 302.3 GB free of 1.8 TB
  last seen 2026-08-26 13:17:44 · last scan 2026-08-26 13:17:44
  volume serial 90FA8BC5

BACKUP-02  [not connected]
  171139 entries · exFAT · 577.6 GB free of 3.6 TB
  last seen 2026-08-26 13:06:49 · last scan 2026-08-26 13:17:13
  volume serial 5C31A9F0
```

### Other commands

| Command | Description |
|---|---|
| `drive-archive sync` | Reconcile every connected drive (called automatically by the scheduler) |
| `drive-archive scan [letter]` | Force a full rescan of one drive |
| `drive-archive status` | Index statistics (drives, entries, DB size) |
| `drive-archive forget <label>` | Drop a drive you no longer use from the index |
| `drive-archive mcp` | Run the MCP server (Claude calls this itself) |
| `drive-archive serve` | Open the browser view of the index (`--port`, `--no-open`) |
| `drive-archive setup-task` / `remove-task` | Register/unregister just the scheduled task |

### Tools Claude sees

Once connected over MCP, Claude gets these tools:

| Tool | What it does |
|---|---|
| `search_files` | Search by name and return which drive holds each hit |
| `list_drives` | Drive list with current connection status |
| `get_status` | Index statistics |

---

## How it works

```
External drive connected
      │
      ▼
Windows logs a volume mount event
      │
      ▼
Task Scheduler catches the event ──▶ runs drive-archive.exe sync
                                        │
                                        ▼
                              Picks out USB-attached volumes
                          (drives identified by volume serial)
                                        │
                                        ▼
                        Walks all files and folders in parallel
                                        │
                                        ▼
                              Diffs against the stored index
                              Applies new / changed / deleted
                                        │
                                        ▼
                                   Process exits
                              (back to zero resource use)
```

**Disconnecting** a drive does nothing at all. The index is already saved, and `drives` checks live connection status at the moment you ask.

### Guards against wiping the index by accident

If a drive is pulled mid-scan, applying the half-read result would mark every remaining file as deleted and destroy a perfectly good index. Three checks prevent that:

1. If the drive root cannot be read at all, the scan fails instead of returning nothing
2. After the scan, the volume serial is re-checked to confirm it is still the same drive
3. If some entries could not be read **and** the count dropped below half of the previous scan, the result is not applied

If any of the three trips, the index is left exactly as it was and the reason is printed to the screen and to `sync.log`. Reconnect the drive and run `drive-archive scan <letter>`.

If you genuinely did delete a lot of files, there are no read errors, so the change is applied as-is.

### Can I move files while a scan is running?

Yes. A scan never opens files — it only reads names, sizes, and dates — so it does not block copying, moving, or cutting. Files moved after the scan has passed them are reconciled on the next run.

### How drives are identified

Drive letters (`E:`, `F:`, …) shift with connection order, so they are useless as identifiers. drive-archive identifies a drive by its **volume serial number** and shows the **volume label** as the human-readable name.

> If you set each drive's volume label to match the physical label stuck on its case, the name in your search results is the name you look for in the drawer.

### Where things are stored

| What | Path |
|---|---|
| Index DB | `%LOCALAPPDATA%\drive-archive\index.db` |
| Log | `%LOCALAPPDATA%\drive-archive\sync.log` |

---

## Building from source

```powershell
git clone https://github.com/dostevskii/drive-archive.git
cd drive-archive
cargo build --release
# output: target\release\drive-archive.exe
```

Requires Rust 1.85 or newer and the MSVC build tools.

---

## Project status

What works today, what is still unverified, the per-version changelog, and the reasoning behind design decisions all live in [STATUS.md](STATUS.md) (written in Korean).

---

## License

This program is distributed under the [GNU General Public License v3.0](LICENSE).

You may use, modify, and redistribute it freely, but if you distribute a modified version you must release its source under the same GPL 3.0 license.

The monospace font used by the browser view, [FiraD2](https://github.com/partrita/FiraD2), is distributed under the SIL Open Font License 1.1 and is separate from this program's license. The full text is in [assets/FiraD2-LICENSE.txt](assets/FiraD2-LICENSE.txt).

```
Copyright (C) 2026 dostevskii

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, version 3.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.
```
