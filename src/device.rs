// Z-NEO Manager - portable manager for the HM LAB Z-NEO 8K USB hub LCD.
// Copyright (C) 2026 charlesctchau
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The device protocol (packet layout, flash layout, config block) is derived from
// hm-hub by Creationsss (https://github.com/Creationsss/hm-hub),
// licensed AGPL-3.0-or-later.
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the LICENSE file.

//! Device session: handshake, config, flash upload/readback, power.

use anyhow::{bail, Context, Result};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use crate::proto::*;

pub const NORMAL_TIMEOUT: Duration = Duration::from_millis(2000);
pub const ERASE_TIMEOUT: Duration = Duration::from_secs(60);
pub const SERIAL_BAUD: u32 = 115_200;

/// Byte transport with a adjustable read timeout (real serial port or a test double).
pub trait Transport: Read + Write {
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()>;
    /// Drop stale bytes that were buffered before this session started.
    fn clear_input(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Transport for Box<dyn serialport::SerialPort> {
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
        self.set_timeout(d).map_err(io::Error::from)
    }
    fn clear_input(&mut self) -> io::Result<()> {
        self.clear(serialport::ClearBuffer::Input).map_err(io::Error::from)
    }
}

impl Transport for Box<dyn Transport> {
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
        (**self).set_read_timeout(d)
    }
    fn clear_input(&mut self) -> io::Result<()> {
        (**self).clear_input()
    }
}

pub type DynDevice = Device<Box<dyn Transport>>;

/// Find the COM port of the Z-NEO by USB VID/PID (enumeration only, no device I/O).
pub fn detect_port() -> Result<String> {
    let ports = serialport::available_ports().context("cannot enumerate serial ports")?;
    for p in &ports {
        if let serialport::SerialPortType::UsbPort(usb) = &p.port_type {
            if usb.vid == HM_VID && usb.pid == HM_PID {
                return Ok(p.port_name.clone());
            }
        }
    }
    bail!("no HM LAB device found (VID {HM_VID:#06x} PID {HM_PID:#06x})")
}

/// All serial ports as (name, description) for the manual port picker.
pub fn list_ports() -> Vec<(String, String)> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let desc = match &p.port_type {
                serialport::SerialPortType::UsbPort(u) => format!(
                    "USB {:04X}:{:04X}{}",
                    u.vid,
                    u.pid,
                    u.product.as_ref().map(|s| format!(" {s}")).unwrap_or_default()
                ),
                serialport::SerialPortType::BluetoothPort => "Bluetooth".to_string(),
                serialport::SerialPortType::PciPort => "PCI".to_string(),
                serialport::SerialPortType::Unknown => String::new(),
            };
            (p.port_name, desc)
        })
        .collect()
}

