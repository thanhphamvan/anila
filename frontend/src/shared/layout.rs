//! Panel placement is independent of terminals, sockets and the DOM.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Layout {
    #[default]
    Single,
    Dual,
    ThreeRight,
    ThreeLeft,
    Four,
}

impl Layout {
    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "dual" => Self::Dual,
            "three" | "three-right" => Self::ThreeRight,
            "three-left" => Self::ThreeLeft,
            "four" => Self::Four,
            _ => Self::Single,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Dual => "dual",
            Self::ThreeRight => "three-right",
            Self::ThreeLeft => "three-left",
            Self::Four => "four",
        }
    }

    pub(crate) fn count(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Dual => 2,
            Self::ThreeRight | Self::ThreeLeft => 3,
            Self::Four => 4,
        }
    }

    pub(crate) fn flipped(self) -> Self {
        match self {
            Self::ThreeRight => Self::ThreeLeft,
            Self::ThreeLeft => Self::ThreeRight,
            other => other,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Panels<T> {
    pub(crate) layout: Layout,
    pub(crate) slots: [Option<T>; 4],
    pub(crate) active: usize,
}

impl<T> Default for Panels<T> {
    fn default() -> Self {
        Self {
            layout: Layout::Single,
            slots: std::array::from_fn(|_| None),
            active: 0,
        }
    }
}

impl<T: Clone + PartialEq> Panels<T> {
    pub(crate) fn visible(&self) -> &[Option<T>] {
        &self.slots[..self.layout.count()]
    }

    pub(crate) fn current(&self) -> Option<&T> {
        self.slots[self.active].as_ref()
    }

    /// A terminal has one DOM instance: assigning it elsewhere swaps panels.
    pub(crate) fn assign(&mut self, index: usize, tab: T) {
        if index >= self.layout.count() {
            return;
        }
        let previous = self
            .slots
            .iter()
            .position(|value| value.as_ref() == Some(&tab));
        let replaced = self.slots[index].replace(tab);
        if let Some(previous) = previous.filter(|previous| *previous != index) {
            self.slots[previous] = replaced;
        }
        self.active = index;
    }

    pub(crate) fn select(&mut self, tab: T) {
        if let Some(index) = self
            .visible()
            .iter()
            .position(|value| value.as_ref() == Some(&tab))
        {
            self.active = index;
        } else {
            self.assign(self.active, tab);
        }
    }

    pub(crate) fn set_layout(&mut self, layout: Layout, candidates: &[T]) {
        self.layout = layout;
        if self.active >= layout.count() {
            self.slots.swap(0, self.active);
            self.active = 0;
        }
        let active = self.active;
        for index in 0..layout.count() {
            if self.slots[index].is_none()
                && let Some(tab) = candidates.iter().find(|tab| {
                    !self
                        .visible()
                        .iter()
                        .any(|value| value.as_ref() == Some(tab))
                })
            {
                self.assign(index, tab.clone());
            }
        }
        self.active = active;
    }

    pub(crate) fn remove(&mut self, tab: &T) {
        for slot in &mut self.slots {
            if slot.as_ref() == Some(tab) {
                *slot = None;
            }
        }
        if self.current().is_none() {
            self.active = self.visible().iter().position(Option::is_some).unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_invalid_layouts_are_single() {
        assert_eq!(Panels::<u8>::default().layout, Layout::Single);
        assert_eq!(Layout::parse("unknown"), Layout::Single);
        assert_eq!(Layout::parse("dual").count(), 2);
        assert_eq!(Layout::parse("four").count(), 4);
        assert_eq!(Layout::parse("three").name(), "three-right");
    }

    #[test]
    fn resizing_preserves_focus_and_hidden_sessions() {
        let mut panels = Panels::default();
        panels.select(1);
        panels.set_layout(Layout::Four, &[1, 2, 3, 4]);
        assert_eq!(panels.slots, [Some(1), Some(2), Some(3), Some(4)]);
        panels.select(4);
        panels.set_layout(Layout::Single, &[]);
        assert_eq!(panels.current(), Some(&4));
        panels.set_layout(Layout::Four, &[]);
        assert_eq!(panels.slots, [Some(4), Some(2), Some(3), Some(1)]);
    }

    #[test]
    fn duplicate_assignment_swaps_instead_of_cloning_a_terminal() {
        let mut panels = Panels::default();
        panels.set_layout(Layout::Four, &[1, 2, 3, 4]);
        panels.assign(1, 4);
        assert_eq!(panels.slots, [Some(1), Some(4), Some(3), Some(2)]);
        panels.select(3);
        assert_eq!(panels.active, 2);
        panels.assign(99, 5);
        assert_eq!(panels.active, 2);
    }

    #[test]
    fn flipping_and_closing_preserve_other_panels() {
        let mut panels = Panels::default();
        panels.set_layout(Layout::ThreeRight, &[1, 2, 3]);
        panels.set_layout(panels.layout.flipped(), &[]);
        assert_eq!(panels.layout.name(), "three-left");
        assert_eq!(panels.slots, [Some(1), Some(2), Some(3), None]);
        panels.remove(&1);
        assert_eq!(panels.current(), Some(&2));
        assert_eq!(panels.layout.flipped(), Layout::ThreeRight);
    }
}
