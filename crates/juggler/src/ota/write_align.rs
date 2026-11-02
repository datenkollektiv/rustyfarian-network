//! Word-aligned flash writes from an arbitrarily chunked byte stream.
//!
//! A NOR flash writer that does not erase (`esp-storage`'s `write_nor`, reached
//! through `FlashRegion::as_nor_flash()`) requires the offset **and** the length
//! of every write to be a multiple of its write size — 4 bytes on ESP32 flash.
//! A socket read hands out 1..N bytes, so a stream written chunk by chunk must
//! hold back the remainder of a partial word; otherwise the offset drifts off
//! the alignment after the first short read, the driver rejects the write, and
//! a caller falls back to the read-modify-write `write`, which erases the
//! sector per call (bug 004).
//!
//! [`ChunkAligner`] performs that bookkeeping and nothing else: it calls back
//! with aligned, contiguous slices, in order, and never allocates. The driver
//! call stays with the tier.
//!
//! ```
//! use juggler::ota::ChunkAligner;
//!
//! let mut aligner = ChunkAligner::new(4);
//! let mut seen = [(0u32, 0usize); 2];
//! let mut written = 0;
//! aligner
//!     .push(0, &[1, 2, 3, 4, 5], |offset, data| {
//!         seen[written] = (offset, data.len());
//!         written += 1;
//!         Ok::<(), ()>(())
//!     })
//!     .unwrap();
//! aligner
//!     .finish(5, |offset, data| {
//!         seen[written] = (offset, data.len());
//!         written += 1;
//!         Ok::<(), ()>(())
//!     })
//!     .unwrap();
//! assert_eq!(&seen[..written], &[(0, 4), (4, 4)]);
//! ```

/// Largest supported write size.
pub const MAX_WRITE_SIZE: usize = 8;

/// Turns a chunked byte stream into aligned flash writes.
///
/// The caller pushes each received chunk with its stream offset, then calls
/// [`finish`](Self::finish) once the whole stream arrived. The callback is
/// invoked only with slices whose offset and length are multiples of the write
/// size, in increasing offset order, with no gaps.
#[derive(Debug)]
pub struct ChunkAligner {
    write_size: usize,
    /// Held-back bytes of a partial word; aligned so the driver writes from it
    /// directly instead of copying through its own sector buffer.
    carry: Carry,
    carry_len: usize,
    /// Stream offset of `carry[0]`, and of the next byte when the carry is empty.
    carry_offset: u32,
}

/// The held-back partial word, aligned to the largest supported write size.
#[repr(align(8))]
#[derive(Debug)]
struct Carry([u8; MAX_WRITE_SIZE]);

impl ChunkAligner {
    /// An aligner for a driver whose write size is `write_size` bytes.
    ///
    /// # Panics
    ///
    /// If `write_size` is not a power of two in `1..=MAX_WRITE_SIZE`; that is a
    /// programming error, not a runtime condition.
    pub const fn new(write_size: usize) -> Self {
        assert!(
            write_size.is_power_of_two() && write_size <= MAX_WRITE_SIZE,
            "write size must be a power of two up to MAX_WRITE_SIZE"
        );
        Self {
            write_size,
            carry: Carry([0; MAX_WRITE_SIZE]),
            carry_len: 0,
            carry_offset: 0,
        }
    }

    /// The write size this aligner was built for.
    pub const fn write_size(&self) -> usize {
        self.write_size
    }

