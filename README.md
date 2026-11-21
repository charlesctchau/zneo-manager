# Z-NEO Manager

Portable Windows manager for the HM LAB Z-NEO 8K USB hub LCD (320x170).
Automatic picture rotation from a folder, manual upload with exact preview,
device settings, backup/restore, system tray, start with Windows.

Status: protocol verified only against a mock device; not yet tested on real hardware.

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
