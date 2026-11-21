// Z-NEO Manager - portable manager for the HM LAB Z-NEO 8K USB hub LCD.
// Copyright (C) 2026 charlesctchau
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the LICENSE file.

//! Pure scheduling logic for automatic picture rotation.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    #[default]
    Sequential,
    Random,
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Seconds-since-epoch at which the next change is due (0 means "immediately").
pub fn next_due(last_change: u64, interval_minutes: u32) -> u64 {
    if last_change == 0 {
        0
    } else {
        last_change.saturating_add(interval_minutes.max(1) as u64 * 60)
    }
}

pub fn is_due(now: u64, last_change: u64, interval_minutes: u32) -> bool {
    now >= next_due(last_change, interval_minutes)
}

/// Small xorshift generator; quality is irrelevant here, no extra crate needed.
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Self {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Rng(seed | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Order in which candidate files should be tried.
/// Sequential: starting at `cursor`, wrapping. Random: shuffled, never starting
/// with `avoid_first` when there is more than one file.
pub fn candidate_order(
    n: usize,
    order: Order,
    cursor: usize,
    rng: &mut Rng,
    avoid_first: Option<usize>,
) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    match order {
        Order::Sequential => (0..n).map(|i| (cursor + i) % n).collect(),
        Order::Random => {
            let mut v: Vec<usize> = (0..n).collect();
            for i in (1..n).rev() {
                let j = (rng.next() % (i as u64 + 1)) as usize;
                v.swap(i, j);
            }
            // The previously shown picture is only a last resort, so skipped (broken)
            // candidates can never lead back to it while another file is usable.
            if let Some(a) = avoid_first {
                if n > 1 {
                    v.retain(|&i| i != a);
                    v.push(a);
                }
            }
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_logic() {
        assert!(is_due(100, 0, 60));
        assert!(!is_due(1000, 1000, 60));
        assert!(!is_due(1000 + 3599, 1000, 60));
        assert!(is_due(1000 + 3600, 1000, 60));
        // interval 0 is clamped to 1 minute
        assert_eq!(next_due(1000, 0), 1060);
    }

    #[test]
    fn sequential_wraps_from_cursor() {
        let mut r = Rng::new();
        assert_eq!(candidate_order(4, Order::Sequential, 2, &mut r, None), vec![2, 3, 0, 1]);
        assert!(candidate_order(0, Order::Sequential, 0, &mut r, None).is_empty());
    }

    #[test]
    fn random_is_permutation_and_avoids_previous() {
        let mut r = Rng::new();
        for _ in 0..200 {
            let v = candidate_order(5, Order::Random, 0, &mut r, Some(3));
            let mut s = v.clone();
            s.sort();
            assert_eq!(s, vec![0, 1, 2, 3, 4]);
            assert_ne!(v[0], 3);
            assert_eq!(*v.last().unwrap(), 3);
        }
    }
}
