//! [`FlashKv`]: the [`OtaKv`] backend over raw NOR flash (`embedded-storage`), for a platform without NVS (the esp-hal tier, ADR 023 rung 1).
//!
//! # On-flash format
//!
//! The region is a log of whole-store snapshots, appended sector after sector and wrapping around.
//! Every `set_*` or `remove` that changes the store appends ONE snapshot of every key; that append is the commit.
//!
//! | Bytes | Field |
//! |:------|:------|
//! | 0 | format byte, `0x01` |
//! | 1 | number of entries |
//! | 2..4 | length `len` of the snapshot including the CRC, `u16` little endian |
//! | 4..8 | sequence number, `u32` little endian, one higher than the previous snapshot |
//! | 8..len-4 | entries: key length, key (1 to 15 ASCII bytes), type tag, value |
//! | len-4..len | CRC-32 (IEEE) of bytes `0..len-4` |
//!
//! A value is a `u8` (tag 1, one byte), a `u32` (tag 2, four bytes little endian) or a string (tag 3, a length byte and up to 32 ASCII bytes).
//! A snapshot is padded with `0xFF` to the flash's read and write granularity and never spans two sectors.
//!
//! # Power-loss safety
//!
//! [`FlashKv::open`] scans every sector and adopts the valid snapshot with the highest sequence number; a torn snapshot fails its CRC and the previous one wins.
//! A sector is erased only to receive the next snapshot, and never while it holds the newest valid snapshot, so a cut during an erase or a write leaves either the state before or the state after that one commit.
//! After a failed or interrupted write the rest of that sector is never written again; the next commit goes to an erased sector.
//!
//! # Wear
//!
//! A commit erases at most one sector.
//! A sector holds `ERASE_SIZE / s` snapshots of `s` bytes (padded), so `n` commits erase at most `n / (ERASE_SIZE / s) + 1` sectors, spread over every sector of the region.
//! A write that leaves the store unchanged (the same value again, or the removal of an absent key) commits nothing.
//!
//! Reads come from a RAM mirror of the newest snapshot; only [`FlashKv::open`] and the commit read flash, always in multiples of `F::READ_SIZE`.

use embedded_storage::nor_flash::NorFlash;

use super::kv::{KvError, KvStr, OtaKv};

/// Format byte of a version-1 snapshot.
///
/// A later format must keep this header (format byte, entry count, length, sequence number, CRC at the end) so this build still skips it as invalid, or use another region.
const FORMAT_V1: u8 = 0x01;

/// Bytes of the snapshot header: format, count, length, sequence number.
const HEADER_LEN: usize = 8;

/// Bytes of the trailing CRC.
const CRC_LEN: usize = 4;

/// Longest key, in bytes (the NVS limit, so both tiers accept the same keys).
pub const MAX_KEY_LEN: usize = 15;

/// Most keys one store holds; the `ota` namespace uses 22.
pub const MAX_ENTRIES: usize = 24;

/// Longest string value, in bytes.
const MAX_STR_LEN: usize = 32;

/// Largest encoded entry: key length, key, type tag, string length, string.
const MAX_ENTRY_LEN: usize = 1 + MAX_KEY_LEN + 1 + 1 + MAX_STR_LEN;

/// Largest snapshot, padded; [`MAX_ENTRIES`] entries of the largest size always fit.
pub const MAX_SNAPSHOT_BYTES: usize = 1216;

/// Largest read or write granularity this backend supports.
const MAX_ALIGN: usize = 64;

/// Fewest sectors a region needs: one holds the newest snapshot while another is erased.
pub const MIN_SECTORS: u32 = 2;

const _: () = assert!(HEADER_LEN + MAX_ENTRIES * MAX_ENTRY_LEN + CRC_LEN <= MAX_SNAPSHOT_BYTES);
const _: () = assert!(MAX_SNAPSHOT_BYTES.is_multiple_of(MAX_ALIGN));
const _: () = assert!(MAX_SNAPSHOT_BYTES <= u16::MAX as usize);

const TAG_U8: u8 = 1;
const TAG_U32: u8 = 2;
const TAG_STR: u8 = 3;

/// A stored value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    U8(u8),
    U32(u32),
    Str(KvStr),
}

