# Reverything
Simple Everything clone written in rust. The app reads the Master File Table file, which contains a record for every file 
on a volume. A simple UI with a search feature and live updating of the NTFS index by reading the journal is included 
as well.

# Building
```
cargo build --release
```
If you want to run it, you need to do so from an elevated shell.

# Resources 
- https://flatcap.github.io/linux-ntfs
- https://github.com/mgeeky/ntfs-journal-viewer
- https://github.com/kikijiki/ntfs-reader
