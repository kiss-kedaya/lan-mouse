//! Pointer geometry helpers for seamless crossing between screens with
//! different sizes and vertical / horizontal offsets.
//!
//! All positions are pixels in the respective device's local screenspace.
//! Crossing a `Right` / `Left` barrier moves along the x axis and keeps the
//! pointer fraction along the y axis; `Top` / `Bottom` conversely. Because the
//! peer's real screen edge coordinates are exchanged via
//! [`crate::PeerGeometry`], devices of different heights line up instead of
//! letting the OS clamp the pointer to an arbitrary corner.

use crate::PeerGeometry;

/// 0..=1 position along an axis, ignoring degenerate ranges
/// `max` is the last addressable pixel, the mapping is done over pixels
pub fn fraction(position: f64, min: f64, max: f64) -> f64 {
    if max > min {
        ((position - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// inverse of [`fraction`]; clamps to the target range
pub fn lerp(t: f64, min: f64, max: f64) -> f64 {
    if max > min {
        min + t.clamp(0.0, 1.0) * (max - min)
    } else {
        min
    }
}

/// the side of `local` the pointer leaves through
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn is_horizontal(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    /// the fraction along this edge for a pointer at `(x, y)` of `peer`
    pub fn fraction_at(self, x: f64, y: f64, peer: &PeerGeometry) -> f64 {
        if self.is_horizontal() {
            fraction(y, peer.top, peer.bottom)
        } else {
            fraction(x, peer.left, peer.right)
        }
    }

    /// position on the edge of `peer` that faces this edge, at fraction `t`
    /// along the shared axis
    pub fn point_on(self, t: f64, peer: &PeerGeometry) -> (f64, f64) {
        match self {
            Edge::Right => (peer.left, lerp(t, peer.top, peer.bottom)),
            Edge::Left => (peer.right, lerp(t, peer.top, peer.bottom)),
            Edge::Bottom => (lerp(t, peer.left, peer.right), peer.top),
            Edge::Top => (lerp(t, peer.left, peer.right), peer.bottom),
        }
    }
}

/// fraction `t` (0..1 along our edge) to a point on the peer's facing edge
pub fn edge_point(edge: Edge, t: f64, peer: &PeerGeometry) -> (f64, f64) {
    edge.point_on(t, peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::Rect;

    fn geom(x: f64, y: f64, w: f64, h: f64) -> PeerGeometry {
        PeerGeometry::from_rect(Rect::new(x, y, w, h), true)
    }

    #[test]
    fn same_height_maps_one_to_one() {
        let local = geom(0., 0., 1920., 1080.);
        let peer = geom(0., 0., 2560., 1080.);
        let t = Edge::Right.fraction_at(0., 500., &local);
        assert_eq!(Edge::Right.point_on(t, &peer), (0., 500.));
    }

    #[test]
    fn different_height_is_scaled() {
        let laptop = geom(0., 0., 1440., 900.);
        let monitor = geom(0., 0., 2560., 1440.);
        let t = Edge::Right.fraction_at(0., 450., &laptop);
        let (_, y) = Edge::Right.point_on(t, &monitor);
        // 450 / 899 -> 450 / 899 * 1439
        assert!((y - 450.0 / 899.0 * 1439.0).abs() < 1e-6);
        // coming back via the monitor's own edge maps back to the laptop edge
        let t = Edge::Right.fraction_at(0., y, &monitor);
        let (_, y) = Edge::Right.point_on(t, &laptop);
        assert!((y - 450.).abs() < 1e-6);
    }

    #[test]
    fn offsets_are_respected() {
        // monitor placed above the laptop (negative y)
        let laptop = geom(0., 0., 1440., 900.);
        let monitor = geom(0., -1440., 2560., 1440.);
        let t = Edge::Right.fraction_at(0., 900., &laptop);
        let (x, y) = Edge::Right.point_on(t, &monitor);
        assert_eq!(x, 0.);
        // fraction 1 lands on the first pixel the peer can address
        assert!((y - (-1.)).abs() < 1e-6);
        // bottom of the laptop maps to the bottom of the monitor
        let t = Edge::Right.fraction_at(0., 899., &laptop);
        assert!((Edge::Right.point_on(t, &monitor).1 - (-1.)).abs() < 1e-6);
    }

    #[test]
    fn vertical_edges_use_x() {
        let a = geom(0., 0., 100., 100.);
        let b = geom(0., 100., 200., 100.);
        let t = Edge::Bottom.fraction_at(30., 99., &a);
        let (x, _) = Edge::Bottom.point_on(t, &b);
        // 30 / 99 -> 30 / 99 * 199
        assert!((x - 30.0 / 99.0 * 199.0).abs() < 1e-6);
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(lerp(-1., 0., 10.), 0.);
        assert_eq!(lerp(2., 0., 10.), 10.);
        assert_eq!(fraction(50., 0., 0.), 0.);
    }
}
