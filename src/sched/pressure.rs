//! Tracks sustained memory pressure for admission and background QoS.

use std::collections::VecDeque;

pub const SWAPOUT_RATE: f64 = 4.0 * 1024.0 * 1024.0;
const WINDOW: usize = 10;
pub const CALM_SECS: f64 = 30.0;

#[derive(Debug, Default)]
pub struct Tracker {
    swap_out_rates: VecDeque<f64>,
    last_seen: Option<f64>,
}

impl Tracker {
    pub fn update(&mut self, swap_out_rate: f64, system_level: u8, now: f64) -> (f64, bool) {
        self.swap_out_rates.push_back(swap_out_rate);
        if self.swap_out_rates.len() > WINDOW {
            self.swap_out_rates.pop_front();
        }
        let average = self.swap_out_rates.iter().sum::<f64>() / self.swap_out_rates.len() as f64;
        if average >= SWAPOUT_RATE || system_level >= 4 {
            self.last_seen = Some(now);
        }
        let pressured = self.last_seen.is_some_and(|t| now - t < CALM_SECS);
        (average, pressured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_and_waits_for_calm() {
        let mut t = Tracker::default();
        for s in 0..9 {
            t.update(0.0, 1, f64::from(s));
        }
        assert!(!t.update(8.0 * SWAPOUT_RATE, 1, 9.0).1);
        for s in 10..20 {
            t.update(1.5 * SWAPOUT_RATE, 1, f64::from(s));
        }
        assert!(t.update(1.5 * SWAPOUT_RATE, 1, 20.0).1);
        for s in 21..40 {
            t.update(0.0, 1, f64::from(s));
        }
        assert!(t.update(0.0, 1, 40.0).1);
        assert!(!t.update(0.0, 1, 60.0).1);
        assert!(t.update(0.0, 4, 61.0).1);
    }
}