    /// Feeds the stream bytes that start at `offset`.
    ///
    /// `offset` must continue the stream exactly: a gap or an overlap would
    /// leave a hole in the image and is reported as
    /// [`AlignError::OutOfOrder`].
    pub fn push<E>(
        &mut self,
        offset: u32,
        bytes: &[u8],
        mut write: impl FnMut(u32, &[u8]) -> Result<(), E>,
    ) -> Result<(), AlignError<E>> {
        if offset != self.carry_offset + self.carry_len as u32 {
            return Err(AlignError::OutOfOrder);
        }

        let mut rest = bytes;
        if self.carry_len > 0 {
            // Complete the held-back word first, then hand it over as one
            // aligned slice of exactly `write_size` bytes.
            let take = (self.write_size - self.carry_len).min(rest.len());
            self.carry.0[self.carry_len..self.carry_len + take].copy_from_slice(&rest[..take]);
            self.carry_len += take;
            rest = &rest[take..];
            if self.carry_len < self.write_size {
                return Ok(());
            }
            write(self.carry_offset, &self.carry.0[..self.write_size])
                .map_err(AlignError::Write)?;
            self.carry_offset += self.write_size as u32;
            self.carry_len = 0;
        }

        // Whole words go out straight from the caller's buffer.
        let whole = rest.len() - rest.len() % self.write_size;
        if whole > 0 {
            write(self.carry_offset, &rest[..whole]).map_err(AlignError::Write)?;
            self.carry_offset += whole as u32;
            rest = &rest[whole..];
        }

        // 0..write_size-1 bytes wait for the next chunk.
        if !rest.is_empty() {
            self.carry.0[..rest.len()].copy_from_slice(rest);
            self.carry_len = rest.len();
        }
        Ok(())
    }

    /// Writes the held-back tail, padded with the erased value `0xFF`.
    ///
    /// `end` is the stream length; the padding never extends the image, and the
    /// caller's erased span must cover it.
    pub fn finish<E>(
        &mut self,
        end: u32,
        mut write: impl FnMut(u32, &[u8]) -> Result<(), E>,
    ) -> Result<(), AlignError<E>> {
        if self.carry_len == 0 {
            return Ok(());
        }
        if end != self.carry_offset + self.carry_len as u32 {
            return Err(AlignError::OutOfOrder);
        }
        self.carry.0[self.carry_len..self.write_size].fill(0xFF);
        write(self.carry_offset, &self.carry.0[..self.write_size]).map_err(AlignError::Write)?;
        self.carry_offset += self.write_size as u32;
        self.carry_len = 0;
        Ok(())
    }
}

/// Why a push or the final flush did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignError<E> {
    /// The offset does not continue the stream.
    OutOfOrder,
    /// The driver refused a write.
    Write(E),
}

