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

//! Wire protocol for the HM LAB Z-NEO 8K (serial/CDC, 256-byte CRC32 packets).
//!
//! The layouts below follow the public `hm-hub` implementation (AGPL-3.0-or-later).
//! They have NOT been independently verified against every firmware revision.

use anyhow::{bail, Result};
use std::fmt;

pub const PACKET_SIZE: usize = 256;
pub const PAYLOAD_SIZE: usize = 251;
pub const CRC_OFFSET: usize = 252;
pub const CHUNK_DATA_SIZE: usize = 240;

pub const CMD_HANDSHAKE: u8 = 1;
pub const CMD_CONFIG: u8 = 3;
pub const CMD_FLASH: u8 = 8;
pub const CMD_POWER: u8 = 9;
pub const CMD_LOG: u8 = 10;

pub const FLASH_HEADER_AREA: usize = 8192;
pub const FRAME_HEADER_SIZE: usize = 28;
pub const MAX_FRAME_HEADERS: usize = 292;
pub const FRAME_MAGIC: u32 = 0xC019_0001;
pub const DISPLAY_WIDTH: u32 = 320;
pub const DISPLAY_HEIGHT: u32 = 170;
pub const FRAME_PIXEL_SIZE: usize = (DISPLAY_WIDTH * DISPLAY_HEIGHT * 2) as usize;

pub const HM_VID: u16 = 0xC019;
pub const HM_PID: u16 = 0x0401;

/// One fixed-size protocol packet.
pub struct Packet {
    pub buf: [u8; PACKET_SIZE],
}

impl Packet {
    pub fn new(cmd_id: u8, payload: &[u8]) -> Result<Self> {
        if payload.len() > PAYLOAD_SIZE {
            bail!("payload too large: {} > {}", payload.len(), PAYLOAD_SIZE);
        }
        let mut buf = [0u8; PACKET_SIZE];
        buf[0] = cmd_id;
        buf[1..1 + payload.len()].copy_from_slice(payload);
        let crc = crc32fast::hash(&buf[..CRC_OFFSET]);
        buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
        Ok(Packet { buf })
    }

    pub fn from_bytes(buf: [u8; PACKET_SIZE]) -> Result<Self> {
        let expected = crc32fast::hash(&buf[..CRC_OFFSET]);
        let actual = u32::from_le_bytes(buf[CRC_OFFSET..].try_into().unwrap());
        if expected != actual {
            bail!("packet CRC mismatch: expected {expected:#x}, got {actual:#x}");
        }
        Ok(Packet { buf })
    }

    pub fn cmd_id(&self) -> u8 {
        self.buf[0]
    }

    pub fn payload(&self) -> &[u8] {
        &self.buf[1..CRC_OFFSET]
    }
}

// ---------------------------------------------------------------- builders

pub fn build_handshake() -> Packet {
    Packet::new(CMD_HANDSHAKE, &[0; PAYLOAD_SIZE]).expect("fixed size")
}

pub fn build_config_read() -> Packet {
    let mut p = [0u8; PAYLOAD_SIZE];
    p[0] = 1;
    Packet::new(CMD_CONFIG, &p).expect("fixed size")
}

pub fn build_flash_start(total_size: u32) -> Packet {
    let mut p = [0u8; PAYLOAD_SIZE];
    p[0] = 1;
    p[1..5].copy_from_slice(&total_size.to_le_bytes());
    Packet::new(CMD_FLASH, &p).expect("fixed size")
}

pub fn build_flash_data_response(offset: u32, length: u16, data: &[u8]) -> Packet {
    let mut p = [0u8; PAYLOAD_SIZE];
    p[0] = 2;
    p[1..5].copy_from_slice(&offset.to_le_bytes());
    p[5..7].copy_from_slice(&length.to_le_bytes());
    let len = data.len().min(PAYLOAD_SIZE - 7);
    p[7..7 + len].copy_from_slice(&data[..len]);
    Packet::new(CMD_FLASH, &p).expect("fixed size")
}

pub fn build_flash_readback() -> Packet {
    let mut p = [0u8; PAYLOAD_SIZE];
    p[0] = 3;
    Packet::new(CMD_FLASH, &p).expect("fixed size")
}