type Key = heapless::String<MAX_KEY_LEN>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    key: Key,
    value: Value,
}

type Mirror = heapless::Vec<Entry, MAX_ENTRIES>;

/// The change one commit makes.
enum Change<'a> {
    Set(&'a str, Value),
    Remove(&'a str),
}

fn flash_err<E>(_: E) -> KvError {
    KvError::new(KvError::CODE_FLASH)
}

fn geometry() -> KvError {
    KvError::new(KvError::CODE_GEOMETRY)
}

fn full() -> KvError {
    KvError::new(KvError::CODE_STORE_FULL)
}

fn invalid_value() -> KvError {
    KvError::new(KvError::CODE_INVALID_VALUE)
}

fn type_mismatch() -> KvError {
    KvError::new(KvError::CODE_TYPE_MISMATCH)
}

/// CRC-32 (IEEE 802.3, reflected polynomial `0xEDB8_8320`, initial and final XOR `0xFFFF_FFFF`).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

const fn round_up(len: usize, align: usize) -> usize {
    len.div_ceil(align) * align
}

fn valid_key(key: &str) -> bool {
    (1..=MAX_KEY_LEN).contains(&key.len()) && key.is_ascii()
}

/// Appends bytes to the snapshot buffer, refusing to overrun it.
struct Writer<'b> {
    buf: &'b mut [u8; MAX_SNAPSHOT_BYTES],
    len: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), KvError> {
        let end = self.len.checked_add(bytes.len()).ok_or_else(full)?;
        self.buf
            .get_mut(self.len..end)
            .ok_or_else(full)?
            .copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    fn entry(&mut self, key: &str, value: &Value) -> Result<(), KvError> {
        // Keys are at most 15 bytes and strings at most 32 (checked on every set and on decode).
        self.put(&[key.len() as u8])?;
        self.put(key.as_bytes())?;
        match value {
            Value::U8(v) => self.put(&[TAG_U8, *v]),
            Value::U32(v) => {
                self.put(&[TAG_U32])?;
                self.put(&v.to_le_bytes())
            }
            Value::Str(s) => {
                self.put(&[TAG_STR, s.len() as u8])?;
                self.put(s.as_bytes())
            }
        }
    }
}

/// Encodes the snapshot of `mirror` with `change` applied; returns its length without padding.
fn encode(
    mirror: &Mirror,
    change: &Change<'_>,
    seq: u32,
    buf: &mut [u8; MAX_SNAPSHOT_BYTES],
) -> Result<usize, KvError> {
    let mut w = Writer {
        buf,
        len: HEADER_LEN,
    };
    let mut count = 0usize;
    let mut placed = false;
    for e in mirror {
        match change {
            Change::Set(key, value) if e.key.as_str() == *key => {
                w.entry(key, value)?;
                placed = true;
                count += 1;
            }
            Change::Remove(key) if e.key.as_str() == *key => {}
            _ => {
                w.entry(&e.key, &e.value)?;
                count += 1;
            }
        }
    }
    if let Change::Set(key, value) = change {
        if !placed {
            if count >= MAX_ENTRIES {
                return Err(full());
            }
            w.entry(key, value)?;
            count += 1;
        }
    }
    let len = w.len + CRC_LEN;
    // `count <= MAX_ENTRIES` and `len <= MAX_SNAPSHOT_BYTES` fit their header fields.
    w.buf[0] = FORMAT_V1;
    w.buf[1] = count as u8;
    w.buf[2..4].copy_from_slice(&(len as u16).to_le_bytes());
    w.buf[4..8].copy_from_slice(&seq.to_le_bytes());
    let crc = crc32(&w.buf[..w.len]);
    w.put(&crc.to_le_bytes())?;
    Ok(len)
}

/// Applies a committed change to the mirror.
fn apply(mirror: &mut Mirror, change: Change<'_>) {
    match change {
        Change::Set(key, value) => {
            if let Some(e) = mirror.iter_mut().find(|e| e.key.as_str() == key) {
                e.value = value;
            } else if let Ok(key) = Key::try_from(key) {
                // `encode` refused a key beyond `MAX_ENTRIES` before the commit.
                let _ = mirror.push(Entry { key, value });
            }
        }
        Change::Remove(key) => {
            if let Some(i) = mirror.iter().position(|e| e.key.as_str() == key) {
                mirror.remove(i);
            }
        }
    }
}