/// Rounds `len` up to the next multiple of `align`.
///
/// `align` must be a power of two; a `len` that already is a multiple is
/// returned unchanged.
pub const fn round_up(len: u32, align: u32) -> u32 {
    len.div_ceil(align) * align
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Writes collected from a chunked push, with the alignment rule checked as
    /// they arrive (`align` is the aligner's write size).
    fn collect(
        aligner: &mut ChunkAligner,
        chunks: &[&[u8]],
        end: u32,
    ) -> (Vec<(u32, Vec<u8>)>, u32) {
        let align = aligner.write_size();
        let mut writes = Vec::new();
        let mut offset = 0u32;
        let mut record = |at: u32, data: &[u8]| {
            assert_eq!(at as usize % align, 0, "unaligned write offset {at}");
            assert_eq!(
                data.len() % align,
                0,
                "unaligned write length {}",
                data.len()
            );
            writes.push((at, data.to_vec()));
            Ok::<(), ()>(())
        };
        for chunk in chunks {
            aligner.push(offset, chunk, &mut record).unwrap();
            offset += chunk.len() as u32;
        }
        aligner.finish(end, &mut record).unwrap();
        (writes, offset)
    }

    /// The stream, reassembled from the writes, with padding trimmed.
    fn reassembled(writes: &[(u32, Vec<u8>)], len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for (at, data) in writes {
            assert_eq!(*at as usize, out.len(), "writes must be contiguous");
            out.extend_from_slice(data);
        }
        out.truncate(len);
        out
    }

    #[test]
    fn a_chunked_stream_is_written_aligned_and_in_order() {
        let stream: Vec<u8> = (0..64u8).collect();
        for chunk in [1usize, 2, 3, 5, 7, 8, 13, 64] {
            let mut aligner = ChunkAligner::new(4);
            let chunks: Vec<&[u8]> = stream.chunks(chunk).collect();
            let (writes, end) = collect(&mut aligner, &chunks, stream.len() as u32);
            assert_eq!(reassembled(&writes, stream.len()), stream, "chunk {chunk}");
            assert_eq!(end, stream.len() as u32);
        }
    }

    #[test]
    fn an_aligned_stream_is_written_without_padding() {
        let mut aligner = ChunkAligner::new(4);
        let (writes, _) = collect(&mut aligner, &[&[1u8, 2, 3, 4][..], &[5, 6, 7, 8][..]], 8);
        assert_eq!(writes.len(), 2, "one write per chunk, no extra flush");
        assert_eq!(
            reassembled(&writes, 8),
            [1, 2, 3, 4, 5, 6, 7, 8],
            "no padding when the stream ends on a word"
        );
    }

    #[test]
    fn the_final_partial_word_is_padded_with_the_erased_value() {
        let mut aligner = ChunkAligner::new(4);
        let (writes, _) = collect(&mut aligner, &[&[0xAA, 0xBB, 0xCC][..]], 3);
        assert_eq!(writes, [(0, vec![0xAA, 0xBB, 0xCC, 0xFF])]);
    }

    #[test]
    fn a_single_byte_at_a_time_still_yields_aligned_writes() {
        let mut aligner = ChunkAligner::new(4);
        let chunks: Vec<Vec<u8>> = (0..7u8).map(|b| vec![b]).collect();
        let refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
        let (writes, _) = collect(&mut aligner, &refs, 7);
        assert_eq!(writes.len(), 2, "7 bytes span two words");
        assert_eq!(reassembled(&writes, 7), (0..7u8).collect::<Vec<_>>());
    }

    #[test]
    fn an_empty_stream_writes_nothing() {
        let mut aligner = ChunkAligner::new(4);
        let (writes, _) = collect(&mut aligner, &[], 0);
        assert!(writes.is_empty());
    }

    #[test]
    fn a_gap_in_the_stream_is_rejected() {
        let mut aligner = ChunkAligner::new(4);
        aligner
            .push(0, &[1, 2, 3], |_, _| Ok::<(), ()>(()))
            .unwrap();
        assert_eq!(
            aligner.push(4, &[4], |_, _| Ok::<(), ()>(())),
            Err(AlignError::OutOfOrder),
            "a skipped byte would leave a hole in the image"
        );
    }

    #[test]
    fn a_finish_at_the_wrong_length_is_rejected() {
        let mut aligner = ChunkAligner::new(4);
        aligner.push(0, &[1, 2], |_, _| Ok::<(), ()>(())).unwrap();
        assert_eq!(
            aligner.finish(5, |_, _| Ok::<(), ()>(())),
            Err(AlignError::OutOfOrder)
        );
    }

    #[test]
    fn a_driver_error_stops_the_stream() {
        let mut aligner = ChunkAligner::new(4);
        assert_eq!(
            aligner.push(0, &[1, 2, 3, 4], |_, _| Err::<(), &str>("flash")),
            Err(AlignError::Write("flash"))
        );
    }

    #[test]
    fn a_larger_write_size_works_the_same_way() {
        let mut aligner = ChunkAligner::new(8);
        let (writes, _) = collect(&mut aligner, &[&[1, 2, 3][..], &[4, 5][..]], 5);
        assert_eq!(writes, [(0, vec![1, 2, 3, 4, 5, 0xFF, 0xFF, 0xFF])]);
    }

    #[test]
    fn the_erased_span_rounds_up_to_whole_sectors() {
        assert_eq!(round_up(0, 4096), 0);
        assert_eq!(round_up(1, 4096), 4096);
        assert_eq!(round_up(4096, 4096), 4096);
        assert_eq!(round_up(4097, 4096), 8192);
        assert_eq!(round_up(1_048_576, 4096), 1_048_576);
    }
}
