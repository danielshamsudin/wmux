//! Pane layout as a binary tree, the same model tmux uses.
//!
//! Each leaf is a pane id. Each split node divides its rectangle in two and
//! keeps one row or column between the halves for the border line.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// `Horizontal` = panes side by side (tmux `%`).
/// `Vertical`   = panes stacked top and bottom (tmux `"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// A border line between two halves of a split.
pub struct Border {
    pub x: u16,
    pub y: u16,
    pub len: u16,
    pub vertical: bool,
}

pub enum Node {
    Leaf(usize),
    Split {
        axis: Axis,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

impl Node {
    /// Calculate the rectangle for each pane and each border.
    pub fn layout(&self, r: Rect, panes: &mut Vec<(usize, Rect)>, borders: &mut Vec<Border>) {
        match self {
            Node::Leaf(id) => panes.push((*id, r)),
            Node::Split { axis, ratio, a, b } => {
                let (ra, rb, border) = split_rect(r, *axis, *ratio);
                borders.push(border);
                a.layout(ra, panes, borders);
                b.layout(rb, panes, borders);
            }
        }
    }

    pub fn contains(&self, id: usize) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    pub fn first(&self) -> usize {
        match self {
            Node::Leaf(x) => *x,
            Node::Split { a, .. } => a.first(),
        }
    }

    /// Replace leaf `target` with a split of `target` and `new`.
    pub fn split(&mut self, target: usize, new: usize, axis: Axis) -> bool {
        match self {
            Node::Leaf(x) if *x == target => {
                *self = Node::Split {
                    axis,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(target)),
                    b: Box::new(Node::Leaf(new)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, axis) || b.split(target, new, axis),
        }
    }

    /// Remove leaf `target`. The sibling takes the space of the parent split.
    /// Returns `None` when the tree becomes empty.
    pub fn remove(self, target: usize) -> Option<Node> {
        match self {
            Node::Leaf(x) if x == target => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Split { axis, ratio, a, b } => match (a.remove(target), b.remove(target)) {
                (Some(a), Some(b)) => Some(Node::Split { axis, ratio, a: Box::new(a), b: Box::new(b) }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            },
        }
    }

    /// Move the nearest border on `axis` that touches pane `target`.
    /// A positive `delta` moves the border right or down.
    pub fn resize(&mut self, target: usize, axis: Axis, delta: f32) -> bool {
        let Node::Split { axis: my_axis, ratio, a, b } = self else {
            return false;
        };
        let child = if a.contains(target) {
            a
        } else if b.contains(target) {
            b
        } else {
            return false;
        };
        if child.resize(target, axis, delta) {
            return true;
        }
        if *my_axis == axis {
            *ratio = (*ratio + delta).clamp(0.05, 0.95);
            return true;
        }
        false
    }
}

/// Size of the first half. Each half gets at least one cell when possible.
fn first_size(avail: u16, ratio: f32) -> u16 {
    if avail < 2 {
        return avail;
    }
    ((avail as f32 * ratio).round() as u16).clamp(1, avail - 1)
}

fn split_rect(r: Rect, axis: Axis, ratio: f32) -> (Rect, Rect, Border) {
    match axis {
        Axis::Horizontal => {
            let avail = r.w.saturating_sub(1);
            let aw = first_size(avail, ratio);
            let a = Rect { w: aw, ..r };
            let b = Rect { x: r.x + aw + 1, w: avail - aw, ..r };
            (a, b, Border { x: r.x + aw, y: r.y, len: r.h, vertical: true })
        }
        Axis::Vertical => {
            let avail = r.h.saturating_sub(1);
            let ah = first_size(avail, ratio);
            let a = Rect { h: ah, ..r };
            let b = Rect { y: r.y + ah + 1, h: avail - ah, ..r };
            (a, b, Border { x: r.x, y: r.y + ah, len: r.w, vertical: false })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_remove() {
        let mut root = Node::Leaf(0);
        assert!(root.split(0, 1, Axis::Horizontal));
        assert!(root.split(1, 2, Axis::Vertical));
        let (mut p, mut b) = (vec![], vec![]);
        root.layout(Rect { x: 0, y: 0, w: 81, h: 25 }, &mut p, &mut b);
        assert_eq!(p[0], (0, Rect { x: 0, y: 0, w: 40, h: 25 }));
        assert_eq!(p[1], (1, Rect { x: 41, y: 0, w: 40, h: 12 }));
        assert_eq!(p[2], (2, Rect { x: 41, y: 13, w: 40, h: 12 }));
        let root = root.remove(1).unwrap();
        let (mut p, mut b) = (vec![], vec![]);
        root.layout(Rect { x: 0, y: 0, w: 81, h: 25 }, &mut p, &mut b);
        assert_eq!(p[1], (2, Rect { x: 41, y: 0, w: 40, h: 25 }));
        assert!(root.remove(0).unwrap().remove(2).is_none());
    }
}