/// Reads the entries of a snapshot body front to back.
struct Reader<'b> {
    body: &'b [u8],
    pos: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, n: usize) -> Option<&'b [u8]> {
        let slice = self.body.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(slice)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn ascii(&mut self, n: usize) -> Option<&'b str> {
        let text = core::str::from_utf8(self.take(n)?).ok()?;
        text.is_ascii().then_some(text)
    }
}

/// Decodes one snapshot of exactly `len` bytes into `out`; `None` for any defect (format, length, CRC, entry).
fn decode(bytes: &[u8], out: &mut Mirror) -> Option<u32> {
    out.clear();
    if bytes.len() < HEADER_LEN + CRC_LEN || bytes[0] != FORMAT_V1 {
        return None;
    }
    if usize::from(u16::from_le_bytes([bytes[2], bytes[3]])) != bytes.len() {
        return None;
    }
    let (body, crc) = bytes.split_at(bytes.len() - CRC_LEN);
    if crc32(body) != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) {
        return None;
    }
    let count = usize::from(bytes[1]);
    let seq = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let mut r = Reader {
        body,
        pos: HEADER_LEN,
    };
    for _ in 0..count {
        let key_len = usize::from(r.byte()?);
        let key = r.ascii(key_len)?;
        if !valid_key(key) || out.iter().any(|e| e.key.as_str() == key) {
            return None;
        }
        let value = match r.byte()? {
            TAG_U8 => Value::U8(r.byte()?),
            TAG_U32 => {
                let b = r.take(4)?;
                Value::U32(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            }
            TAG_STR => {
                let n = usize::from(r.byte()?);
                if n > MAX_STR_LEN {
                    return None;
                }
                Value::Str(KvStr::try_from(r.ascii(n)?).ok()?)
            }
            _ => return None,
        };
        out.push(Entry {
            key: Key::try_from(key).ok()?,
            value,
        })
        .ok()?;
    }
    (r.pos == body.len()).then_some(seq)
}

/// [`OtaKv`] over a region of NOR flash; see the module documentation for the format and its power-loss rules.
///
/// The region is `total_bytes / F::ERASE_SIZE` whole sectors from `base_offset`, at least [`MIN_SECTORS`].
/// The store owns the region: nothing else may write it.
/// Reads take no flash access; a commit writes one snapshot and reads it back.
///
/// The backend is generic over `F`, so a platform hands it any `NorFlash` view (on esp-hal, a shared `FlashStorage` handle).
pub struct FlashKv<F: NorFlash> {
    flash: F,
    base: u32,
    sectors: u32,
    mirror: Mirror,
    /// Sector of the next append.
    cursor_sector: u32,
    /// Offset in that sector of the next append.
    cursor_pos: u32,
    /// Whether the cursor sector is erased from `cursor_pos` to its end.
    cursor_clean: bool,
    /// Sector of the newest valid snapshot; never erased.
    latest_sector: Option<u32>,
    /// Sequence number of the next snapshot; `None` once exhausted.
    next_seq: Option<u32>,
}

impl<F: NorFlash> core::fmt::Debug for FlashKv<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlashKv")
            .field("base", &self.base)
            .field("sectors", &self.sectors)
            .field("keys", &self.mirror.len())
            .finish_non_exhaustive()
    }
}

impl<F: NorFlash> FlashKv<F> {
    /// Read and write granularity of the snapshots.
    const ALIGN: usize = if F::READ_SIZE > F::WRITE_SIZE {
        F::READ_SIZE
    } else {
        F::WRITE_SIZE
    };

    const SECTOR: u32 = F::ERASE_SIZE as u32;