/// Split `data` (+ CRC32 trailer) into host-to-device chunk packets.
pub fn encode_chunked(cmd_id: u8, sub_cmd: u8, data: &[u8]) -> Result<Vec<Packet>> {
    let crc = crc32fast::hash(data);
    let mut full = Vec::with_capacity(data.len() + 4);
    full.extend_from_slice(data);
    full.extend_from_slice(&crc.to_le_bytes());

    let total_len = full.len();
    let total_chunks = total_len.div_ceil(CHUNK_DATA_SIZE);
    let mut packets = Vec::with_capacity(total_chunks);
    for i in 0..total_chunks {
        let offset = i * CHUNK_DATA_SIZE;
        let size = (total_len - offset).min(CHUNK_DATA_SIZE);
        let mut p = [0u8; PAYLOAD_SIZE];
        p[0] = sub_cmd;
        p[1..3].copy_from_slice(&(total_chunks as u16).to_le_bytes());
        p[3..5].copy_from_slice(&(i as u16).to_le_bytes());
        p[5..7].copy_from_slice(&(size as u16).to_le_bytes());
        p[7..7 + size].copy_from_slice(&full[offset..offset + size]);
        packets.push(Packet::new(cmd_id, &p)?);
    }
    Ok(packets)
}

/// Reassembles device-to-host chunked data (config read-back).
#[derive(Default)]
pub struct ChunkedReceiver {
    buffer: Vec<u8>,
    total_chunks: usize,
    received: usize,
    initialized: bool,
}

impl ChunkedReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// `payload` is the config packet payload without its sub-command byte.
    pub fn feed(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>> {
        if payload.len() < 4 {
            bail!("chunk header truncated");
        }
        let total = payload[1] as usize;
        let chunk_len = u16::from_le_bytes([payload[2], payload[3]]) as usize;
        if 4 + chunk_len > payload.len() {
            bail!("chunk length {chunk_len} exceeds packet");
        }
        if !self.initialized {
            self.total_chunks = total;
            self.initialized = true;
        }
        self.buffer.extend_from_slice(&payload[4..4 + chunk_len]);
        self.received += 1;
        if self.received < self.total_chunks {
            return Ok(None);
        }
        if self.buffer.len() < 4 {
            bail!("chunked data too small");
        }
        let n = self.buffer.len() - 4;
        let expected = crc32fast::hash(&self.buffer[..n]);
        let actual = u32::from_le_bytes(self.buffer[n..].try_into().unwrap());
        if expected != actual {
            bail!("chunked CRC mismatch: expected {expected:#x}, got {actual:#x}");
        }
        self.buffer.truncate(n);
        Ok(Some(std::mem::take(&mut self.buffer)))
    }
}

// ------------------------------------------------------------ device types

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub hw_id: u32,
    pub fw_ver: u32,
    pub flash_size: u32,
}

impl DeviceInfo {
    pub fn max_frames(&self) -> usize {
        (self.flash_size as usize).saturating_sub(FLASH_HEADER_AREA) / FRAME_PIXEL_SIZE
    }

    pub fn fw_string(&self) -> String {
        format!(
            "{}.{}.{}",
            (self.fw_ver >> 16) & 0xFF,
            (self.fw_ver >> 8) & 0xFF,
            self.fw_ver & 0xFF
        )
    }
}

