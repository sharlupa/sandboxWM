/// BSP/Dwindle tiling tree for sandboxWM.
///
/// Every node is either:
///   - Leaf(Window)  – an actual window taking up a rectangle
///   - Split         – an invisible container with two children and a split direction
///
/// When a new window is opened it splits the currently focused window's cell in half.
/// The split direction is chosen by the aspect ratio of the cell (wider → horizontal, taller → vertical).

use smithay::{
    desktop::Window,
    utils::{Logical, Rectangle},
};

/// Split direction for a container node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitDir {
    /// Side-by-side: left | right
    H,
    /// Stacked: top / bottom
    V,
}

/// A node in the BSP tile tree.
#[derive(Clone, Debug)]
pub enum TileNode {
    Leaf(Window),
    Split {
        dir: SplitDir,
        /// Fraction of the area that goes to `left` (0.0 – 1.0).
        ratio: f64,
        left: Box<TileNode>,
        right: Box<TileNode>,
    },
}

impl TileNode {
    // ── Queries ─────────────────────────────────────────────────────────────

    /// Does this subtree contain `w`?
    pub fn contains(&self, w: &Window) -> bool {
        match self {
            TileNode::Leaf(window) => window == w,
            TileNode::Split { left, right, .. } => left.contains(w) || right.contains(w),
        }
    }

    /// Collect all windows in depth-first order.
    pub fn windows(&self) -> Vec<Window> {
        match self {
            TileNode::Leaf(w) => vec![w.clone()],
            TileNode::Split { left, right, .. } => {
                let mut v = left.windows();
                v.extend(right.windows());
                v
            }
        }
    }

    /// Count leaf nodes.
    pub fn count(&self) -> usize {
        match self {
            TileNode::Leaf(_) => 1,
            TileNode::Split { left, right, .. } => left.count() + right.count(),
        }
    }

    /// Get the allocated area (before gaps) for `target`.
    pub fn area_for(
        &self,
        target: &Window,
        total: Rectangle<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        match self {
            TileNode::Leaf(w) => {
                if w == target { Some(total) } else { None }
            }
            TileNode::Split { dir, ratio, left, right } => {
                let (a, b) = split_rect(total, *dir, *ratio);
                left.area_for(target, a).or_else(|| right.area_for(target, b))
            }
        }
    }



    /// Walk the tree and collect `(window, final_rect)` with `gaps_in` applied to every leaf.
    pub fn collect_rects(
        &self,
        total: Rectangle<i32, Logical>,
        gaps_in: i32,
        out: &mut Vec<(Window, Rectangle<i32, Logical>)>,
    ) {
        match self {
            TileNode::Leaf(w) => {
                out.push((w.clone(), shrink(total, gaps_in)));
            }
            TileNode::Split { dir, ratio, left, right } => {
                let (a, b) = split_rect(total, *dir, *ratio);
                left.collect_rects(a, gaps_in, out);
                right.collect_rects(b, gaps_in, out);
            }
        }
    }

    /// Resizes the target window by adjusting the nearest horizontal and vertical splits.
    /// `dx` and `dy` are pixel deltas. `drag_left` and `drag_top` indicate which edges of the window are being dragged.
    pub fn resize_target(&mut self, target: &Window, dx: f64, dy: f64, drag_left: bool, drag_top: bool, area: Rectangle<i32, Logical>) -> bool {
        self.resize_target_impl(target, dx, dy, drag_left, drag_top, area).is_some()
    }

    /// Returns `Some((needs_h, needs_v))` where booleans indicate if a horizontal/vertical drag hasn't been consumed yet.
    fn resize_target_impl(&mut self, target: &Window, dx: f64, dy: f64, drag_left: bool, drag_top: bool, area: Rectangle<i32, Logical>) -> Option<(bool, bool)> {
        match self {
            TileNode::Leaf(w) => {
                if w == target {
                    // Reached the target. We now need to consume horizontal and vertical delta by traversing up.
                    Some((true, true))
                } else {
                    None
                }
            }
            TileNode::Split { dir, ratio, left, right } => {
                let (a, b) = split_rect(area, *dir, *ratio);
                
                // Check if target is in the left child
                if let Some((mut needs_h, mut needs_v)) = left.resize_target_impl(target, dx, dy, drag_left, drag_top, a) {
                    if *dir == SplitDir::H && needs_h && !drag_left {
                        // Dragging right edge of the target, and this split IS that right edge.
                        let delta_ratio = dx / area.size.w as f64;
                        *ratio = (*ratio + delta_ratio).clamp(0.05, 0.95);
                        needs_h = false; // Consumed
                    } else if *dir == SplitDir::V && needs_v && !drag_top {
                        // Dragging bottom edge of the target, and this split IS that bottom edge.
                        let delta_ratio = dy / area.size.h as f64;
                        *ratio = (*ratio + delta_ratio).clamp(0.05, 0.95);
                        needs_v = false; // Consumed
                    }
                    return Some((needs_h, needs_v));
                }
                
                // Check if target is in the right child
                if let Some((mut needs_h, mut needs_v)) = right.resize_target_impl(target, dx, dy, drag_left, drag_top, b) {
                    if *dir == SplitDir::H && needs_h && drag_left {
                        // Dragging left edge of the target, and this split IS that left edge.
                        let delta_ratio = dx / area.size.w as f64;
                        *ratio = (*ratio + delta_ratio).clamp(0.05, 0.95);
                        needs_h = false; // Consumed
                    } else if *dir == SplitDir::V && needs_v && drag_top {
                        // Dragging top edge of the target, and this split IS that top edge.
                        let delta_ratio = dy / area.size.h as f64;
                        *ratio = (*ratio + delta_ratio).clamp(0.05, 0.95);
                        needs_v = false; // Consumed
                    }
                    return Some((needs_h, needs_v));
                }
                
                None
            }
        }
    }

