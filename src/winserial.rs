// Z-NEO Manager - portable manager for the HM LAB Z-NEO 8K USB hub LCD.
// Copyright (C) 2026 charlesctchau
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the LICENSE file.

//! Minimal Win32 COM port that opens the port the way Chrome's Web Serial does:
//! DTR and RTS are enabled by the very first DCB (no low-to-high toggling, which the
//! `serialport` crate causes because it first disables DTR).

#![cfg(windows)]

use std::io::{self, Read, Write};
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
};
use windows_sys::Win32::Devices::Communication::{
    GetCommState, PurgeComm, SetCommState, SetCommTimeouts, COMMTIMEOUTS, DCB,
};

const PURGE_RXCLEAR: u32 = 0x0008;
const PURGE_TXCLEAR: u32 = 0x0004;

/// How to configure the line after opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 115200 8N1 with DTR+RTS enabled from the first DCB (Chrome-like).
    ChromeLike,
    /// Do not touch the DCB at all; keep the driver defaults.
    DriverDefault,
}

pub struct RawPort {
    handle: HANDLE,
}

// The handle is only used from one thread at a time.
unsafe impl Send for RawPort {}

impl RawPort {
    pub fn open(name: &str, baud: u32, mode: Mode, read_timeout: Duration) -> io::Result<RawPort> {
        let full = if name.starts_with('\\') { name.to_string() } else { format!(r"\\.\{name}") };
        let wide: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut port = RawPort { handle };

        if mode == Mode::ChromeLike {
            let mut dcb: DCB = unsafe { std::mem::zeroed() };
            dcb.DCBlength = std::mem::size_of::<DCB>() as u32;
            if unsafe { GetCommState(handle, &mut dcb) } == 0 {
                return Err(io::Error::last_os_error());
            }
            dcb.BaudRate = baud;
            dcb.ByteSize = 8;
            dcb.Parity = 0; // NOPARITY
            dcb.StopBits = 0; // ONESTOPBIT
            // Bit layout: fBinary:0 fParity:1 fOutxCts:2 fOutxDsr:3 fDtrControl:4-5
            // fDsrSens:6 fTXContinue:7 fOutX:8 fInX:9 fErrChar:10 fNull:11
            // fRtsControl:12-13 fAbortOnError:14
            const DTR_CONTROL_ENABLE: u32 = 1;
            const RTS_CONTROL_ENABLE: u32 = 1;
            dcb._bitfield = 1 | (DTR_CONTROL_ENABLE << 4) | (RTS_CONTROL_ENABLE << 12);
            if unsafe { SetCommState(handle, &dcb) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        port.apply_timeout(read_timeout)?;
        Ok(port)
    }

    fn apply_timeout(&mut self, d: Duration) -> io::Result<()> {
        let ms = d.as_millis().min(u32::MAX as u128 - 1) as u32;
        let t = COMMTIMEOUTS {
            // MAXDWORD + MAXDWORD + constant: return at once if bytes are waiting,
            // otherwise wait up to `ms` for the first byte.
            ReadIntervalTimeout: u32::MAX,
            ReadTotalTimeoutMultiplier: u32::MAX,
            ReadTotalTimeoutConstant: ms.max(1),
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 3000,
        };
        if unsafe { SetCommTimeouts(self.handle, &t) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn purge_input(&mut self) -> io::Result<()> {
        if unsafe { PurgeComm(self.handle, PURGE_RXCLEAR) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn purge_all(&mut self) -> io::Result<()> {
        if unsafe { PurgeComm(self.handle, PURGE_RXCLEAR | PURGE_TXCLEAR) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn set_timeout(&mut self, d: Duration) -> io::Result<()> {
        self.apply_timeout(d)
    }
}

impl Drop for RawPort {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Read for RawPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n: u32 = 0;
        let ok = unsafe {
            ReadFile(self.handle, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n, std::ptr::null_mut())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "read timed out"));
        }
        Ok(n as usize)
    }
}

impl Write for RawPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut n: u32 = 0;
        let ok = unsafe {
            WriteFile(self.handle, buf.as_ptr().cast(), buf.len() as u32, &mut n, std::ptr::null_mut())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // FlushFileBuffers on a virtual COM port can block; writes are already synchronous.
        let _ = unsafe { FlushFileBuffers(self.handle) };
        Ok(())
    }
}
