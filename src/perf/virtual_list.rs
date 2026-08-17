use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualWindow {
    start: usize,
    end: usize,
}

impl VirtualWindow {
    pub fn around(
        total_items: usize,
        first_visible: usize,
        visible_items: usize,
        overscan_items: usize,
    ) -> Self {
        if total_items == 0 || visible_items == 0 || first_visible >= total_items {
            return Self { start: 0, end: 0 };
        }

        let visible_end = first_visible.saturating_add(visible_items).min(total_items);
        let start = first_visible.saturating_sub(overscan_items);
        let end = visible_end.saturating_add(overscan_items).min(total_items);

        Self { start, end }
    }

    pub fn tail(total_items: usize, visible_items: usize, overscan_items: usize) -> Self {
        let first_visible = total_items.saturating_sub(visible_items);
        Self::around(total_items, first_visible, visible_items, overscan_items)
    }

    pub fn range(self) -> Range<usize> {
        self.start..self.end
    }

    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn materialize<T>(self, items: &[T]) -> &[T] {
        let start = self.start.min(items.len());
        let end = self.end.min(items.len()).max(start);
        &items[start..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialization_cost_is_bounded_by_viewport_not_history() {
        let items: Vec<usize> = (0..100_000).collect();
        let window = VirtualWindow::around(items.len(), 50_000, 40, 20);
        let visible = window.materialize(&items);

        assert_eq!(visible.len(), 80);
        assert_eq!(visible.first(), Some(&49_980));
        assert_eq!(visible.last(), Some(&50_059));
    }

    #[test]
    fn tail_window_tracks_streaming_append_without_full_history_layout() {
        let mut total_items = 0;

        for _ in 0..100_000 {
            total_items += 1;
            let window = VirtualWindow::tail(total_items, 40, 20);
            assert!(window.len() <= 60);
        }

        let final_window = VirtualWindow::tail(total_items, 40, 20);
        assert_eq!(final_window.range(), 99_940..100_000);
    }

    #[test]
    fn empty_and_out_of_range_viewports_are_safe() {
        assert!(VirtualWindow::around(0, 0, 40, 20).is_empty());
        assert!(VirtualWindow::around(100, 200, 40, 20).is_empty());
    }
}