pub fn parse_handshake(p: &Packet) -> Result<DeviceInfo> {
    if p.cmd_id() != CMD_HANDSHAKE {
        bail!("expected handshake response, got cmd {}", p.cmd_id());
    }
    let b = p.payload();
    Ok(DeviceInfo {
        hw_id: u32::from_le_bytes(b[0..4].try_into().unwrap()),
        fw_ver: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        flash_size: u32::from_le_bytes(b[8..12].try_into().unwrap()),
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PowerStats {
    pub bus_mv: u16,
    pub port_ma: [u16; 3],
}

pub fn parse_power(p: &Packet) -> Result<PowerStats> {
    if p.cmd_id() != CMD_POWER {
        bail!("expected power stats, got cmd {}", p.cmd_id());
    }
    let b = p.payload();
    let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    Ok(PowerStats { bus_mv: u(0), port_ma: [u(2), u(4), u(6)] })
}

pub fn parse_log(p: &Packet) -> String {
    let b = p.payload();
    let len = (b[0] as usize).min(b.len() - 1);
    String::from_utf8_lossy(&b[1..1 + len]).to_string()
}

/// 24-byte device configuration block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    pub raw: [u8; 24],
}

/// Rotation codes as used by the device: 0=0°, 1=180°, 2=90°, 3=270°.
pub const ROTATIONS: [(u8, u16); 4] = [(0, 0), (2, 90), (1, 180), (3, 270)];

impl DeviceConfig {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 24 {
            bail!("config data too short: {} < 24", data.len());
        }
        let mut raw = [0u8; 24];
        raw.copy_from_slice(&data[..24]);
        Ok(Self { raw })
    }

    pub fn to_bytes(&self) -> [u8; 24] {
        self.raw
    }

    pub fn memory_page(&self) -> u8 { self.raw[2] }
    pub fn rotation_code(&self) -> u8 { self.raw[3] }
    pub fn brightness(&self) -> u8 { self.raw[4] }
    pub fn crop_flag(&self) -> u8 { self.raw[5] }
    pub fn screen_onoff_by_usb(&self) -> u8 { self.raw[12] }
    pub fn switch_random(&self) -> u8 { self.raw[19] }
    pub fn switch_interval(&self) -> u8 { self.raw[22] }

    pub fn rotation_degrees(&self) -> Option<u16> {
        ROTATIONS.iter().find(|(c, _)| *c == self.raw[3]).map(|(_, d)| *d)
    }

    pub fn set_brightness(&mut self, v: u8) -> Result<()> {
        if v > 30 {
            bail!("brightness must be 0-30");
        }
        self.raw[4] = v;
        Ok(())
    }

    pub fn set_rotation_degrees(&mut self, deg: u16) -> Result<()> {
        match ROTATIONS.iter().find(|(_, d)| *d == deg) {
            Some((c, _)) => {
                self.raw[3] = *c;
                Ok(())
            }
            None => bail!("rotation must be 0, 90, 180 or 270"),
        }
    }

    pub fn set_screen_onoff_by_usb(&mut self, on: bool) { self.raw[12] = on as u8; }
    pub fn set_switch_random(&mut self, on: bool) { self.raw[19] = on as u8; }
    pub fn set_switch_interval(&mut self, secs: u8) { self.raw[22] = secs; }
}

impl fmt::Display for DeviceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "brightness={} rotation={:?} interval={}s random={}",
            self.brightness(),
            self.rotation_degrees(),
            self.switch_interval(),
            self.switch_random()
        )
    }
}

// ------------------------------------------------------------ flash layout

/// A decoded album: one still (1 frame) or an animation (n frames), RGB565 big-endian.
pub struct Album {
    pub frames: Vec<Vec<u8>>,
    pub delay_ms: u16,
}

pub fn max_frames_for(flash_size: u32) -> usize {
    DeviceInfo { flash_size, ..Default::default() }.max_frames()
}

pub fn write_frame_header(
    buf: &mut [u8],
    frame_count: u16,
    delay_ms: u16,
    data_offset: u32,
    data_length: u32,
    data_crc: u32,
) {
    let mut pos = 0;
    let mut put = |bytes: &[u8]| {
        buf[pos..pos + bytes.len()].copy_from_slice(bytes);
        pos += bytes.len();
    };
    put(&FRAME_MAGIC.to_le_bytes());
    put(&(DISPLAY_WIDTH as u16).to_le_bytes());
    put(&(DISPLAY_HEIGHT as u16).to_le_bytes());
    put(&frame_count.to_le_bytes());
    put(&delay_ms.to_le_bytes());
    put(&data_offset.to_le_bytes());
    put(&data_length.to_le_bytes());
    put(&data_crc.to_le_bytes());
    let hdr_crc = crc32fast::hash(&buf[..24]);
    buf[24..28].copy_from_slice(&hdr_crc.to_le_bytes());
}

