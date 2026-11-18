//! Clock-sweep buffer pool for fixed-size pages.
//!
//! A database does not read bytes. It pins pages, marks them dirty, and
//! hopes the eviction policy does not throw away the wrong one. This crate
//! is that policy, and nothing else: no WAL, no filesystem, no locks.
//!
//! The directory is a scan of frame headers. A miss is dominated by the
//! `load` callback, not by looking at a few thousand headers, and eviction
//! cannot desynchronise a side index that does not exist.
//!
//! # Rules
//!
//! * The pool is single-threaded. Share it behind a lock if you must.
//! * `load` and `flush` must not call back into the same pool.
//! * [`Pool::bytes_mut`] marks the frame dirty. A later eviction of that
//!   frame calls `flush` before the image is replaced. If `flush` returns
//!   an error, the resident page is left in place.
//! * A failed `load` does not evict anything. A page that was flushed first
//!   stays resident and clean.
//!
//! # Example
//!
//! ```
//! use pageclock::Pool;
//!
//! let mut pool: Pool<64, 2> = Pool::new();
//! let frame = pool
//!     .pin(
//!         7,
//!         |dst| {
//!             dst[..4].copy_from_slice(&7u32.to_le_bytes());
//!             Ok(())
//!         },
//!         |_id, _bytes| Ok(()),
//!     )
//!     .unwrap();
//! assert_eq!(&pool.bytes(frame).unwrap()[..4], &7u32.to_le_bytes());
//! ```

#![no_std]

mod crc;

pub use crc::crc32;

/// Caller-chosen page identity. The pool does not interpret it.
pub type PageId = u64;

/// Why a pool operation refused to change state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Every frame is pinned, so the clock cannot take one.
    Full,
    /// The frame index is out of range, or the frame holds no page.
    BadFrame,
    /// The frame is not pinned.
    NotPinned,
    /// The CRC-32 trailer does not match the page image.
    Checksum,
    /// `load` rejected the page. No frame was replaced.
    Load,
    /// `flush` rejected a dirty page. That page is still resident.
    Flush,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Full => "every frame is pinned",
            Error::BadFrame => "frame is empty or out of range",
            Error::NotPinned => "frame is not pinned",
            Error::Checksum => "page checksum mismatch",
            Error::Load => "page load failed",
            Error::Flush => "dirty page flush failed",
        })
    }
}

/// Counters since [`Pool::new`]. They are exact on this single thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `pin` found the page already resident.
    pub hits: u64,
    /// `pin` installed a page that was not resident.
    pub misses: u64,
    /// A resident page was replaced by a different page.
    pub evictions: u64,
    /// A dirty page was handed to `flush` and `flush` succeeded.
    pub flushes: u64,
}

#[derive(Clone, Copy)]
struct Frame<const PAGE: usize> {
    page: Option<PageId>,
    pins: u32,
    dirty: bool,
    referenced: bool,
    bytes: [u8; PAGE],
}

impl<const PAGE: usize> Frame<PAGE> {
    const fn empty() -> Self {
        Self {
            page: None,
            pins: 0,
            dirty: false,
            referenced: false,
            bytes: [0; PAGE],
        }
    }
}

/// Fixed table of `FRAMES` pages, each `PAGE` bytes.
///
/// `PAGE` must be at least 8. `FRAMES` must be at least 1.
/// [`Pool::new`] panics if either constant is outside that range: a buffer
/// pool of zero frames, or a page too small for a 4-byte trailer plus a
/// payload, is a programming error rather than a runtime one.
pub struct Pool<const PAGE: usize, const FRAMES: usize> {
    frames: [Frame<PAGE>; FRAMES],
    scratch: [u8; PAGE],
    hand: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    flushes: u64,
}

impl<const PAGE: usize, const FRAMES: usize> Pool<PAGE, FRAMES> {
    /// Empty pool. The clock hand starts on frame 0.
    ///
    /// # Panics
    ///
    /// Panics if `FRAMES == 0` or `PAGE < 8`.
    pub fn new() -> Self {
        assert!(FRAMES > 0, "pageclock needs at least one frame");
        assert!(PAGE >= 8, "pageclock pages must be at least 8 bytes");
        Self {
            frames: [Frame::empty(); FRAMES],
            scratch: [0; PAGE],
            hand: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            flushes: 0,
        }
    }

    pub const fn page_size(&self) -> usize {
        PAGE
    }

    pub const fn frame_count(&self) -> usize {
        FRAMES
    }