/// Resolve the port: a manual override (non-empty) or VID/PID auto-detection.
pub fn resolve_port(override_port: &str) -> Result<String> {
    let o = override_port.trim();
    if o.is_empty() {
        detect_port()
    } else {
        Ok(o.to_string())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// Raw-capture diagnostics: try several line settings and report exactly which
/// bytes the port returns. Returns one report line per attempt.
pub fn diagnose(path: &str) -> Vec<String> {
    // (dtr, rts, send handshake first)
    let variants = [
        (true, true, false),
        (true, true, true),
        (false, false, true),
        (true, false, true),
        (false, true, true),
    ];
    let mut out = vec![format!("Diagnostics on {path} (115200 8N1)")];
    for (dtr, rts, send) in variants {
        let label = format!("DTR={} RTS={} {}", dtr as u8, rts as u8, if send { "send-handshake" } else { "listen-only" });
        let mut port = match serialport::new(path, SERIAL_BAUD).timeout(Duration::from_millis(200)).open() {
            Ok(p) => p,
            Err(e) => {
                out.push(format!("{label}: cannot open: {e}"));
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let _ = port.write_data_terminal_ready(dtr);
        let _ = port.write_request_to_send(rts);
        std::thread::sleep(Duration::from_millis(400));
        let _ = port.clear(serialport::ClearBuffer::Input);
        let mut wrote = String::from("-");
        if send {
            wrote = match port.write_all(&build_handshake().buf).and_then(|_| port.flush()) {
                Ok(()) => "256 bytes written".into(),
                Err(e) => format!("write failed: {e}"),
            };
        }
        let mut got = Vec::new();
        let end = Instant::now() + Duration::from_secs(3);
        let mut buf = [0u8; 512];
        while Instant::now() < end && got.len() < 1024 {
            match port.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => {
                    out.push(format!("{label}: read error: {e}"));
                    break;
                }
            }
        }
        let valid = got.len() >= PACKET_SIZE && Packet::from_bytes(got[..PACKET_SIZE].try_into().unwrap()).is_ok();
        out.push(format!(
            "{label}: {wrote}; received {} bytes; valid packet: {valid}; first bytes: {}",
            got.len(),
            hex(&got[..got.len().min(40)])
        ));
        drop(port);
        std::thread::sleep(Duration::from_millis(1200));
    }
    out
}

pub fn open_serial(path: &str) -> Result<DynDevice> {
    let port = serialport::new(path, SERIAL_BAUD)
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        .timeout(NORMAL_TIMEOUT)
        .open()
        .with_context(|| format!("cannot open {path} (in use by another program?)"))?;
    let mut port = port;
    // CDC-ACM devices on Windows usually stay silent until the host asserts DTR.
    let _ = port.write_data_terminal_ready(true);
    let _ = port.write_request_to_send(true);
    std::thread::sleep(Duration::from_millis(250));
    Device::new(Box::new(port) as Box<dyn Transport>)
}

/// Resolve (override or auto-detect) + open in one step.
pub fn open_port(override_port: &str) -> Result<(String, DynDevice)> {
    let name = resolve_port(override_port)?;
    let dev = open_serial(&name)?;
    Ok((name, dev))
}

fn recv_packet<T: Transport>(io: &mut T, timeout: Duration) -> Result<Packet> {
    io.set_read_timeout(timeout)?;
    let mut buf = [0u8; PACKET_SIZE];
    let mut pos = 0;
    let deadline = Instant::now() + timeout;
    while pos < PACKET_SIZE {
        if Instant::now() > deadline {
            bail!("timeout waiting for packet ({pos}/{PACKET_SIZE} bytes)");
        }
        match io.read(&mut buf[pos..]) {
            Ok(0) => bail!("device closed the connection"),
            Ok(n) => pos += n,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                if pos == 0 {
                    bail!("timeout waiting for packet");
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Packet::from_bytes(buf)
}

pub struct Device<T: Transport> {
    io: T,
    pub info: DeviceInfo,
    /// Log lines pushed by the device firmware during the session.
    pub device_log: Vec<String>,
}

impl<T: Transport> Device<T> {
    pub fn new(io: T) -> Result<Self> {
        let mut dev = Device { io, info: DeviceInfo::default(), device_log: Vec::new() };
        let _ = dev.io.clear_input();
        dev.handshake()?;
        Ok(dev)
    }

    fn send(&mut self, p: &Packet) -> Result<()> {
        self.io.write_all(&p.buf)?;
        self.io.flush()?;
        Ok(())
    }

    fn note_log(&mut self, p: &Packet) {
        let msg = parse_log(p);
        if !msg.is_empty() && self.device_log.len() < 200 {
            self.device_log.push(msg);
        }
    }

    fn handshake(&mut self) -> Result<()> {
        let mut last_err = anyhow::anyhow!("no handshake response");
        for _ in 0..3 {
            self.send(&build_handshake())?;
            for _ in 0..10 {
                match recv_packet(&mut self.io, Duration::from_millis(1500)) {
                    Ok(resp) if resp.cmd_id() == CMD_HANDSHAKE => {
                        self.info = parse_handshake(&resp)?;
                        if self.info.flash_size == 0 {
                            bail!("device reported flash size 0 (unexpected handshake data)");
                        }
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        last_err = e;
                        let _ = self.io.clear_input();
                        break;
                    }
                }
            }
        }
        Err(last_err.context("device did not answer the handshake (wrong COM port, or another program is using it?)"))
    }

    pub fn read_config(&mut self) -> Result<DeviceConfig> {
        self.send(&build_config_read())?;
        let mut rx = ChunkedReceiver::new();
        let mut unexpected = 0;
        loop {
            let resp = recv_packet(&mut self.io, NORMAL_TIMEOUT)?;
            match resp.cmd_id() {
                CMD_CONFIG => {
                    let p = resp.payload();
                    match p[0] {
                        1 => {}
                        2 => {
                            if let Some(data) = rx.feed(&p[1..])? {
                                return DeviceConfig::from_bytes(&data);
                            }
                        }
                        other => {
                            unexpected += 1;
                            if unexpected > 10 {
                                bail!("unexpected config sub-command {other}");
                            }
                        }
                    }
                }
                CMD_LOG => self.note_log(&resp),
                CMD_POWER => {}
                _ => {
                    unexpected += 1;
                    if unexpected > 10 {
                        bail!("config read failed after {unexpected} unexpected packets");
                    }
                }
            }
        }
    }

    /// Write config, then read it back and compare (fails if the device did not take it).
    pub fn write_config_verified(&mut self, cfg: &DeviceConfig) -> Result<DeviceConfig> {
        for pkt in encode_chunked(CMD_CONFIG, 2, &cfg.to_bytes())? {
            self.send(&pkt)?;
        }
        std::thread::sleep(Duration::from_millis(300));
        let back = self.read_config()?;
        if back != *cfg {
            bail!("device config read-back differs from the written value");
        }
        Ok(back)
    }

    /// Upload a complete flash image. `progress(done, total)` is called per chunk.
    pub fn upload_flash(&mut self, data: &[u8], progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        let total = data.len() as u64;
        self.send(&build_flash_start(data.len() as u32))?;
        loop {
            let resp = recv_packet(&mut self.io, ERASE_TIMEOUT).context("upload stalled")?;
            if resp.cmd_id() == CMD_LOG {
                self.note_log(&resp);
                continue;
            }
            if resp.cmd_id() != CMD_FLASH {
                continue;
            }
            let p = resp.payload();
            match p[0] {
                1 => {} // erase phase notification
                2 => {
                    let offset = u32::from_le_bytes(p[1..5].try_into().unwrap());
                    let length = u16::from_le_bytes(p[5..7].try_into().unwrap());
                    let start = offset as usize;
                    if start >= data.len() {
                        bail!("device requested offset {start} beyond image size");
                    }
                    let end = (start + length as usize).min(data.len());
                    self.send(&build_flash_data_response(offset, length, &data[start..end]))?;
                    progress(end as u64, total);
                    if end >= data.len() {
                        self.wait_flash_done();
                        return Ok(());
                    }
                }
                4 => {
                    progress(total, total);
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    /// After the last chunk, give the device a moment to confirm (best effort).
    fn wait_flash_done(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match recv_packet(&mut self.io, Duration::from_millis(500)) {
                Ok(p) if p.cmd_id() == CMD_FLASH && p.payload()[0] == 4 => return,
                Ok(p) if p.cmd_id() == CMD_LOG => self.note_log(&p),
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    pub fn read_flash(&mut self, progress: &mut dyn FnMut(u64, u64)) -> Result<Vec<u8>> {
        let size = self.info.flash_size as usize;
        self.send(&build_flash_readback())?;
        let mut buf = vec![0u8; size];
        loop {
            let resp = recv_packet(&mut self.io, NORMAL_TIMEOUT).context("flash read stalled")?;
            match (resp.cmd_id(), resp.payload()[0]) {
                (CMD_FLASH, 3) => {
                    let p = resp.payload();
                    let off = u32::from_le_bytes(p[1..5].try_into().unwrap()) as usize;
                    let len = u16::from_le_bytes(p[5..7].try_into().unwrap()) as usize;
                    if 7 + len > p.len() {
                        bail!("flash chunk length out of range");
                    }
                    if off + len <= buf.len() {
                        buf[off..off + len].copy_from_slice(&p[7..7 + len]);
                    }
                    progress((off + len) as u64, size as u64);
                }
                (CMD_FLASH, 4) => return Ok(buf),
                (CMD_LOG, _) => self.note_log(&resp),
                _ => {}
            }
        }
    }

    /// The device pushes power packets periodically; wait up to `timeout` for one.
    pub fn read_power(&mut self, timeout: Duration) -> Result<PowerStats> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("no power packet received");
            }
            let resp = recv_packet(&mut self.io, left)?;
            match resp.cmd_id() {
                CMD_POWER => return parse_power(&resp),
                CMD_LOG => self.note_log(&resp),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
pub mod mock {
    //! In-memory device that speaks the same packet layouts as the host code expects.
    use super::*;
    use std::collections::VecDeque;

    pub struct MockDevice {
        pub rx: VecDeque<u8>,   // bytes the host will read
        pub tx: Vec<u8>,        // bytes the host wrote (partial packet buffer)
        pub config: [u8; 24],
        pub flash: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        pub flash_size: u32,
        pub flash_expect: usize,
        pub flash_next: usize,
    }

    impl MockDevice {
        pub fn new(flash_size: u32) -> Self {
            let mut config = [0u8; 24];
            config[4] = 10;
            Self {
                rx: VecDeque::new(),
                tx: Vec::new(),
                config,
                flash: Default::default(),
                flash_size,
                flash_expect: 0,
                flash_next: 0,
            }
        }

        fn push(&mut self, cmd: u8, payload: &[u8]) {
            let p = Packet::new(cmd, payload).unwrap();
            self.rx.extend(p.buf.iter());
        }

        fn request_chunk(&mut self) {
            let len = 240usize.min(self.flash_expect - self.flash_next);
            let mut pl = [0u8; 8];
            pl[0] = 2;
            pl[1..5].copy_from_slice(&(self.flash_next as u32).to_le_bytes());
            pl[5..7].copy_from_slice(&(len as u16).to_le_bytes());
            self.push(CMD_FLASH, &pl);
        }

        fn handle(&mut self, pkt: Packet) {
            let p = pkt.payload().to_vec();
            match pkt.cmd_id() {
                CMD_HANDSHAKE => {
                    let mut pl = [0u8; 12];
                    pl[0..4].copy_from_slice(&0xABCDu32.to_le_bytes());
                    pl[4..8].copy_from_slice(&0x0001_0203u32.to_le_bytes());
                    pl[8..12].copy_from_slice(&self.flash_size.to_le_bytes());
                    // a stray power packet first, like the real device pushes
                    self.push(CMD_POWER, &[0x80, 0x12, 1, 0, 2, 0, 3, 0]);
                    self.push(CMD_HANDSHAKE, &pl);
                }
                CMD_CONFIG if p[0] == 1 => {
                    let mut data = self.config.to_vec();
                    let crc = crc32fast::hash(&data);
                    data.extend_from_slice(&crc.to_le_bytes());
                    // device->host layout: [2][idx][total][len u16][data...]
                    let mut pl = vec![2u8, 0, 1];
                    pl.extend_from_slice(&(data.len() as u16).to_le_bytes());
                    pl.extend_from_slice(&data);
                    self.push(CMD_CONFIG, &pl);
                }
                CMD_CONFIG if p[0] == 2 => {
                    let len = u16::from_le_bytes([p[5], p[6]]) as usize;
                    self.config.copy_from_slice(&p[7..7 + 24.min(len)]);
                }
                CMD_FLASH if p[0] == 1 => {
                    self.flash_expect = u32::from_le_bytes(p[1..5].try_into().unwrap()) as usize;
                    *self.flash.lock().unwrap() = vec![0; self.flash_expect];
                    self.flash_next = 0;
                    self.push(CMD_FLASH, &[1, 2]);
                    self.request_chunk();
                }
                CMD_FLASH if p[0] == 2 => {
                    let off = u32::from_le_bytes(p[1..5].try_into().unwrap()) as usize;
                    let len = u16::from_le_bytes([p[5], p[6]]) as usize;
                    self.flash.lock().unwrap()[off..off + len].copy_from_slice(&p[7..7 + len]);
                    self.flash_next = off + len;
                    if self.flash_next < self.flash_expect {
                        self.request_chunk();
                    } else {
                        self.push(CMD_FLASH, &[4]);
                    }
                }
                _ => {}
            }
        }
    }

    impl Read for MockDevice {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.rx.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "mock timeout"));
            }
            let n = buf.len().min(self.rx.len());
            for b in buf.iter_mut().take(n) {
                *b = self.rx.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for MockDevice {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.tx.extend_from_slice(data);
            while self.tx.len() >= PACKET_SIZE {
                let raw: Vec<u8> = self.tx.drain(..PACKET_SIZE).collect();
                let pkt = Packet::from_bytes(raw.try_into().unwrap()).expect("host sent bad CRC");
                self.handle(pkt);
            }
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Transport for MockDevice {
        fn set_read_timeout(&mut self, _d: Duration) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockDevice;
    use super::*;

    #[test]
    fn handshake_skips_stray_power_packet() {
        let dev = Device::new(MockDevice::new(8 * 1024 * 1024)).unwrap();
        assert_eq!(dev.info.max_frames(), 77);
        assert_eq!(dev.info.fw_string(), "1.2.3");
    }

    #[test]
    fn config_write_is_verified_by_readback() {
        let mut dev = Device::new(MockDevice::new(8 * 1024 * 1024)).unwrap();
        let mut cfg = dev.read_config().unwrap();
        assert_eq!(cfg.brightness(), 10);
        cfg.set_brightness(25).unwrap();
        cfg.set_rotation_degrees(90).unwrap();
        let back = dev.write_config_verified(&cfg).unwrap();
        assert_eq!(back.brightness(), 25);
        assert_eq!(back.rotation_degrees(), Some(90));
    }

    #[test]
    fn flash_upload_transfers_every_byte() {
        let mut dev = Device::new(MockDevice::new(8 * 1024 * 1024)).unwrap();
        let album = Album { frames: vec![vec![0x5A; FRAME_PIXEL_SIZE]], delay_ms: 0 };
        let img = build_flash_buffer(&[album], dev.info.flash_size).unwrap();
        let mut last = (0, 0);
        dev.upload_flash(&img, &mut |d, t| last = (d, t)).unwrap();
        assert_eq!(last.0, last.1);
        assert_eq!(*dev.io.flash.lock().unwrap(), img);
    }
}