/// Build the complete flash image (header table + pixel data).
pub fn build_flash_buffer(albums: &[Album], flash_size: u32) -> Result<Vec<u8>> {
    let max = max_frames_for(flash_size);
    if albums.is_empty() {
        bail!("no albums to upload");
    }
    if albums.len() > MAX_FRAME_HEADERS {
        bail!("too many albums: {} > {}", albums.len(), MAX_FRAME_HEADERS);
    }
    let total_frames: usize = albums.iter().map(|a| a.frames.len()).sum();
    if total_frames > max {
        bail!("total frames ({total_frames}) exceeds device capacity ({max})");
    }
    let total_pixels: usize = albums.iter().flat_map(|a| a.frames.iter()).map(|f| f.len()).sum();
    let mut buffer = vec![0u8; FLASH_HEADER_AREA + total_pixels];
    let mut data_offset = FLASH_HEADER_AREA;
    for (i, album) in albums.iter().enumerate() {
        let start = data_offset;
        for frame in &album.frames {
            buffer[data_offset..data_offset + frame.len()].copy_from_slice(frame);
            data_offset += frame.len();
        }
        let crc = crc32fast::hash(&buffer[start..data_offset]);
        let h = i * FRAME_HEADER_SIZE;
        write_frame_header(
            &mut buffer[h..h + FRAME_HEADER_SIZE],
            album.frames.len() as u16,
            album.delay_ms,
            start as u32,
            (data_offset - start) as u32,
            crc,
        );
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip_and_crc() {
        let p = Packet::new(CMD_CONFIG, &[1, 2, 3]).unwrap();
        let q = Packet::from_bytes(p.buf).unwrap();
        assert_eq!(q.cmd_id(), CMD_CONFIG);
        assert_eq!(&q.payload()[..3], &[1, 2, 3]);
        let mut bad = p.buf;
        bad[5] ^= 0xFF;
        assert!(Packet::from_bytes(bad).is_err());
    }

    #[test]
    fn config_rotation_mapping() {
        let mut c = DeviceConfig::from_bytes(&[0; 24]).unwrap();
        for (code, deg) in ROTATIONS {
            c.set_rotation_degrees(deg).unwrap();
            assert_eq!(c.rotation_code(), code);
            assert_eq!(c.rotation_degrees(), Some(deg));
        }
        assert!(c.set_rotation_degrees(45).is_err());
        assert!(c.set_brightness(31).is_err());
        c.set_brightness(30).unwrap();
        assert_eq!(c.brightness(), 30);
    }

    #[test]
    fn max_frames_matches_8mb_flash() {
        assert_eq!(max_frames_for(8 * 1024 * 1024), 77);
    }

    #[test]
    fn flash_buffer_layout() {
        let a = Album { frames: vec![vec![0xAB; FRAME_PIXEL_SIZE]], delay_ms: 0 };
        let b = Album { frames: vec![vec![0xCD; FRAME_PIXEL_SIZE]; 2], delay_ms: 100 };
        let buf = build_flash_buffer(&[a, b], 8 * 1024 * 1024).unwrap();
        assert_eq!(buf.len(), FLASH_HEADER_AREA + 3 * FRAME_PIXEL_SIZE);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), FRAME_MAGIC);
        let off2 = u32::from_le_bytes(buf[28 + 12..28 + 16].try_into().unwrap()) as usize;
        assert_eq!(off2, FLASH_HEADER_AREA + FRAME_PIXEL_SIZE);
        assert_eq!(buf[off2], 0xCD);
        // header CRC covers the first 24 bytes
        let crc = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        assert_eq!(crc, crc32fast::hash(&buf[..24]));
    }

    #[test]
    fn flash_buffer_rejects_overflow() {
        let a = Album { frames: vec![vec![0; FRAME_PIXEL_SIZE]; 78], delay_ms: 50 };
        assert!(build_flash_buffer(&[a], 8 * 1024 * 1024).is_err());
    }

    #[test]
    fn chunk_encoding_splits_and_appends_crc() {
        let data = vec![7u8; 500];
        let pk = encode_chunked(CMD_CONFIG, 2, &data).unwrap();
        assert_eq!(pk.len(), (500 + 4usize).div_ceil(CHUNK_DATA_SIZE));
    }
}
