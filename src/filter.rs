//! Chatter detection logic, independent from evdev so it can be unit tested.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

/// Kernel `EV_KEY` values.
pub const KEY_RELEASE: i32 = 0;
pub const KEY_PRESS: i32 = 1;
pub const KEY_REPEAT: i32 = 2;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// Blocked press; holds the time elapsed since the previous release.
    Chatter(Duration),
    /// Event belonging to an already blocked press (repeat or release).
    Swallow,
}

/// Blocks a press occurring less than `threshold` after the previous release of the
/// same key, along with the repeats and the release that follow it.
#[derive(Default)]
pub struct ChatterFilter {
    last_release: HashMap<u16, SystemTime>,
    blocked: HashSet<u16>,
}

impl ChatterFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn process(&mut self, code: u16, value: i32, time: SystemTime, threshold: Duration) -> Verdict {
        match value {
            KEY_PRESS => {
                if !threshold.is_zero()
                    && let Some(last) = self.last_release.get(&code)
                    && let Ok(elapsed) = time.duration_since(*last)
                    && elapsed < threshold
                {
                    self.blocked.insert(code);
                    return Verdict::Chatter(elapsed);
                }
                Verdict::Pass
            }
            KEY_RELEASE => {
                if self.blocked.remove(&code) {
                    return Verdict::Swallow;
                }
                self.last_release.insert(code, time);
                Verdict::Pass
            }
            KEY_REPEAT if self.blocked.contains(&code) => Verdict::Swallow,
            _ => Verdict::Pass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: u16 = 30;
    const TH: Duration = Duration::from_millis(50);

    fn at(ms: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn normal_typing_passes() {
        let mut f = ChatterFilter::new();
        assert_eq!(f.process(KEY, KEY_PRESS, at(0), TH), Verdict::Pass);
        assert_eq!(f.process(KEY, KEY_RELEASE, at(80), TH), Verdict::Pass);
        assert_eq!(f.process(KEY, KEY_PRESS, at(200), TH), Verdict::Pass);
        assert_eq!(f.process(KEY, KEY_RELEASE, at(260), TH), Verdict::Pass);
    }

    #[test]
    fn chatter_press_and_its_release_are_blocked() {
        let mut f = ChatterFilter::new();
        f.process(KEY, KEY_PRESS, at(0), TH);
        f.process(KEY, KEY_RELEASE, at(80), TH);
        assert_eq!(
            f.process(KEY, KEY_PRESS, at(100), TH),
            Verdict::Chatter(Duration::from_millis(20))
        );
        assert_eq!(f.process(KEY, KEY_REPEAT, at(400), TH), Verdict::Swallow);
        assert_eq!(f.process(KEY, KEY_RELEASE, at(500), TH), Verdict::Swallow);
        // The swallowed release does not rearm the window: it still counts from the release at 80 ms.
        assert_eq!(f.process(KEY, KEY_PRESS, at(520), TH), Verdict::Pass);
    }

    #[test]
    fn zero_threshold_disables_filtering() {
        let mut f = ChatterFilter::new();
        f.process(KEY, KEY_PRESS, at(0), Duration::ZERO);
        f.process(KEY, KEY_RELEASE, at(10), Duration::ZERO);
        assert_eq!(f.process(KEY, KEY_PRESS, at(11), Duration::ZERO), Verdict::Pass);
    }

    #[test]
    fn keys_are_independent() {
        let mut f = ChatterFilter::new();
        f.process(KEY, KEY_PRESS, at(0), TH);
        f.process(KEY, KEY_RELEASE, at(10), TH);
        assert_eq!(f.process(KEY + 1, KEY_PRESS, at(20), TH), Verdict::Pass);
    }

    #[test]
    fn clock_going_backwards_does_not_block() {
        let mut f = ChatterFilter::new();
        f.process(KEY, KEY_PRESS, at(100), TH);
        f.process(KEY, KEY_RELEASE, at(200), TH);
        assert_eq!(f.process(KEY, KEY_PRESS, at(150), TH), Verdict::Pass);
    }
}
