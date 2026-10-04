# Reverything
Simple Everything clone written in Rust. It indexes the fixed NTFS volumes you pick by reading the Master File Table
directly, keeps the index up to date from the USN change journal and searches it as you type.

## Architecture
```
crates/
  reverything-core/      NTFS indexing, search, saving/loading and following the journal (library)
  reverything-protocol/  messages between the service and its clients, plus a pipe client
  reverything-service/   Windows service that owns the index and serves searches on a named pipe
  reverything-ui/        the search window (GPUI), runs as the normal user
installer/               Inno Setup script
scripts/                 icon generator, installer build, benchmark task
```

- **Service** (`reverything-service.exe`): runs as LocalSystem, because reading raw volumes needs it. For every
  volume turned on in the settings (none by default, the choice is saved in `%ProgramData%\Reverything\config.json`)
  it loads the saved index from `%ProgramData%\Reverything` (or scans the MFT if there is none), follows the journal
  and answers requests on `\\.\pipe\reverything`. Turning a volume off drops its index and deletes the saved one. The pipe and the data directory are restricted to SYSTEM, administrators and
  interactively logged on users, because they expose every file name. Clients are treated as untrusted: message
  sizes are capped and the service never opens or changes files for them.
- **Window** (`reverything.exe`): asks the service for the number of results and only fetches the rows around the
  visible area. Opening files, the context menu, file icons, the tray icon and the global shortcut all run in this
  process, as the user. It stays in the tray when closed, so showing it again is instant.

### Indexing
- `$MFT` is located through its first record, `$MFT:$BITMAP` is used to skip unused records, and the table is read
  with a few large unbuffered overlapped reads (3 workers × 32 MB, double buffered). Parsing a record is a single
  pass over its attributes without allocations, so the scan runs at disk speed (~2 GB/s on NVMe).
- The index is a struct of arrays indexed by MFT record number plus one contiguous UTF-8 name buffer in sorted order.
  Hard link names live in a separate link table. About 60 bytes per file including the name.
- Changed records are re-read with `FSCTL_GET_NTFS_FILE_RECORD` and parsed with the same code as the scan, so names,
  sizes, dates and hard links stay exact. Folder sizes are updated incrementally.
- The index is saved only after a full scan and when the service stops (including shutdown), never while running.
  On the next start only the changes since then are applied (~0.1 s instead of a full scan).

### Resource use while not in use
Both processes do almost nothing while the window is not focused:

- **Service:** the index is only kept live while the app's window has the focus. Without it, nothing is fetched or
  applied: every 5 minutes one query checks how full the change journal is, and the journal is read (in Windows
  background mode, low CPU/disk priority) only once a quarter of it is new, to remember which files changed. Focusing
  the window applies just those. After an hour without focus the index is dropped from memory (~10 MB left);
  focusing the window loads the saved index again and applies everything that changed since it was saved (~0.1 s).
  If the journal was reset or overflowed in the meantime, the drive is scanned again (a few seconds).
- **App:** tray, menu, shortcut and second start are handled through callbacks, nothing polls. The service status is
  only polled while the window has the focus. A window hidden in the tray is closed after 10 minutes to free its
  memory and opened again on demand; started with Windows, it is only opened when first needed (~35 MB in the tray).

## Using it
| Shortcut | |
| --- | --- |
| Enter / double click | Open (folders open in the default file manager, e.g. OneCommander if it replaced Explorer) |
| Ctrl+Enter | Open the containing folder (selects the entry when Explorer is the file manager) |
| Ctrl+Shift+C | Copy the full path |
| Alt+Enter | Properties |
| Ctrl+F, Ctrl+L | Focus the search box |
| Ctrl+, | Settings |
| Alt+F | Show or hide files in the results |
| Alt+D | Show or hide folders in the results |
| Escape | Hide to the tray |
| Drag a row | Drop the file into Explorer or any other program |
| Drag a column header | Reorder the columns |
| Right click a column header | Show or hide columns |

Hovering the status icon in the bottom right shows a summary, its "Details" switch shows every timing. The `?` next
to the search box explains the search syntax. Column order, widths and visibility are saved.

### Search syntax
Terms are separated by spaces (use quotes for spaces inside a term) and all have to match. Matching is
case-insensitive.

