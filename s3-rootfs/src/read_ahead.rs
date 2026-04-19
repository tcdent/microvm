//! Dynamic read-ahead (research/14 §4).
//!
//! Per-fd streak detection: sequential reads grow the prefetch window
//! (doubling, capped at MAX_WINDOW); random access shrinks it (halving,
//! floored at MIN_WINDOW). The ruleset's `ReadAhead` mode modifies
//! the starting window and growth rate but doesn't change the core
//! detection algorithm.
//!
//! Replaces the static `read_ahead_size` config knob. The 128 MB
//! default from research/13 becomes the MAX_WINDOW ceiling — dynamic
//! detection reaches it naturally on sequential reads and stays small
//! on random-access files.

use crate::ruleset::ReadAhead;

/// Minimum read-ahead window. Even after random-access shrinking,
/// we never drop below this — below 64 KB the overhead of per-GET
/// round-trips dominates.
pub const MIN_WINDOW: usize = 64 * 1024;

/// Maximum read-ahead window. Sequential reads can grow up to this
/// but no further. Sized so a 99 MB binary is fetched in 1–2 GETs.
pub const MAX_WINDOW: usize = 256 * 1024 * 1024;

/// Starting window for `ReadAhead::Auto`. Small enough to not waste
/// bandwidth on random-access files that happen to do 2–3 sequential
/// reads before seeking, large enough to avoid startup penalty on
/// truly sequential workloads.
pub const AUTO_INITIAL_WINDOW: usize = 128 * 1024; // 128 KB

/// Starting window for `ReadAhead::Aggressive`. Archives, binaries,
/// streaming reads — skip the warmup, start large.
const AGGRESSIVE_INITIAL_WINDOW: usize = 16 * 1024 * 1024; // 16 MB

/// Per-handle read-ahead state. Created at open() time, updated on
/// every read(), consulted by resolve_read to decide whether and how
/// much to prefetch.
#[derive(Debug)]
pub struct ReadAheadState {
    /// Mode from the ruleset. `Off` disables all read-ahead for this
    /// handle — every read fetches exactly the requested range.
    pub mode: ReadAhead,
    /// End of the last RA-served read: `Some(offset + size)`. `None`
    /// before the first observation. Using `Option` avoids the edge
    /// case where a read that ends at offset 0 would be misdetected
    /// as "no prior read".
    pub last_end: Option<u64>,
    /// Number of consecutive sequential reads. Resets to 0 on any
    /// non-sequential access. Window growth requires streak >= 2
    /// to dampen noise from accidental 2-read sequences.
    pub streak: u32,
    /// Current prefetch window in bytes. Grows on sequential reads,
    /// shrinks on random access.
    pub window: usize,
    /// Prefetched data, if any. Covers [chunk_offset .. chunk_offset + chunk.len()).
    pub chunk: Option<Vec<u8>>,
    /// Start offset of the prefetched chunk.
    pub chunk_offset: u64,
}

impl ReadAheadState {
    /// Create initial state for the given readahead mode.
    pub fn new(mode: ReadAhead) -> Self {
        let window = match mode {
            ReadAhead::Off => 0,
            ReadAhead::Auto => AUTO_INITIAL_WINDOW,
            ReadAhead::Aggressive => AGGRESSIVE_INITIAL_WINDOW,
        };
        Self {
            mode,
            last_end: None,
            streak: 0,
            window,
            chunk: None,
            chunk_offset: 0,
        }
    }

    /// Update streak detection after a read served via the RA chunk
    /// or a fresh S3 GET (steps 2 + 6 in resolve_read). Called AFTER
    /// the read is served. Must NOT be called for reads served from
    /// handle buffer, inflight map, read cache, or golden blob cache
    /// — those don't exercise the RA path and would fake sequential
    /// detection.
    pub fn observe(&mut self, offset: u64, size: u32) {
        if self.mode == ReadAhead::Off {
            return;
        }

        if self.last_end == Some(offset) {
            // Sequential continuation.
            self.streak = self.streak.saturating_add(1);
            // Gate: require streak >= 2 before growing. Dampens
            // noise from accidental 2-read sequences that happen
            // to be contiguous.
            if self.streak >= 2 {
                self.grow();
            }
        } else if self.last_end.is_none() {
            // First observation — don't penalize.
            self.streak = 1;
        } else {
            // Non-sequential: shrink window, reset streak.
            self.streak = 0;
            self.shrink();
            // Invalidate the chunk — it's at the wrong offset.
            self.chunk = None;
        }

        self.last_end = Some(offset + size as u64);
    }