    pub fn stats(&self) -> Stats {
        Stats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            flushes: self.flushes,
        }
    }

    /// How many frames currently hold a page, pinned or not.
    pub fn resident_count(&self) -> usize {
        self.frames.iter().filter(|frame| frame.page.is_some()).count()
    }

    /// Pin `id`, loading it if it is not resident.
    ///
    /// On a hit the pin count goes up and the reference bit is set, which
    /// gives the page a second chance against the clock.
    ///
    /// On a miss the clock first looks for a free frame, then walks frames
    /// from the hand. A pinned frame is skipped. A referenced unpinned frame
    /// loses its reference bit and is skipped once. The first unpinned,
    /// unreferenced frame is the victim.
    ///
    /// `load` fills the scratch image. It is called only after a dirty victim
    /// has been flushed successfully, and the victim is replaced only if
    /// `load` returns `Ok(())`.
    pub fn pin<L, F>(&mut self, id: PageId, load: L, mut flush: F) -> Result<usize, Error>
    where
        L: FnOnce(&mut [u8]) -> Result<(), ()>,
        F: FnMut(PageId, &[u8]) -> Result<(), ()>,
    {
        if let Some(index) = self.find(id) {
            let frame = &mut self.frames[index];
            frame.pins = frame.pins.saturating_add(1);
            frame.referenced = true;
            self.hits += 1;
            return Ok(index);
        }

        let index = self.select_victim()?;
        if let Some(old_id) = self.frames[index].page {
            if self.frames[index].dirty {
                if flush(old_id, &self.frames[index].bytes).is_err() {
                    return Err(Error::Flush);
                }
                self.frames[index].dirty = false;
                self.flushes += 1;
            }
        }

        self.scratch = [0; PAGE];
        if load(&mut self.scratch).is_err() {
            return Err(Error::Load);
        }

        let replaced = self.frames[index].page;
        self.frames[index].bytes = self.scratch;
        self.frames[index].page = Some(id);
        self.frames[index].pins = 1;
        self.frames[index].dirty = false;
        self.frames[index].referenced = true;
        if replaced.is_some() {
            self.evictions += 1;
        }
        self.misses += 1;
        self.hand = (index + 1) % FRAMES;
        Ok(index)
    }

    /// Drop one pin. The page stays resident until the clock reuses the frame.
    pub fn unpin(&mut self, frame: usize) -> Result<(), Error> {
        let frame = self.frames.get_mut(frame).ok_or(Error::BadFrame)?;
        if frame.page.is_none() || frame.pins == 0 {
            return Err(Error::NotPinned);
        }
        frame.pins -= 1;
        Ok(())
    }

    /// Mark a pinned frame dirty without writing its bytes.
    pub fn mark_dirty(&mut self, frame: usize) -> Result<(), Error> {
        let frame = self.frame_mut_pinned(frame)?;
        frame.dirty = true;
        Ok(())
    }

    pub fn is_dirty(&self, frame: usize) -> Result<bool, Error> {
        Ok(self.frame(frame)?.dirty)
    }

    pub fn pins(&self, frame: usize) -> Result<u32, Error> {
        Ok(self.frame(frame)?.pins)
    }

    pub fn page_id(&self, frame: usize) -> Result<PageId, Error> {
        self.frame(frame)?.page.ok_or(Error::BadFrame)
    }

    /// Borrow a resident page. The frame does not have to be pinned.
    pub fn bytes(&self, frame: usize) -> Result<&[u8], Error> {
        Ok(&self.frame(frame)?.bytes)
    }

    /// Borrow a pinned page for writing and mark it dirty.
    pub fn bytes_mut(&mut self, frame: usize) -> Result<&mut [u8], Error> {
        let frame = self.frame_mut_pinned(frame)?;
        frame.dirty = true;
        frame.referenced = true;
        Ok(&mut frame.bytes)
    }

    /// Write a CRC-32 of `bytes[..PAGE - 4]` into the last four bytes.
    ///
    /// The frame must be pinned. Sealing marks the page dirty.
    pub fn seal(&mut self, frame: usize) -> Result<(), Error> {
        let frame = self.frame_mut_pinned(frame)?;
        let digest = crc32(&frame.bytes[..PAGE - 4]);
        frame.bytes[PAGE - 4..].copy_from_slice(&digest.to_le_bytes());
        frame.dirty = true;
        Ok(())
    }

    /// Check the CRC-32 trailer of a resident frame.
    pub fn verify(&self, frame: usize) -> Result<(), Error> {
        let bytes = &self.frame(frame)?.bytes;
        let digest = crc32(&bytes[..PAGE - 4]);
        let stored = u32::from_le_bytes([
            bytes[PAGE - 4],
            bytes[PAGE - 3],
            bytes[PAGE - 2],
            bytes[PAGE - 1],
        ]);
        if digest == stored {
            Ok(())
        } else {
            Err(Error::Checksum)
        }
    }

    fn find(&self, id: PageId) -> Option<usize> {
        self.frames.iter().position(|frame| frame.page == Some(id))
    }

    fn frame(&self, index: usize) -> Result<&Frame<PAGE>, Error> {
        let frame = self.frames.get(index).ok_or(Error::BadFrame)?;
        if frame.page.is_none() {
            return Err(Error::BadFrame);
        }
        Ok(frame)
    }

    fn frame_mut_pinned(&mut self, index: usize) -> Result<&mut Frame<PAGE>, Error> {
        let frame = self.frames.get_mut(index).ok_or(Error::BadFrame)?;
        if frame.page.is_none() {
            return Err(Error::BadFrame);
        }
        if frame.pins == 0 {
            return Err(Error::NotPinned);
        }
        Ok(frame)
    }

    fn select_victim(&mut self) -> Result<usize, Error> {
        for step in 0..FRAMES {
            let index = (self.hand + step) % FRAMES;
            if self.frames[index].page.is_none() {
                return Ok(index);
            }
        }
        for _ in 0..(FRAMES * 2) {
            let index = self.hand;
            self.hand = (self.hand + 1) % FRAMES;
            let frame = &mut self.frames[index];
            if frame.pins > 0 {
                continue;
            }
            if frame.referenced {
                frame.referenced = false;
                continue;
            }
            return Ok(index);
        }
        Err(Error::Full)
    }
}

impl<const PAGE: usize, const FRAMES: usize> Default for Pool<PAGE, FRAMES> {
    fn default() -> Self {
        Self::new()
    }
}
