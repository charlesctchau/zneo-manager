
# Z-NEO Manager
<img width="997" height="822" alt="Clipboarder 2026 11 23-002" src="https://github.com/user-attachments/assets/845e4aac-5a84-4f99-ab8e-602ac848a446" />

Portable Windows manager for the HM LAB Z-NEO 8K USB hub LCD (320x170).
Automatic picture rotation from a folder, manual upload with exact preview,
device settings, backup/restore, system tray, start with Windows.

Status: tested on a real Z-NEO 8K (firmware 1.3.7) for connection, manual upload,
GIF upload and automatic picture change. Other firmware versions are untested.

Important: a COM port can only be opened by one program at a time. Close any browser tab
with hmlab.tech (Web Serial keeps the port even in the background) while using this app.

## Build (Windows target from Linux)

    rustup target add x86_64-pc-windows-gnu
    apt install gcc-mingw-w64-x86-64
    cargo build --release --target x86_64-pc-windows-gnu

Native Windows: `cargo build --release`. Tests: `cargo test`.

## Disclaimer

Not affiliated with, endorsed by or supported by HM LAB. Provided as is, without
warranty of any kind; use at your own risk. Writing to the device replaces its stored
pictures and can overwrite its settings. Hardware behaviour has not been verified yet,
so no feature should be considered supported until tested on a real unit.

## License

Copyright (C) 2026 charlesctchau. AGPL-3.0-or-later (see LICENSE).

The device protocol implementation is derived from
[hm-hub](https://github.com/Creationsss/hm-hub) by Creationsss, also AGPL-3.0-or-later.
Anyone distributing a binary must offer the corresponding source code.
