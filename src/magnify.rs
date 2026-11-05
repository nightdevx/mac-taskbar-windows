//! macOS dock magnification: icons near the pointer grow along a cosine curve
//! and push their neighbours aside; the icon under the pointer stays under it.

use crate::dock::Geometry;

/// Influence radius, in icon slots each side of the pointer.
pub const RADIUS: f32 = 2.5;

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Left edge and scale of each icon (DIPs, relative to the resting dock's left edge).
    pub items: Vec<(f32, f32)>,
    /// Background edges; they move out as icons grow.
    pub left: f32,
    pub right: f32,
}

/// Scale for an icon whose centre is `distance` DIPs from the pointer.
pub fn scale(g: Geometry, distance: f32, max: f32) -> f32 {
    let d = (distance.abs() / (g.icon + g.gap()) / RADIUS).min(1.0);
    1.0 + (max - 1.0) * (1.0 + (std::f32::consts::PI * d).cos()) / 2.0
}

/// Layout for `n` icons with the pointer at `x` (None: at rest).
pub fn layout(g: Geometry, n: usize, x: Option<f32>, max: f32) -> Layout {
    let step = g.icon + g.gap();
    let rest = Layout { items: (0..n).map(|i| (g.item_x(i), 1.0)).collect(), left: 0.0, right: g.width(n) };
    let Some(x) = x.filter(|_| max > 1.0 && n > 0) else { return rest };

    let scales: Vec<f32> = (0..n).map(|i| scale(g, x - (g.item_x(i) + g.icon / 2.0), max)).collect();
    let mut lefts = Vec::with_capacity(n);
    let mut edge = g.pad();
    for s in &scales {
        lefts.push(edge);
        edge += g.icon * s + g.gap();
    }
    // Keep the point under the pointer where it was: same slot, same fraction of it.
    let k = (((x - g.pad() + g.gap() / 2.0) / step).floor().max(0.0) as usize).min(n - 1);
    let slot_start = g.item_x(k) - g.gap() / 2.0;
    let frac = ((x - slot_start) / step).clamp(0.0, 1.0);
    let moved = lefts[k] - g.gap() / 2.0 + frac * (g.icon * scales[k] + g.gap());
    let shift = x - moved;

    Layout {
        items: lefts.iter().zip(&scales).map(|(l, s)| (l + shift, *s)).collect(),
        left: shift,
        right: shift + edge - g.gap() + g.pad(),
    }
}

/// Item under `x` in a magnified layout; gaps go to the nearer icon.
pub fn hit(g: Geometry, l: &Layout, x: f32) -> Option<usize> {
    if x < l.left || x >= l.right {
        return None;
    }
    l.items
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (x - (a.0 + g.icon * a.1 / 2.0)).abs();
            let db = (x - (b.0 + g.icon * b.1 / 2.0)).abs();
            da.total_cmp(&db)
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Geometry = Geometry { icon: 48.0 };

    #[test]
    fn scale_peaks_at_pointer_and_fades_to_one() {
        assert_eq!(scale(G, 0.0, 1.6), 1.6);
        let near = scale(G, 54.0, 1.6);
        assert!(near < 1.6 && near > 1.0, "{near}");
        assert_eq!(scale(G, 54.0 * RADIUS, 1.6), 1.0);
        assert_eq!(scale(G, 1000.0, 1.6), 1.0);
        assert_eq!(scale(G, -54.0, 1.6), scale(G, 54.0, 1.6), "symmetric");
    }

    #[test]
    fn at_rest_or_off_equals_plain_layout() {
        let rest = layout(G, 5, None, 1.6);
        assert_eq!(rest.items[2], (G.item_x(2), 1.0));
        assert_eq!((rest.left, rest.right), (0.0, G.width(5)));
        assert_eq!(layout(G, 5, Some(100.0), 1.0), rest, "max 1.0 is off");
    }

    #[test]
    fn magnified_icons_never_overlap_and_pointer_icon_stays_under_pointer() {
        for x in [10.0, 80.0, 133.0, 200.0, 290.0] {
            let l = layout(G, 6, Some(x), 1.8);
            for w in l.items.windows(2) {
                let (a, b) = (w[0], w[1]);
                assert!(b.0 >= a.0 + G.icon * a.1 + G.gap() - 0.01, "overlap at x={x}");
            }
            let i = hit(G, &l, x).unwrap();
            let (left, s) = l.items[i];
            assert!(x >= left - G.gap() && x <= left + G.icon * s + G.gap(), "x={x} drifted off icon {i}");
            assert!(l.left <= 0.0 && l.right >= G.width(6), "background grows, never shrinks");
        }
    }

    #[test]
    fn middle_pointer_grows_both_sides_equally() {
        let n = 5;
        let x = G.item_x(2) + G.icon / 2.0;
        let l = layout(G, n, Some(x), 1.6);
        let grew_left = -l.left;
        let grew_right = l.right - G.width(n);
        assert!((grew_left - grew_right).abs() < 0.01, "{grew_left} vs {grew_right}");
        assert_eq!(l.items[2].1, 1.6);
    }

    #[test]
    fn hit_at_rest_splits_gaps_between_neighbours() {
        let l = layout(G, 3, None, 1.5);
        assert_eq!(hit(G, &l, G.item_x(0) + 1.0), Some(0));
        assert_eq!(hit(G, &l, G.item_x(1) - G.gap() / 2.0 + 0.1), Some(1));
        assert_eq!(hit(G, &l, G.item_x(1) - G.gap() / 2.0 - 0.1), Some(0));
        assert_eq!(hit(G, &l, G.width(3) + 5.0), None);
        assert_eq!(hit(G, &l, -1.0), None);
    }
}