    /// Opens the store on `total_bytes` of `flash` from `base_offset` and loads the newest valid snapshot.
    ///
    /// A blank region, or one holding no valid snapshot (foreign data, a torn first write), opens as an empty store; its sectors are erased as the log reaches them.
    ///
    /// # Errors
    ///
    /// [`KvError::CODE_GEOMETRY`] when `base_offset` is not sector-aligned, the region holds fewer than [`MIN_SECTORS`] sectors or ends beyond the flash capacity, or the flash's granularity is unsupported (read or write size not a power of two up to 64, or a sector smaller than [`MAX_SNAPSHOT_BYTES`]); [`KvError::CODE_FLASH`] when a read fails.
    pub fn open(mut flash: F, base_offset: u32, total_bytes: u32) -> Result<Self, KvError> {
        let erase = F::ERASE_SIZE;
        if !Self::ALIGN.is_power_of_two()
            || Self::ALIGN > MAX_ALIGN
            || erase < MAX_SNAPSHOT_BYTES
            || !erase.is_multiple_of(MAX_ALIGN)
            || u32::try_from(erase).is_err()
        {
            return Err(geometry());
        }
        if !base_offset.is_multiple_of(Self::SECTOR) {
            return Err(geometry());
        }
        let sectors = total_bytes / Self::SECTOR;
        if sectors < MIN_SECTORS {
            return Err(geometry());
        }
        let end = sectors
            .checked_mul(Self::SECTOR)
            .and_then(|len| base_offset.checked_add(len))
            .ok_or_else(geometry)?;
        if end as usize > flash.capacity() {
            return Err(geometry());
        }

        let mut buf = [0u8; MAX_SNAPSHOT_BYTES];
        let mut scratch = Mirror::new();
        let mut mirror = Mirror::new();
        let mut best: Option<(u32, u32, u32)> = None;
        let header_len = round_up(HEADER_LEN, Self::ALIGN);
        for sector in 0..sectors {
            let start = base_offset + sector * Self::SECTOR;
            let mut pos = 0usize;
            while pos + header_len <= erase {
                let at = start + pos as u32;
                flash.read(at, &mut buf[..header_len]).map_err(flash_err)?;
                if buf[0] != FORMAT_V1 {
                    break;
                }
                let len = usize::from(u16::from_le_bytes([buf[2], buf[3]]));
                if !(HEADER_LEN + CRC_LEN..=MAX_SNAPSHOT_BYTES).contains(&len) {
                    break;
                }
                let padded = round_up(len, Self::ALIGN);
                if pos + padded > erase {
                    break;
                }
                flash.read(at, &mut buf[..padded]).map_err(flash_err)?;
                let Some(seq) = decode(&buf[..len], &mut scratch) else {
                    break;
                };
                pos += padded;
                if best.is_none_or(|(newest, _, _)| seq > newest) {
                    best = Some((seq, sector, pos as u32));
                    core::mem::swap(&mut mirror, &mut scratch);
                }
            }
        }

        let (cursor_sector, cursor_pos) = best.map_or((0, 0), |(_, sector, end)| (sector, end));
        let mut kv = Self {
            flash,
            base: base_offset,
            sectors,
            mirror,
            cursor_sector,
            cursor_pos,
            cursor_clean: false,
            latest_sector: best.map(|(_, sector, _)| sector),
            next_seq: best.map_or(Some(1), |(seq, _, _)| seq.checked_add(1)),
        };
        kv.cursor_clean = kv.is_erased(cursor_sector, cursor_pos)?;
        Ok(kv)
    }

    fn sector_offset(&self, sector: u32) -> u32 {
        self.base + sector * Self::SECTOR
    }

    /// Whether `sector` reads erased from `from` to its end.
    fn is_erased(&mut self, sector: u32, from: u32) -> Result<bool, KvError> {
        let mut chunk = [0u8; MAX_ALIGN];
        let start = self.sector_offset(sector);
        let mut pos = from;
        while pos < Self::SECTOR {
            // `pos` and the sector size are multiples of `ALIGN`, and `ALIGN` divides 64.
            let n = (Self::SECTOR - pos).min(MAX_ALIGN as u32) as usize;
            self.flash
                .read(start + pos, &mut chunk[..n])
                .map_err(flash_err)?;
            if chunk[..n].iter().any(|b| *b != 0xFF) {
                return Ok(false);
            }
            pos += n as u32;
        }
        Ok(true)
    }

