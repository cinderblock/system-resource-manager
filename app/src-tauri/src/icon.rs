//! The tray icon, drawn from the state it reports: one bar per slot, filled when held.
//! Something queued adds a marker in the corner.

use srm_proto::{Fleet, DEFAULT_POOL};

pub const SIZE: u32 = 32;
const GAP: u32 = 2;
const CORNER: u32 = 10;

const FILLED: [u8; 4] = [0x39, 0x87, 0xe5, 0xff];
const FREE: [u8; 4] = [0x80, 0x80, 0x80, 0x70];
const FULL: [u8; 4] = [0xfa, 0xb2, 0x19, 0xff];
const QUEUED: [u8; 4] = [0xd0, 0x3b, 0x3b, 0xff];

/// What the icon depends on. It's only redrawn when this changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Face {
    pub used: u32,
    pub capacity: u32,
    pub waiting: u32,
}

impl Face {
    pub fn of(fleet: &Fleet) -> Self {
        fleet
            .local
            .pool(DEFAULT_POOL)
            .map(|pool| Self {
                used: pool.used,
                capacity: pool.capacity,
                waiting: pool.waiters.len() as u32,
            })
            .unwrap_or_default()
    }

    pub fn tooltip(&self, cpu_percent: f32) -> String {
        let mut text = format!(
            "{} of {} slots in use, CPU {cpu_percent:.0}%",
            self.used, self.capacity
        );
        if self.waiting > 0 {
            text.push_str(&format!(", {} waiting", self.waiting));
        }
        text
    }
}

pub fn draw(face: Face) -> Vec<u8> {
    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
    let mut paint = |x: u32, y: u32, color: [u8; 4]| {
        let at = ((y * SIZE + x) * 4) as usize;
        pixels[at..at + 4].copy_from_slice(&color);
    };

    let bars = face.capacity.clamp(1, SIZE / 3);
    let width = (SIZE - GAP * (bars - 1)) / bars;
    let inset = (SIZE - (width * bars + GAP * (bars - 1))) / 2;
    let full = face.used >= face.capacity && face.capacity > 0;
    for bar in 0..bars {
        let left = inset + bar * (width + GAP);
        let held = bar < face.used;
        let color = match (held, full) {
            (true, true) => FULL,
            (true, false) => FILLED,
            (false, _) => FREE,
        };
        for x in left..left + width {
            for y in 0..SIZE {
                paint(x, y, color);
            }
        }
    }

    if face.waiting > 0 {
        for x in SIZE - CORNER..SIZE {
            for y in 0..CORNER {
                paint(x, y, QUEUED);
            }
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
        let at = ((y * SIZE + x) * 4) as usize;
        pixels[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn held_slots_fill_from_the_left() {
        let face = Face {
            used: 1,
            capacity: 4,
            waiting: 0,
        };
        let pixels = draw(face);
        assert_eq!(pixel(&pixels, 1, 20), FILLED);
        assert_eq!(pixel(&pixels, SIZE - 2, 20), FREE);
    }

    #[test]
    fn a_full_budget_changes_color() {
        let face = Face {
            used: 4,
            capacity: 4,
            waiting: 0,
        };
        assert_eq!(pixel(&draw(face), 1, 20), FULL);
    }

    #[test]
    fn a_queue_marks_the_corner() {
        let face = Face {
            used: 4,
            capacity: 4,
            waiting: 1,
        };
        assert_eq!(pixel(&draw(face), SIZE - 1, 0), QUEUED);
    }

    #[test]
    fn huge_pools_and_empty_ones_both_draw() {
        for capacity in [0, 1, 7, 64, 1000] {
            draw(Face {
                used: capacity,
                capacity,
                waiting: 0,
            });
        }
    }

    #[test]
    fn over_capacity_is_still_full() {
        let face = Face {
            used: 6,
            capacity: 4,
            waiting: 0,
        };
        assert_eq!(pixel(&draw(face), 1, 20), FULL);
    }

    #[test]
    fn the_tooltip_only_mentions_a_queue_when_there_is_one() {
        let mut face = Face {
            used: 2,
            capacity: 4,
            waiting: 0,
        };
        assert_eq!(face.tooltip(33.4), "2 of 4 slots in use, CPU 33%");
        face.waiting = 3;
        assert_eq!(face.tooltip(33.4), "2 of 4 slots in use, CPU 33%, 3 waiting");
    }
}