    /// Try to serve a read from the prefetched chunk. Returns the
    /// slice if the requested range [offset..offset+size) is fully
    /// contained in the chunk, None otherwise.
    pub fn try_serve(&self, offset: u64, size: u32) -> Option<&[u8]> {
        let chunk = self.chunk.as_ref()?;
        let chunk_end = self.chunk_offset + chunk.len() as u64;
        let read_end = offset + size as u64;
        if offset >= self.chunk_offset && read_end <= chunk_end {
            let start = (offset - self.chunk_offset) as usize;
            let end = start + size as usize;
            Some(&chunk[start..end])
        } else {
            None
        }
    }

    /// Whether read-ahead is active for this handle.
    pub fn is_active(&self) -> bool {
        self.mode != ReadAhead::Off && self.window > 0
    }

    /// The window to use for the next prefetch GET.
    pub fn current_window(&self) -> usize {
        self.window
    }

    fn grow(&mut self) {
        let factor = match self.mode {
            ReadAhead::Aggressive => 4, // grow faster
            _ => 2,
        };
        self.window = (self.window * factor).min(MAX_WINDOW);
    }

    fn shrink(&mut self) {
        self.window = (self.window / 2).max(MIN_WINDOW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_mode_stays_inactive() {
        let mut s = ReadAheadState::new(ReadAhead::Off);
        assert!(!s.is_active());
        s.observe(0, 4096);
        assert_eq!(s.streak, 0);
        assert_eq!(s.window, 0);
    }

    #[test]
    fn auto_sequential_grows_window() {
        let mut s = ReadAheadState::new(ReadAhead::Auto);
        assert_eq!(s.window, AUTO_INITIAL_WINDOW);

        // First read at offset 0.
        s.observe(0, 4096);
        assert_eq!(s.streak, 1);

        // Sequential continuation.
        s.observe(4096, 4096);
        assert_eq!(s.streak, 2);
        assert_eq!(s.window, AUTO_INITIAL_WINDOW * 2); // doubled

        s.observe(8192, 4096);
        assert_eq!(s.streak, 3);
        assert_eq!(s.window, AUTO_INITIAL_WINDOW * 4);
    }

    #[test]
    fn random_access_shrinks_window() {
        let mut s = ReadAheadState::new(ReadAhead::Auto);

        // Build up a streak.
        s.observe(0, 4096);
        s.observe(4096, 4096);
        s.observe(8192, 4096);
        let grown = s.window;
        assert!(grown > AUTO_INITIAL_WINDOW);

        // Random seek.
        s.observe(1_000_000, 4096);
        assert_eq!(s.streak, 0);
        assert_eq!(s.window, grown / 2);
    }

    #[test]
    fn window_capped_at_max() {
        let mut s = ReadAheadState::new(ReadAhead::Aggressive);
        let mut offset = 0u64;
        for _ in 0..100 {
            s.observe(offset, 4096);
            offset += 4096;
        }
        assert!(s.window <= MAX_WINDOW);
    }

    #[test]
    fn window_floored_at_min() {
        let mut s = ReadAheadState::new(ReadAhead::Auto);
        // Many random seeks.
        for i in 0..50 {
            s.observe(i * 1_000_000, 4096);
        }
        assert!(s.window >= MIN_WINDOW);
    }

    #[test]
    fn aggressive_starts_larger() {
        let auto = ReadAheadState::new(ReadAhead::Auto);
        let aggr = ReadAheadState::new(ReadAhead::Aggressive);
        assert!(aggr.window > auto.window);
    }

    #[test]
    fn try_serve_hit_and_miss() {
        let mut s = ReadAheadState::new(ReadAhead::Auto);
        s.chunk = Some(vec![0u8; 8192]);
        s.chunk_offset = 1000;

        // Hit: fully within chunk.
        assert!(s.try_serve(1000, 100).is_some());
        assert!(s.try_serve(5000, 3000).is_some());

        // Miss: starts before chunk.
        assert!(s.try_serve(500, 1000).is_none());

        // Miss: extends past chunk.
        assert!(s.try_serve(8000, 2000).is_none());
    }
}
