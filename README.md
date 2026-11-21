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

## License

AGPL-3.0-or-later. Device protocol derived from hm-hub (Creationsss/hm-hub).
