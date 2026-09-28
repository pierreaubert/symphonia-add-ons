//! Minimal ISO-BMFF box reader.
//!
//! Reads the box header (size, `FourCC`, optional largesize, optional
//! `uuid`) and exposes helpers to walk container children with a depth cap
//! and a per-box payload limit. Only the box types the IAMF demuxer needs
//! are interpreted; everything else is skipped by size.

use std::io::{Read, Seek, SeekFrom};

use crate::error::IamfMp4Error;

/// Maximum container nesting the walker descends into.
pub const MAX_BOX_DEPTH: u8 = 16;

/// Largest single box payload buffered in memory (64 MiB).
pub const MAX_BOX_PAYLOAD: u64 = 64 * 1024 * 1024;

/// A parsed ISO-BMFF box header.
#[derive(Debug, Clone, Copy)]
pub struct BoxHeader {
    /// `FourCC` box type.
    pub typ: [u8; 4],
    /// Offset of the first payload byte in the stream.
    pub payload_offset: u64,
    /// Payload length in bytes (`u64::MAX` means "to end of file").
    pub payload_len: u64,
    /// Offset just past the box (when the length is known).
    pub end_offset: Option<u64>,
}

impl BoxHeader {
    /// Read one box header at the current stream position.
    pub fn read<R: Read + Seek>(r: &mut R) -> Result<Option<Self>, IamfMp4Error> {
        let start = r.stream_position()?;
        let mut hdr = [0u8; 8];
        match r.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => return Err(IamfMp4Error::Io(e)),
        }
        let mut size = u64::from(u32::from_be_bytes(hdr[0..4].try_into().unwrap()));
        let typ: [u8; 4] = hdr[4..8].try_into().unwrap();
        let mut header_len = 8u64;

        if size == 1 {
            let mut ext = [0u8; 8];
            r.read_exact(&mut ext).map_err(IamfMp4Error::Io)?;
            size = u64::from_be_bytes(ext);
            header_len = 16;
        } else if size == 0 {
            // Box extends to end of file.
            let end = r.seek(SeekFrom::End(0))?;
            r.seek(SeekFrom::Start(start + 8))?;
            return Ok(Some(Self {
                typ,
                payload_offset: start + 8,
                payload_len: end.saturating_sub(start + 8),
                end_offset: Some(end),
            }));
        }
        if size < header_len {
            return Err(IamfMp4Error::MalformedBox(
                box_name(typ),
                "size smaller than header",
            ));
        }
        if typ == *b"uuid" {
            let mut uuid = [0u8; 16];
            r.read_exact(&mut uuid).map_err(IamfMp4Error::Io)?;
            header_len += 16;
            if size < header_len {
                return Err(IamfMp4Error::MalformedBox(
                    "uuid",
                    "size smaller than header",
                ));
            }
        }
        Ok(Some(Self {
            typ,
            payload_offset: start + header_len,
            payload_len: size - header_len,
            end_offset: Some(start + size),
        }))
    }

    /// Buffer the whole payload, enforcing [`MAX_BOX_PAYLOAD`].
    ///
    /// # Errors
    ///
    /// Rejects payloads over [`MAX_BOX_PAYLOAD`] or unreadable streams.
    pub fn read_payload<R: Read + Seek>(&self, r: &mut R) -> Result<Vec<u8>, IamfMp4Error> {
        if self.payload_len > MAX_BOX_PAYLOAD {
            return Err(IamfMp4Error::BoxTooLarge(self.payload_len));
        }
        let len = usize::try_from(self.payload_len)
            .ok()
            .ok_or(IamfMp4Error::BoxTooLarge(self.payload_len))?;
        r.seek(SeekFrom::Start(self.payload_offset))?;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).map_err(IamfMp4Error::Io)?;
        Ok(buf)
    }

    /// Position the stream just past this box.
    pub fn skip<R: Read + Seek>(&self, r: &mut R) -> Result<(), IamfMp4Error> {
        if let Some(end) = self.end_offset {
            r.seek(SeekFrom::Start(end))?;
        }
        Ok(())
    }
}