    // ── Mutations ────────────────────────────────────────────────────────────

    /// Insert `new_win` next to `focused` using the Dwindle algorithm.
    /// `total` is the area of the whole tree so we can compute the focused cell's aspect ratio.
    pub fn insert(
        self,
        focused: &Window,
        new_win: Window,
        total: Rectangle<i32, Logical>,
    ) -> TileNode {
        match self {
            TileNode::Leaf(w) => {
                if &w == focused {
                    // Split this cell; direction chosen by aspect ratio
                    let dir = if total.size.w >= total.size.h {
                        SplitDir::H
                    } else {
                        SplitDir::V
                    };
                    TileNode::Split {
                        dir,
                        ratio: 0.5,
                        left:  Box::new(TileNode::Leaf(w)),
                        right: Box::new(TileNode::Leaf(new_win)),
                    }
                } else {
                    TileNode::Leaf(w)
                }
            }
            TileNode::Split { dir, ratio, left, right } => {
                let (a, b) = split_rect(total, dir, ratio);
                if left.contains(focused) {
                    TileNode::Split {
                        dir, ratio,
                        left:  Box::new(left.insert(focused, new_win, a)),
                        right,
                    }
                } else if right.contains(focused) {
                    TileNode::Split {
                        dir, ratio,
                        left,
                        right: Box::new(right.insert(focused, new_win, b)),
                    }
                } else {
                    // Fallback: append to the rightmost leaf
                    TileNode::Split {
                        dir, ratio,
                        left,
                        right: Box::new(right.append(new_win, b)),
                    }
                }
            }
        }
    }

    /// Append `new_win` to the rightmost leaf (fallback when focused is unknown).
    pub fn append(self, new_win: Window, total: Rectangle<i32, Logical>) -> TileNode {
        match self {
            TileNode::Leaf(w) => {
                let dir = if total.size.w >= total.size.h { SplitDir::H } else { SplitDir::V };
                TileNode::Split {
                    dir,
                    ratio: 0.5,
                    left:  Box::new(TileNode::Leaf(w)),
                    right: Box::new(TileNode::Leaf(new_win)),
                }
            }
            TileNode::Split { dir, ratio, left, right } => {
                let (_, b) = split_rect(total, dir, ratio);
                TileNode::Split {
                    dir, ratio,
                    left,
                    right: Box::new(right.append(new_win, b)),
                }
            }
        }
    }

    /// Remove `target`. Returns `None` if this node itself should be deleted.
    pub fn remove(self, target: &Window) -> Option<TileNode> {
        match self {
            TileNode::Leaf(w) => {
                if &w == target { None } else { Some(TileNode::Leaf(w)) }
            }
            TileNode::Split { dir, ratio, left, right } => {
                match (left.remove(target), right.remove(target)) {
                    (None, None)    => None,
                    (Some(l), None) => Some(l),       // collapse: right was removed
                    (None, Some(r)) => Some(r),       // collapse: left was removed
                    (Some(l), Some(r)) => Some(TileNode::Split {
                        dir, ratio,
                        left:  Box::new(l),
                        right: Box::new(r),
                    }),
                }
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Divide `area` into two rectangles according to `dir` and `ratio`.
pub fn split_rect(
    area: Rectangle<i32, Logical>,
    dir: SplitDir,
    ratio: f64,
) -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
    match dir {
        SplitDir::H => {
            let w1 = ((area.size.w as f64 * ratio) as i32).max(1);
            let w2 = (area.size.w - w1).max(1);
            (
                Rectangle::new(area.loc, (w1, area.size.h).into()),
                Rectangle::new((area.loc.x + w1, area.loc.y).into(), (w2, area.size.h).into()),
            )
        }
        SplitDir::V => {
            let h1 = ((area.size.h as f64 * ratio) as i32).max(1);
            let h2 = (area.size.h - h1).max(1);
            (
                Rectangle::new(area.loc, (area.size.w, h1).into()),
                Rectangle::new((area.loc.x, area.loc.y + h1).into(), (area.size.w, h2).into()),
            )
        }
    }
}

/// Shrink `rect` by `gaps` on every side.
pub fn shrink(rect: Rectangle<i32, Logical>, gaps: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (rect.loc.x + gaps, rect.loc.y + gaps).into(),
        ((rect.size.w - gaps * 2).max(100), (rect.size.h - gaps * 2).max(100)).into(),
    )
}
