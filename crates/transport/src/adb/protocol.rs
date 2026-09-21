//! ADB packet codec. Same layout as `webadb-rs` `protocol.rs` (24-byte LE header,
//! magic = command ^ 0xffffffff, checksum = sum of payload bytes).

use thiserror::Error;

pub const ADB_VERSION: u32 = 0x01000001;
pub const ADB_MAXDATA: u32 = 1024 * 1024;
/// Host banner. A bare `host::name` string is not valid parameter syntax.
pub const CNXN_BANNER: &[u8] = b"host::features=shell_v2,cmd,stat_v2,ls_v2,fixed_push_mkdir,apex,abb\0";

pub const AUTH_TOKEN: u32 = 1;
pub const AUTH_SIGNATURE: u32 = 2;
pub const AUTH_RSAPUBLICKEY: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Command {
    Sync = 0x434e5953,
    Cnxn = 0x4e584e43,
    Auth = 0x48545541,
    Open = 0x4e45504f,
    Okay = 0x59414b4f,
    Clse = 0x45534c43,
    Wrte = 0x45545257,
}

impl Command {
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0x434e5953 => Some(Self::Sync),
            0x4e584e43 => Some(Self::Cnxn),
            0x48545541 => Some(Self::Auth),
            0x4e45504f => Some(Self::Open),
            0x59414b4f => Some(Self::Okay),
            0x45534c43 => Some(Self::Clse),
            0x45545257 => Some(Self::Wrte),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub command: Command,
    pub arg0: u32,
    pub arg1: u32,
    pub data_length: u32,
    pub data_crc32: u32,
    pub magic: u32,
}

impl Packet {
    pub fn new(command: Command, arg0: u32, arg1: u32, data: &[u8]) -> Self {
        Self {
            command,
            arg0,
            arg1,
            data_length: data.len() as u32,
            data_crc32: checksum(data),
            magic: (command as u32) ^ 0xffff_ffff,
        }
    }

    pub fn to_bytes(&self) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0..4].copy_from_slice(&(self.command as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&self.arg0.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.arg1.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.data_length.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.data_crc32.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.magic.to_le_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AdbError> {
        if bytes.len() < 24 {
            return Err(AdbError::Protocol("header short".into()));
        }
        let command_u = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let command = Command::from_u32(command_u)
            .ok_or_else(|| AdbError::Protocol(format!("unknown command {command_u:08x}")))?;
        let magic = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        if magic != (command as u32) ^ 0xffff_ffff {
            return Err(AdbError::Protocol("magic mismatch".into()));
        }
        Ok(Self {
            command,
            arg0: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            arg1: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            data_length: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            data_crc32: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            magic,
        })
    }
}

/// ADB "checksum": unsigned byte sum, not CRC32. Peers on version ≥ 0x01000001
/// may send 0; callers treat 0 as "not present".
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
}

#[derive(Debug, Error)]
pub enum AdbError {
    #[error("usb: {0}")]
    Usb(String),
    #[error("io: {0}")]
    Io(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("timeout waiting for device")]
    Timeout,
    #[error("stream closed")]
    Closed,
    #[error("{0}")]
    Msg(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let data = b"abc";
        let pkt = Packet::new(Command::Cnxn, ADB_VERSION, ADB_MAXDATA, data);
        let bytes = pkt.to_bytes();
        let back = Packet::from_bytes(&bytes).unwrap();
        assert_eq!(back.command, Command::Cnxn);
        assert_eq!(back.arg0, ADB_VERSION);
        assert_eq!(back.data_length, 3);
        assert_eq!(back.data_crc32, checksum(data));
        assert_eq!(back.magic, (Command::Cnxn as u32) ^ 0xffff_ffff);
    }
}