/// Short static name for diagnostics (`"uuid"`, `"ftyp"`, ...).
fn box_name(typ: [u8; 4]) -> &'static str {
    match &typ {
        b"ftyp" => "ftyp",
        b"moov" => "moov",
        b"moof" => "moof",
        b"mdat" => "mdat",
        b"trak" => "trak",
        b"mdia" => "mdia",
        b"minf" => "minf",
        b"stbl" => "stbl",
        b"iacb" => "iacb",
        _ => "box",
    }
}

/// Walk the direct children of a container box.
///
/// `container_end` is the offset just past the container. The callback
/// receives each child header with the stream positioned at the child
/// payload start; returning `Ok(true)` continues, `Ok(false)` stops early.
pub fn walk_children<R: Read + Seek>(
    r: &mut R,
    container_end: u64,
    depth: u8,
    mut visit: impl FnMut(&mut R, &BoxHeader, u8) -> Result<bool, IamfMp4Error>,
) -> Result<(), IamfMp4Error> {
    if depth > MAX_BOX_DEPTH {
        return Err(IamfMp4Error::NestingTooDeep);
    }
    loop {
        let pos = r.stream_position()?;
        if pos >= container_end {
            return Ok(());
        }
        let Some(header) = BoxHeader::read(r)? else {
            return Ok(());
        };
        if header.payload_offset > container_end {
            return Err(IamfMp4Error::MalformedBox(
                box_name(header.typ),
                "child starts past container end",
            ));
        }
        let keep_going = visit(r, &header, depth)?;
        // Ensure the callback consumed (or explicitly left) the child:
        // reposition past it unless the callback already moved beyond.
        let after = r.stream_position()?;
        let end = header.end_offset.unwrap_or(after);
        if after < end {
            r.seek(SeekFrom::Start(end))?;
        }
        if !keep_going {
            return Ok(());
        }
    }
}

/// Read a `FullBox` header (version + flags) from the front of `buf`.
pub fn read_full_box(buf: &[u8]) -> Result<(u8, u32, &[u8]), IamfMp4Error> {
    if buf.len() < 4 {
        return Err(IamfMp4Error::MalformedBox(
            "box",
            "fullbox header truncated",
        ));
    }
    Ok((
        buf[0],
        u32::from_be_bytes([0, buf[1], buf[2], buf[3]]),
        &buf[4..],
    ))
}

/// Big-endian readers over a byte slice with bounds errors.
pub struct SliceReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    /// Wrap a buffer.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes still available.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], IamfMp4Error> {
        if self.remaining() < n {
            return Err(IamfMp4Error::MalformedBox("box", what));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Read one byte.
    pub fn u8(&mut self) -> Result<u8, IamfMp4Error> {
        Ok(self.take(1, "truncated u8")?[0])
    }

    /// Read a big-endian u16.
    pub fn u16_be(&mut self) -> Result<u16, IamfMp4Error> {
        let b = self.take(2, "truncated u16")?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Read a big-endian u32.
    pub fn u32_be(&mut self) -> Result<u32, IamfMp4Error> {
        let b = self.take(4, "truncated u32")?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a big-endian u64.
    pub fn u64_be(&mut self) -> Result<u64, IamfMp4Error> {
        let b = self.take(8, "truncated u64")?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read a `FourCC`.
    pub fn fourcc(&mut self) -> Result<[u8; 4], IamfMp4Error> {
        Ok(self.take(4, "truncated fourcc")?.try_into().unwrap())
    }

    /// Read raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], IamfMp4Error> {
        self.take(n, "truncated bytes")
    }

    /// Skip forward.
    pub fn skip(&mut self, n: usize) -> Result<(), IamfMp4Error> {
        self.take(n, "truncated skip")?;
        Ok(())
    }
}
