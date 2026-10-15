//! Arrival decrypts the card once. A new set of readings decrypts the list.
//! Moving the pointer sweeps that row. Nothing else moves on its own.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tachyonfx::fx::{self, Glitch};
use tachyonfx::{Duration, Effect, EffectManager, Interpolation, IntoEffect, Motion as Dir};

use crate::style::ink;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    #[default]
    Card,
    List,
    Row,
    Field,
}

pub struct Motion {
    effects: EffectManager<Slot>,
    enabled: bool,
    last: Instant,
}

impl Motion {
    pub fn new(enabled: bool) -> Self {
        Self {
            effects: EffectManager::default(),
            enabled,
            last: Instant::now(),
        }
    }

    pub fn arrive(&mut self, card: Rect) {
        if card.width == 0 || card.height == 0 {
            return;
        }
        let effect = fx::parallel(&[
            fx::coalesce((420, Interpolation::QuadOut)),
            fx::sweep_in(
                Dir::LeftToRight,
                18,
                0,
                ink::ICE,
                (480, Interpolation::QuadOut),
            ),
        ]);
        self.add(Slot::Card, effect.with_area(card));
    }

    pub fn decode(&mut self, list: Rect) {
        if list.width == 0 || list.height == 0 {
            return;
        }
        let effect = fx::parallel(&[
            fx::coalesce((320, Interpolation::QuadOut)),
            fx::fade_from_fg(ink::ICE, (360, Interpolation::QuadOut)),
        ]);
        self.add(Slot::List, effect.with_area(list));
    }

    pub fn lock(&mut self, row: Rect) {
        if row.width == 0 {
            return;
        }
        let effect = fx::sweep_in(
            Dir::LeftToRight,
            14,
            0,
            ink::ICE,
            (180, Interpolation::QuadOut),
        );
        self.add(Slot::Row, effect.with_area(row));
    }

    pub fn field(&mut self, row: Rect) {
        if row.width == 0 {
            return;
        }
        let effect = fx::sweep_in(
            Dir::LeftToRight,
            10,
            0,
            ink::AMBER,
            (180, Interpolation::QuadOut),
        );
        self.add(Slot::Field, effect.with_area(row));
    }

    pub fn fault(&mut self, card: Rect) {
        if card.width == 0 || card.height == 0 {
            return;
        }
        let glitch = Glitch::builder()
            .cell_glitch_ratio(0.12)
            .action_start_delay_ms(0..80)
            .action_ms(40..120)
            .build()
            .into_effect();
        let effect = fx::parallel(&[
            fx::with_duration(Duration::from_millis(320), glitch),
            fx::fade_from_fg(ink::HOT, (400, Interpolation::QuadOut)),
        ]);
        self.add(Slot::Card, effect.with_area(card));
    }

    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        self.effects.process_effects(elapsed.into(), buf, area);
    }

    fn add(&mut self, slot: Slot, effect: Effect) {
        if !self.enabled {
            return;
        }
        if !self.effects.is_running() {
            self.last = Instant::now();
        }
        self.effects.add_unique_effect(slot, effect);
    }
}