Results are ordered by relevance unless a column is sorted (the Name column gives plain name order). Compared in
this order, ties stay in name order:
1. How the name matches: exactly, exactly without the extension, at the start, at the start of a word, anywhere.
2. Location: inside a user folder (`C:\Users\<name>`) first, inside Windows, Program Files, ProgramData, AppData,
   WinSxS, node_modules, target, the Recycle Bin or a folder starting with a dot last.
3. Same upper/lower case as typed.
4. Recently modified.

Queries without a name part (empty, or only folders like `system32\`) stay in name order.

| Query | Finds |
| --- | --- |
| `notepad` | names containing `notepad` |
| `report 2026` | names containing both `report` and `2026` |
| `"my file"` | names containing `my file` |
| `windows\system32\note` | `note` directly in a folder matching `system32`, inside one matching `windows` |
| `system32\` | everything directly in folders matching `system32` |
| `C:\Users\` | everything directly in `C:\Users` |
| `*.mp3` | names ending in `.mp3`; `*` and `?` make a term match the whole name |
| `report-??.pdf` | e.g. `report-07.pdf`, `?` is exactly one character |
| `.rs !test` | names containing `.rs` but not `test` |
| `size:>1gb`, `size:1mb..5mb`, `size:empty` | size filter (units b, kb, mb, gb, tb; folders use their total size) |
| `dm:today`, `dm:lastweek`, `dc:2024`, `dm:>=2024-05-01`, `dm:2024-01..2024-03` | modified (`dm:`) or created (`dc:`) date in local time; also `yesterday`, `thisweek`, `thismonth`, `lastmonth`, `thisyear`, `lastyear` |
| `!size:<1mb` | `!` in front of a filter negates it |
| `.rs !target\` | leaves out folders matching `target` and everything below them |
| `notepad !C:\Windows` | leaves out exactly `C:\Windows` and everything below it |

## Building
```
cargo build --release
```
The `dist` profile (`cargo build --profile dist`) adds fat LTO and a single codegen unit. It is slow to compile and
used by CI for the released installer.
GPUI compiles its shaders with `fxc.exe` from the Windows SDK. Its build script picks the newest installed SDK, which
does not always contain `fxc.exe`; in that case point `GPUI_FXC_PATH` at one that does, e.g. in
`%USERPROFILE%\.cargo\config.toml`:
```toml
[env]
GPUI_FXC_PATH = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.20348.0\x64\fxc.exe'
```

## Running
- Install the service (admin): `reverything-service install`, remove it with `reverything-service uninstall`.
- Or run it in the foreground (admin): `reverything-service --console`.
- UI development without admin rights: `reverything-service --console --offline` serves the indices saved in
  `%LOCALAPPDATA%\reverything-dev` (written by the benchmarks and `RV_SERVE_SECS`) without updating them.
- `REVERYTHING_PIPE=\\.\pipe\reverything-dev` makes the window, `query` and the console/bench service use another
  pipe, so a development service can run next to the installed one. The installed service ignores it.
- `reverything-service query <text>` searches through a running service from the command line.
  `RV_QUERY_VOLUMES=CD` first changes the indexed volumes, `RV_QUERY_NO_FILES` / `RV_QUERY_NO_FOLDERS` filter the
  results and `RV_QUERY_STATUS` prints the full status (`RV_QUERY_STATUS_ONLY` without searching, which would count
  as using the index).

## Installer
`scripts\build-installer.ps1 [-Profile dist]` builds the binaries and
`target\installer\reverything-setup-<version>.exe` with [Inno Setup 6](https://jrsoftware.org/isinfo.php)
(`winget install JRSoftware.InnoSetup`). The installer registers and starts the service, optionally adds the window
to the logon startup, and on upgrades stops the service first (which saves the index). Uninstalling removes the
service, the program, the saved index, the drive choice and the service log (`%ProgramData%\Reverything`), the settings
(`%APPDATA%\Reverything`), the UI log (`%LOCALAPPDATA%\Reverything`) and the autostart entry.
`scripts\generate-icon.py` regenerates `assets\reverything.ico`.

## CI
- `.github/workflows/ci.yml` checks formatting, runs clippy and the tests on every push.
- `.github/workflows/release.yml` builds the installer with the `dist` profile when a tag like `v0.1.0` is pushed
  (it has to match the version in `Cargo.toml`) and attaches it to a GitHub release. Started manually, it only uploads
  the installer as a build artifact.

## Testing everything manually
1. Build the installer: `scripts\build-installer.ps1`.
2. Run `target\installer\reverything-setup-<version>.exe`, confirm the UAC prompt, keep "Start Reverything when I log
   on" checked and let it start Reverything at the end.
3. No drive is indexed yet: the window says so. "Open settings", turn on the drives and close the settings. The
   status icon in the bottom right shows a spinner while the MFT is scanned (a few seconds), then "Up to date". Hover
   it for the summary, turn on "Details": the per volume sections show the full scan with its timings and "Saved
   index: None at start, saved now". Make the window small: the popup scrolls instead of being cut off.
4. Search: `notepad`, `windows\system32\`, `notepad !winsxs\`, sort by clicking column headers, scroll through an empty
   search (all files). Hover the `?` next to the search box. Toggle the file and folder buttons (or Alt+F / Alt+D).
   Drag a column header to another place, right click a header to hide a column, restart the window: both are kept.
5. Live updates: create, rename and delete a file in Explorer, then search for it. The results refresh within 10
   seconds (immediately when you change the search).
6. Rows: double click opens, right click shows the menu (open, open folder, copy path/name, properties), dragging a
   row into an Explorer window copies the file.
7. Tray: close the window (it keeps running in the tray), bring it back with the tray icon, the global shortcut
   (Settings shows which one is active) or by starting Reverything again. "Quit" in the tray menu exits. After 10
   minutes in the tray the window is closed (the process drops to ~115 MB or less); the shortcut opens it again.
8. Settings (gear in the title bar or Ctrl+,): switch light/dark, change the shortcut, toggle "Start with Windows".
   Turn a drive off: after closing the settings its results are gone and
   `%ProgramData%\Reverything\<letter>.db` is deleted. Turning it on again scans it again.
9. Restart the service (`services.msc`, "Reverything Index", or reboot): it saves the index while stopping and
   starts without loading it. Focusing the window loads it, the status popup shows "Loading the saved index" and the
   catch-up instead of a full scan. The window reconnects by itself.
10. Leave the window unfocused for an hour: the service drops the index ("Unloaded while not in use" in the
    details) and uses ~10 MB; focusing the window brings it back in about a tenth of a second.
11. Log off and on: Reverything starts hidden in the tray.
12. Uninstall from "Apps & features": afterwards `%ProgramData%\Reverything`, `%APPDATA%\Reverything`,
    `%LOCALAPPDATA%\Reverything`, the "Reverything" value in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
    and the service (`sc query Reverything`) are gone.

## Benchmarking
`reverything-service --bench` indexes every volume without the service and prints timings, memory usage and search
times. To run it elevated without running everything as admin, `scripts/register-bench-task.ps1` (run once from an
elevated PowerShell) registers a scheduled task that only runs this command. Start it with
`schtasks /run /tn ReverythingBench`; the output goes to `target/bench.log`. `scripts/unregister-bench-task.ps1`
removes the task again.

Variations are read from `target/bench.env` (`KEY=VALUE` lines):

| Variable | Effect |
| --- | --- |
| `RV_THREADS`, `RV_CHUNK_KB`, `RV_NOBUF` | Scan options |
| `RV_SWEEP=threads:chunk_kb,...` | Times scans of the first volume with each combination |
| `RV_READTEST=1` | Raw sequential read speed of the first volume at several request sizes and queue depths, then its MFT scan with and without parsing |
| `RV_PARSE=0` | Scans without parsing, to time reading alone (the index stays empty) |
| `RV_STARTUP=1` | Measures startup from the saved index including the journal replay |
| `RV_JOURNAL=1` | Creates, renames, links and deletes files in `%TEMP%` and checks the index follows |
| `RV_SERVE_SECS=90` | Runs the live service for that long, to test clients against it (volumes are turned on through a client, e.g. `RV_QUERY_VOLUMES`) |
| `REVERYTHING_PIPE=\\.\pipe\reverything-bench` | Serves on another pipe, next to the installed service |
| `RV_IDLE_CHECK_SECS=5`, `RV_UNLOAD_SECS=25` | Shorter idle timings for `RV_SERVE_SECS` (default 5 and 60 minutes) |

## Resources
- https://flatcap.github.io/linux-ntfs
- https://github.com/mgeeky/ntfs-journal-viewer
- https://github.com/kikijiki/ntfs-reader