    /// Whether `bytes` read back at `at`.
    fn verify(&mut self, at: u32, bytes: &[u8]) -> Result<(), KvError> {
        let mut chunk = [0u8; MAX_ALIGN];
        for (i, expected) in bytes.chunks(MAX_ALIGN).enumerate() {
            let got = &mut chunk[..expected.len()];
            self.flash
                .read(at + (i * MAX_ALIGN) as u32, got)
                .map_err(flash_err)?;
            if got != expected {
                return Err(KvError::new(KvError::CODE_FLASH));
            }
        }
        Ok(())
    }

    /// Appends the snapshot with `change` applied, then applies it to the mirror.
    fn commit(&mut self, change: Change<'_>) -> Result<(), KvError> {
        let seq = self.next_seq.ok_or_else(full)?;
        let mut buf = [0xFFu8; MAX_SNAPSHOT_BYTES];
        let len = encode(&self.mirror, &change, seq, &mut buf)?;
        // `MAX_SNAPSHOT_BYTES` is a multiple of 64, and `ALIGN` divides 64.
        let padded = round_up(len, Self::ALIGN);
        let fits = self.cursor_pos as usize + padded <= F::ERASE_SIZE;
        if !(self.cursor_clean && fits) {
            // Never the sector of the newest valid snapshot: with two sectors after a failed write
            // in the fresh one, that fresh sector is erased again.
            let mut target = (self.cursor_sector + 1) % self.sectors;
            if Some(target) == self.latest_sector {
                target = self.cursor_sector;
            }
            let from = self.sector_offset(target);
            self.flash
                .erase(from, from + Self::SECTOR)
                .map_err(flash_err)?;
            self.cursor_sector = target;
            self.cursor_pos = 0;
            self.cursor_clean = true;
        }
        // A number is used once even if its write fails, so a write that landed despite an error
        // never ties with the next snapshot.
        self.next_seq = seq.checked_add(1);
        let at = self.sector_offset(self.cursor_sector) + self.cursor_pos;
        let bytes = &buf[..padded];
        let written = self
            .flash
            .write(at, bytes)
            .map_err(flash_err)
            .and_then(|()| self.verify(at, bytes));
        if let Err(e) = written {
            self.cursor_clean = false;
            return Err(e);
        }
        self.cursor_pos += padded as u32;
        self.latest_sector = Some(self.cursor_sector);
        apply(&mut self.mirror, change);
        Ok(())
    }

    fn find(&self, key: &str) -> Option<&Value> {
        self.mirror
            .iter()
            .find(|e| e.key.as_str() == key)
            .map(|e| &e.value)
    }

    fn set(&mut self, key: &str, value: Value) -> Result<(), KvError> {
        if !valid_key(key) {
            return Err(invalid_value());
        }
        if self.find(key) == Some(&value) {
            return Ok(());
        }
        self.commit(Change::Set(key, value))
    }
}

impl<F: NorFlash> OtaKv for FlashKv<F> {
    fn get_u8(&self, key: &str) -> Result<Option<u8>, KvError> {
        match self.find(key) {
            None => Ok(None),
            Some(Value::U8(v)) => Ok(Some(*v)),
            Some(_) => Err(type_mismatch()),
        }
    }

    fn get_u32(&self, key: &str) -> Result<Option<u32>, KvError> {
        match self.find(key) {
            None => Ok(None),
            Some(Value::U32(v)) => Ok(Some(*v)),
            Some(_) => Err(type_mismatch()),
        }
    }

    fn get_str(&self, key: &str) -> Result<Option<KvStr>, KvError> {
        match self.find(key) {
            None => Ok(None),
            Some(Value::Str(v)) => Ok(Some(v.clone())),
            Some(_) => Err(type_mismatch()),
        }
    }

    fn set_u8(&mut self, key: &str, value: u8) -> Result<(), KvError> {
        self.set(key, Value::U8(value))
    }

    fn set_u32(&mut self, key: &str, value: u32) -> Result<(), KvError> {
        self.set(key, Value::U32(value))
    }

    fn set_str(&mut self, key: &str, value: &str) -> Result<(), KvError> {
        if !value.is_ascii() {
            return Err(invalid_value());
        }
        let value = KvStr::try_from(value).map_err(|_| invalid_value())?;
        self.set(key, Value::Str(value))
    }

    fn remove(&mut self, key: &str) -> Result<(), KvError> {
        if self.find(key).is_none() {
            return Ok(());
        }
        self.commit(Change::Remove(key))
    }
}
