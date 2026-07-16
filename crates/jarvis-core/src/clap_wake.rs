//! Clap-to-wake: detect three claps as an alternative wake trigger, running on
//! the same 80 ms audio chunks as the wake-word model.
//!
//! A clap is a sharp broadband ATTACK. Claps ring/reverb, so the sound never
//! drops to silence between them — detecting "loud then quiet" fails. Instead
//! we do onset detection at ~8 ms sub-frame resolution: an onset is a sub-frame
//! whose peak both exceeds an absolute floor AND jumps sharply above the
//! previous sub-frame, with a short refractory so one clap's ring isn't counted
//! twice. Three onsets inside a ~2 s window fire the wake. Params were tuned
//! against a real recording of the user's claps (peak ~25000, onsets ~150 ms
//! apart).

/// Samples per sub-frame (~8 ms at 16 kHz) — fine enough to separate fast claps.
const SUB: usize = 128;
/// Peak (int16 units) a clap onset must exceed outright.
const ABS_THRESHOLD: f32 = 4000.0;
/// …and it must jump this multiple above the previous sub-frame (a sharp attack).
const RISE: f32 = 1.5;
/// Minimum sub-frames between two counted onsets (~110 ms) — debounces the ring.
const REFRACTORY_SF: u64 = 14;
/// All three onsets must land inside this many sub-frames (~2 s).
const WINDOW_SF: u64 = 250;
const NEEDED: usize = 3;

pub struct ClapDetector {
    t: u64,          // running sub-frame counter
    prev: f32,       // previous sub-frame peak
    last_onset: u64,
    onsets: Vec<u64>,
}

impl Default for ClapDetector {
    fn default() -> Self {
        // Start the counter high so the first onset isn't blocked by refractory.
        Self { t: WINDOW_SF + 1, prev: 0.0, last_onset: 0, onsets: Vec::new() }
    }
}

impl ClapDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk (16 kHz mono, int16-range f32). Returns true the moment a
    /// third clap completes the pattern.
    pub fn feed(&mut self, chunk: &[f32]) -> bool {
        let mut fired = false;
        for sf in chunk.chunks(SUB) {
            let peak = sf.iter().fold(0f32, |m, &s| m.max(s.abs()));
            let is_onset = peak > ABS_THRESHOLD
                && peak > self.prev * RISE
                && self.t.saturating_sub(self.last_onset) >= REFRACTORY_SF;
            if is_onset {
                self.last_onset = self.t;
                let now = self.t;
                self.onsets.retain(|&o| now - o < WINDOW_SF);
                self.onsets.push(now);
                if self.onsets.len() >= NEEDED {
                    self.onsets.clear();
                    fired = true;
                }
            }
            self.prev = peak;
            self.t += 1;
        }
        fired
    }

    pub fn reset(&mut self) {
        self.onsets.clear();
        self.last_onset = self.t;
    }
}
