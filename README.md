# Reverything
Simple Everything clone written in Rust. It indexes every fixed NTFS volume by reading the Master File Table directly,
keeps the index up to date from the USN change journal and searches it as you type.

## How it works
- **Indexing**: `$MFT` is located through its first record, the `$MFT:$BITMAP` is used to skip unused records, and the
  table is read with a few large unbuffered overlapped reads (3 workers × 32 MB, double buffered). Parsing a record is
  a single pass over its attributes without allocations, so the scan runs at disk speed (~2 GB/s on NVMe).
- **Index layout**: struct of arrays indexed by MFT record number (name offset/length, parent, flags, size, created,
  modified, sequence number) plus one contiguous UTF-8 name buffer laid out in sorted order. Additional hard link
  names live in a separate link table. About 60 bytes per file including the name.
- **Live updates**: a thread per volume blocks on the change journal. Changed records are re-read with
  `FSCTL_GET_NTFS_FILE_RECORD` and parsed with the same code as the scan, so names, sizes, dates and hard links stay
  exact. Folder sizes are updated incrementally.
- **Persistence**: the index is saved to `%LOCALAPPDATA%\reverything\<drive>.db` after a full scan and on exit. On the
  next start it is loaded and only the journal entries since then are replayed. A full rescan happens only if the
  journal was deleted or wrapped.
- **Search**: case-insensitive substring search over all names in parallel. `folder\name` matches the parent
  directories as well, `C:\path\` anchors at the root of a volume. Results are in name order and can be sorted by any
  column.

## Building
```
cargo build --release
```
It needs to run from an elevated shell to read the volumes.

## Benchmarking
`reverything.exe --bench` indexes every volume without starting the UI and prints timings, memory usage and search
times. To run it elevated without running everything as admin, `scripts/register-bench-task.ps1` (run once from an
elevated PowerShell) registers a scheduled task that only runs this command. Start it with
`schtasks /run /tn ReverythingBench`; the output goes to `target/bench.log`. `scripts/unregister-bench-task.ps1`
removes the task again.

Variations are read from `target/bench.env` (`KEY=VALUE` lines):

| Variable | Effect |
| --- | --- |
| `RV_THREADS`, `RV_CHUNK_KB`, `RV_NOBUF` | Scan options |
| `RV_SWEEP=threads:chunk_kb,...` | Times scans of the first volume with each combination |
| `RV_STARTUP=1` | Measures startup from the saved index including the journal replay |
| `RV_JOURNAL=1` | Creates, renames, links and deletes files in `%TEMP%` and checks the index follows |

## Resources
- https://flatcap.github.io/linux-ntfs
- https://github.com/mgeeky/ntfs-journal-viewer
- https://github.com/kikijiki/ntfs-reader
