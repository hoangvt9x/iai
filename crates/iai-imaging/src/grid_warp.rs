//! Warp on a grid (Edit ▸ Transform ▸ Warp, as Photoshop has it): a layer's
//! box bent by a bicubic Bézier patch of 4 × 4 control points. The four
//! corners are anchors, the eight points beside them on the edges are their
//! handles, the four inside pull the middle; the grid shown is the patch's
//! lines at thirds. Dragging the surface anywhere moves the control points
//! so that the point grabbed follows the pointer.
//!
//! Everything is in one plane (the canvas): the net, the source layer's
//! place, the frame drawn into.

use rayon::prelude::*;

/// The control net, row by row (`index = row * 4 + column`), top-left first.
pub type Net = [(f32, f32); 16];

/// The four corners of the net.
pub const ANCHORS: [usize; 4] = [0, 3, 12, 15];
/// The handles shown beside the corners, each with its anchor.
pub const HANDLES: [(usize, usize); 8] = [
    (1, 0),
    (4, 0),
    (2, 3),
    (7, 3),
    (8, 12),
    (13, 12),
    (11, 15),
    (14, 15),
];

/// The net of an unbent box `(x, y, w, h)`.
pub fn identity(x: f32, y: f32, w: f32, h: f32) -> Net {
    let mut net = [(0.0, 0.0); 16];
    for (k, p) in net.iter_mut().enumerate() {
        let (i, j) = ((k % 4) as f32, (k / 4) as f32);
        *p = (x + w * i / 3.0, y + h * j / 3.0);
    }
    net
}

fn bernstein(t: f32) -> [f32; 4] {
    let s = 1.0 - t;
    [s * s * s, 3.0 * s * s * t, 3.0 * s * t * t, t * t * t]
}

/// How much each control point counts at `(u, v)`.
pub fn weights(u: f32, v: f32) -> [f32; 16] {
    let (bu, bv) = (bernstein(u), bernstein(v));
    let mut w = [0.0; 16];
    for (k, wk) in w.iter_mut().enumerate() {
        *wk = bu[k % 4] * bv[k / 4];
    }
    w
}

/// The point of the patch at `(u, v)` (`0..=1` across, down).
pub fn eval(net: &Net, u: f32, v: f32) -> (f32, f32) {
    let w = weights(u, v);
    net.iter()
        .zip(w)
        .fold((0.0, 0.0), |(x, y), (p, wk)| (x + p.0 * wk, y + p.1 * wk))
}

/// Move a control point; a corner takes its two handles along.
pub fn move_point(net: &mut Net, index: usize, dx: f32, dy: f32) {
    let Some(p) = net.get_mut(index) else {
        return;
    };
    p.0 += dx;
    p.1 += dy;
    if ANCHORS.contains(&index) {
        for &(h, a) in &HANDLES {
            if a == index {
                net[h].0 += dx;
                net[h].1 += dy;
            }
        }
    }
}

/// Pull the surface at `(u, v)` by `(dx, dy)`: the control points move as
/// little as they can for that point to move by exactly so much, those
/// nearest it the most.
pub fn pull(net: &mut Net, u: f32, v: f32, dx: f32, dy: f32) {
    let w = weights(u, v);
    let norm: f32 = w.iter().map(|x| x * x).sum();
    if norm <= f32::EPSILON {
        return;
    }
    for (p, wk) in net.iter_mut().zip(w) {
        p.0 += dx * wk / norm;
        p.1 += dy * wk / norm;
    }
}

/// The grid drawn on the canvas: the patch's edges and its lines at thirds,
/// each a polyline of `steps + 1` points.
pub fn grid_lines(net: &Net, steps: usize) -> Vec<Vec<(f32, f32)>> {
    let steps = steps.max(1);
    let mut lines = Vec::with_capacity(8);
    for k in 0..4 {
        let t = k as f32 / 3.0;
        lines.push(
            (0..=steps)
                .map(|s| eval(net, s as f32 / steps as f32, t))
                .collect(),
        );
        lines.push(
            (0..=steps)
                .map(|s| eval(net, t, s as f32 / steps as f32))
                .collect(),
        );
    }
    lines
}

/// The patch cut into small triangles: corners on the canvas and where
/// each one is on the patch. Reaches a little past the patch's edge
/// (`reach`, on the patch) so the edge is drawn soft, not stepped.
struct Tri {
    p: [(f32, f32); 3],
    uv: [(f32, f32); 3],
}

fn tessellate(net: &Net, n: usize, reach: (f32, f32)) -> Vec<Tri> {
    let n = n.max(1);
    let at = |i: usize, extent: f32| -extent + (1.0 + 2.0 * extent) * i as f32 / n as f32;
    let mut grid = Vec::with_capacity((n + 1) * (n + 1));
    for j in 0..=n {
        for i in 0..=n {
            let (u, v) = (at(i, reach.0), at(j, reach.1));
            grid.push((eval(net, u, v), (u, v)));
        }
    }
    let mut tris = Vec::with_capacity(2 * n * n);
    for j in 0..n {
        for i in 0..n {
            let a = grid[j * (n + 1) + i];
            let b = grid[j * (n + 1) + i + 1];
            let c = grid[(j + 1) * (n + 1) + i];
            let d = grid[(j + 1) * (n + 1) + i + 1];
            for (x, y, z) in [(a, b, d), (a, d, c)] {
                tris.push(Tri {
                    p: [x.0, y.0, z.0],
                    uv: [x.1, y.1, z.1],
                });
            }
        }
    }
    tris
}

/// Where on the triangle's plane `(x, y)` is, as the weights of its three
/// corners; `None` for a triangle squashed flat.
fn barycentric(t: &Tri, x: f32, y: f32) -> Option<[f32; 3]> {
    let [(x0, y0), (x1, y1), (x2, y2)] = t.p;
    let det = (y1 - y2) * (x0 - x2) + (x2 - x1) * (y0 - y2);
    if det.abs() < 1e-9 {
        return None;
    }
    let l0 = ((y1 - y2) * (x - x2) + (x2 - x1) * (y - y2)) / det;
    let l1 = ((y2 - y0) * (x - x2) + (x0 - x2) * (y - y2)) / det;
    Some([l0, l1, 1.0 - l0 - l1])
}

const INSIDE: f32 = -1e-4;

/// Where on the patch the canvas point `(x, y)` is, when it is on it.
pub fn locate(net: &Net, x: f32, y: f32) -> Option<(f32, f32)> {
    tessellate(net, 32, (0.0, 0.0)).iter().find_map(|t| {
        let l = barycentric(t, x, y)?;
        l.iter().all(|&w| w >= INSIDE).then(|| {
            (
                (l[0] * t.uv[0].0 + l[1] * t.uv[1].0 + l[2] * t.uv[2].0).clamp(0.0, 1.0),
                (l[0] * t.uv[0].1 + l[1] * t.uv[1].1 + l[2] * t.uv[2].1).clamp(0.0, 1.0),
            )
        })
    })
}

/// The box the bent patch covers, `(x0, y0, x1, y1)`.
pub fn bounds(net: &Net) -> (f32, f32, f32, f32) {
    let mut b = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for j in 0..=24 {
        for i in 0..=24 {
            let (x, y) = eval(net, i as f32 / 24.0, j as f32 / 24.0);
            b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
        }
    }
    b
}

/// The source the patch is drawn from: a layer's straight-alpha RGBA, where
/// it sits on the canvas, and the box of it the net bends.
pub struct Source<'a> {
    pub rgba: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub offset: (i32, i32),
    /// `(x, y, w, h)` on the canvas: the patch's `(0, 0)..(1, 1)`.
    pub frame: (f32, f32, f32, f32),
}

/// `v.floor()` as a whole number without the call `floor` is on a build
/// without SSE4.1 (once a pixel, it showed).
#[inline]
fn floor(v: f32) -> i64 {
    let t = v as i64;
    if (t as f32) > v { t - 1 } else { t }
}

/// One pixel of the source at a canvas point, weighed from the four
/// around it (alpha-weighted, so a transparent neighbour lends no colour),
/// straight alpha; transparent off the layer.
fn sample(src: &Source<'_>, x: f32, y: f32) -> [u8; 4] {
    let fx = x - src.offset.0 as f32 - 0.5;
    let fy = y - src.offset.1 as f32 - 0.5;
    let (ix, iy) = (floor(fx), floor(fy));
    let (tx, ty) = (fx - ix as f32, fy - iy as f32);
    let (w, h) = (src.width as i64, src.height as i64);
    // Inside, all four opaque (a photo, nearly every pixel): plain weights.
    if ix >= 0 && iy >= 0 && ix + 1 < w && iy + 1 < h {
        let i = ((iy * w + ix) * 4) as usize;
        let down = (w * 4) as usize;
        if let (Some(a), Some(b)) = (src.rgba.get(i..i + 8), src.rgba.get(i + down..i + down + 8))
            && a[3] == 255
            && a[7] == 255
            && b[3] == 255
            && b[7] == 255
        {
            let mut out = [255u8; 4];
            for c in 0..3 {
                let top = a[c] as f32 + (a[c + 4] as f32 - a[c] as f32) * tx;
                let bot = b[c] as f32 + (b[c + 4] as f32 - b[c] as f32) * tx;
                out[c] = (top + (bot - top) * ty + 0.5) as u8;
            }
            return out;
        }
    }
    let mut acc = [0.0f32; 4];
    for (dy, wy) in [(0i64, 1.0 - ty), (1, ty)] {
        for (dx, wx) in [(0i64, 1.0 - tx), (1, tx)] {
            let (px, py) = (ix + dx, iy + dy);
            if px < 0 || py < 0 || px >= w || py >= h {
                continue;
            }
            let i = ((py * w + px) * 4) as usize;
            let Some(p) = src.rgba.get(i..i + 4) else {
                continue;
            };
            let a = p[3] as f32 * wx * wy;
            acc[0] += p[0] as f32 * a;
            acc[1] += p[1] as f32 * a;
            acc[2] += p[2] as f32 * a;
            acc[3] += a;
        }
    }
    let a = acc[3];
    if a <= 0.0 {
        return [0; 4];
    }
    [
        iai_raster::fast_math::round_u8(acc[0] / a),
        iai_raster::fast_math::round_u8(acc[1] / a),
        iai_raster::fast_math::round_u8(acc[2] / a),
        a.round().clamp(0.0, 255.0) as u8,
    ]
}

/// The nearest pixel of the source at a canvas point; transparent off the
/// layer. A draft's sampling: a quarter of the reads.
fn nearest(src: &Source<'_>, x: f32, y: f32) -> [u8; 4] {
    let px = floor(x - src.offset.0 as f32);
    let py = floor(y - src.offset.1 as f32);
    if px < 0 || py < 0 || px >= src.width as i64 || py >= src.height as i64 {
        return [0; 4];
    }
    let i = (py as usize * src.width as usize + px as usize) * 4;
    match src.rgba.get(i..i + 4) {
        Some(p) => [p[0], p[1], p[2], p[3]],
        None => [0; 4],
    }
}

/// How a picture is drawn: finely (what is kept), or as a draft that
/// follows the pointer: nearest pixels, one in `step × step` (1, 2 or 4)
/// filling its block, which a view zoomed out that far cannot tell apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    Fine,
    Draft(u32),
}

/// The draft step a view at `zoom` (screen pixels per picture pixel) can
/// not tell from the fine picture: 1, 2 or 4.
pub fn draft_step(zoom: f32) -> u32 {
    if zoom <= 0.26 {
        4
    } else if zoom <= 0.51 {
        2
    } else {
        1
    }
}

/// Rows drawn by one worker; a multiple of every draft step.
const BAND: usize = 32;

/// Draw the source bent by `net` into a fresh straight-alpha RGBA frame
/// `(x, y, w, h)` on the canvas; what the patch leaves is transparent.
pub fn render(
    src: &Source<'_>,
    net: &Net,
    frame: (i32, i32, u32, u32),
    quality: Quality,
) -> Vec<u8> {
    let (fw, fh) = (frame.2, frame.3);
    let mut out = vec![0u8; fw as usize * fh as usize * 4];
    render_into(src, net, frame, &mut out, (0, 0, fw, fh), quality);
    out
}

/// Each triangle as planes over the frame: two of its corner weights (the
/// third is what they leave) and the source point, so a pixel costs a few
/// multiply-adds, no division.
struct Plane {
    l0: [f32; 3],
    l1: [f32; 3],
    sx: [f32; 3],
    sy: [f32; 3],
    x: (f32, f32),
    y: (f32, f32),
}

fn planes(
    src: &Source<'_>,
    net: &Net,
    origin: (f32, f32),
    rect: (f32, f32, f32, f32),
) -> Vec<Plane> {
    if src.frame.2 <= 0.0 || src.frame.3 <= 0.0 {
        return Vec::new();
    }
    // Finer for a bigger box: a cell some 16 px across at most.
    let n = ((src.frame.2.max(src.frame.3) / 16.0).ceil() as usize).clamp(8, 128);
    let reach = (0.75 / src.frame.2, 0.75 / src.frame.3);
    let span = |v: [f32; 3]| {
        (
            v.iter().copied().fold(f32::MAX, f32::min),
            v.iter().copied().fold(f32::MIN, f32::max),
        )
    };
    tessellate(net, n, reach)
        .into_iter()
        .filter_map(|t| {
            let p = t.p.map(|(x, y)| (x - origin.0, y - origin.1));
            let (x, y) = (span(p.map(|q| q.0)), span(p.map(|q| q.1)));
            if x.1 < rect.0 || x.0 > rect.2 || y.1 < rect.1 || y.0 > rect.3 {
                return None;
            }
            let [(x0, y0), (x1, y1), (x2, y2)] = p;
            let det = (y1 - y2) * (x0 - x2) + (x2 - x1) * (y0 - y2);
            if det.abs() < 1e-9 {
                return None;
            }
            let (a0, b0) = ((y1 - y2) / det, (x2 - x1) / det);
            let (a1, b1) = ((y2 - y0) / det, (x0 - x2) / det);
            let l0 = [a0, b0, -(a0 * x2 + b0 * y2)];
            let l1 = [a1, b1, -(a1 * x2 + b1 * y2)];
            // A value at the corners, as a plane: v2 + l0 (v0 - v2) + l1 (v1 - v2).
            let plane = |v: [f32; 3]| {
                let (d0, d1) = (v[0] - v[2], v[1] - v[2]);
                [
                    l0[0] * d0 + l1[0] * d1,
                    l0[1] * d0 + l1[1] * d1,
                    l0[2] * d0 + l1[2] * d1 + v[2],
                ]
            };
            let sx = plane(t.uv.map(|(u, _)| src.frame.0 + u * src.frame.2));
            let sy = plane(t.uv.map(|(_, v)| src.frame.1 + v * src.frame.3));
            Some(Plane {
                l0,
                l1,
                sx,
                sy,
                x,
                y,
            })
        })
        .collect()
}

/// Draw the source bent by `net` into `out`, the straight-alpha RGBA of the
/// frame `(x, y, w, h)` on the canvas, within `rect` (`x0, y0, x1, y1`,
/// frame pixels, end exclusive) only: that part is cleared first, the rest
/// of `out` is left as it was. What a drag moves is drawn again, not the
/// whole frame.
pub fn render_into(
    src: &Source<'_>,
    net: &Net,
    frame: (i32, i32, u32, u32),
    out: &mut [u8],
    rect: (u32, u32, u32, u32),
    quality: Quality,
) {
    let (fx, fy, fw, fh) = frame;
    let row = fw as usize * 4;
    if fw == 0 || fh == 0 || out.len() < row * fh as usize {
        return;
    }
    let (rx0, ry0) = (rect.0.min(fw) as usize, rect.1.min(fh) as usize);
    let (rx1, ry1) = (rect.2.min(fw) as usize, rect.3.min(fh) as usize);
    if rx1 <= rx0 || ry1 <= ry0 {
        return;
    }
    let (step, fine) = match quality {
        Quality::Fine => (1, true),
        Quality::Draft(s) => (s.clamp(1, 4) as usize, false),
    };
    // A block is drawn from the triangle its centre falls in, which may lie
    // up to a block outside the part.
    let reach = step as f32;
    let planes = planes(
        src,
        net,
        (fx as f32, fy as f32),
        (
            rx0 as f32 - reach,
            ry0 as f32 - reach,
            rx1 as f32 + reach,
            ry1 as f32 + reach,
        ),
    );
    // Which triangles each band of rows meets.
    let bands = (fh as usize).div_ceil(BAND);
    let mut in_band: Vec<Vec<u32>> = vec![Vec::new(); bands];
    for (k, t) in planes.iter().enumerate() {
        let b0 = ((t.y.0 - reach).max(0.0) as usize / BAND).min(bands - 1);
        let b1 = (t.y.1.max(0.0) as usize / BAND).min(bands - 1);
        for list in &mut in_band[b0..=b1] {
            list.push(k as u32);
        }
    }
    let at = |c: &[f32; 3], x: f32, y: f32| c[0] * x + c[1] * y + c[2];
    out[..row * fh as usize]
        .par_chunks_mut(row * BAND)
        .zip(in_band.par_iter())
        .enumerate()
        .for_each(|(band, (rows, mine))| {
            let top = band * BAND;
            let bottom = top + rows.len() / row;
            let (y_lo, y_hi) = (top.max(ry0), bottom.min(ry1));
            if y_lo >= y_hi {
                return;
            }
            for y in y_lo..y_hi {
                rows[(y - top) * row + rx0 * 4..(y - top) * row + rx1 * 4].fill(0);
            }
            for &k in mine {
                let t = &planes[k as usize];
                let x_start = ((t.x.0 - reach).floor().max(rx0 as f32) as usize) / step * step;
                let x_end = t.x.1.ceil().min(rx1 as f32 - 1.0).max(0.0) as usize;
                let y_start =
                    (((t.y.0 - reach).floor().max(y_lo as f32) as usize) / step * step).max(top);
                let y_end = t.y.1.ceil().min(y_hi as f32 - 1.0).max(0.0) as usize;
                let mut y = y_start;
                while y <= y_end {
                    let cy = y as f32 + step as f32 * 0.5;
                    let mut x = x_start;
                    while x <= x_end {
                        let cx = x as f32 + step as f32 * 0.5;
                        let w0 = at(&t.l0, cx, cy);
                        let w1 = at(&t.l1, cx, cy);
                        if w0 >= INSIDE && w1 >= INSIDE && 1.0 - w0 - w1 >= INSIDE {
                            let (sx, sy) = (at(&t.sx, cx, cy), at(&t.sy, cx, cy));
                            let p = if fine {
                                sample(src, sx, sy)
                            } else {
                                nearest(src, sx, sy)
                            };
                            for by in y.max(y_lo)..(y + step).min(y_hi) {
                                let line = (by - top) * row;
                                for bx in x.max(rx0)..(x + step).min(rx1) {
                                    rows[line + bx * 4..line + bx * 4 + 4].copy_from_slice(&p);
                                }
                            }
                        }
                        x += step;
                    }
                    y += step;
                }
            }
        });
}

/// The box (`x0, y0, x1, y1`, end exclusive) of the frame `(x, y, w, h)`
/// the bent patch covers, a pixel wider all round; empty when it is off the
/// frame.
pub fn covered(net: &Net, frame: (i32, i32, u32, u32)) -> (u32, u32, u32, u32) {
    let (x0, y0, x1, y1) = bounds(net);
    let clamp = |v: f32, lo: i32, len: u32| (v as i64 - lo as i64).clamp(0, len as i64) as u32;
    (
        clamp(x0.floor() - 1.0, frame.0, frame.2),
        clamp(y0.floor() - 1.0, frame.1, frame.3),
        clamp(x1.ceil() + 2.0, frame.0, frame.2),
        clamp(y1.ceil() + 2.0, frame.1, frame.3),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    #[test]
    fn an_unbent_net_is_the_box() {
        let net = identity(10.0, 20.0, 300.0, 150.0);
        assert!(close(eval(&net, 0.0, 0.0), (10.0, 20.0)));
        assert!(close(eval(&net, 1.0, 1.0), (310.0, 170.0)));
        assert!(close(eval(&net, 0.5, 0.25), (160.0, 57.5)));
        let (u, v) = locate(&net, 85.0, 95.0).expect("inside");
        assert!((u - 0.25).abs() < 1e-3 && (v - 0.5).abs() < 1e-3);
        assert!(locate(&net, 5.0, 95.0).is_none());
    }

    #[test]
    fn the_point_pulled_follows_the_pointer() {
        let mut net = identity(0.0, 0.0, 90.0, 90.0);
        let before = eval(&net, 0.4, 0.7);
        pull(&mut net, 0.4, 0.7, 12.0, -5.0);
        assert!(close(
            eval(&net, 0.4, 0.7),
            (before.0 + 12.0, before.1 - 5.0)
        ));
        // The far corner hardly moves.
        assert!(eval(&net, 1.0, 0.0).0 - 90.0 < 0.5);
    }

    #[test]
    fn a_corner_takes_its_handles_along() {
        let mut net = identity(0.0, 0.0, 90.0, 90.0);
        move_point(&mut net, 0, -6.0, 4.0);
        assert_eq!(net[0], (-6.0, 4.0));
        assert_eq!(net[1], (24.0, 4.0));
        assert_eq!(net[4], (-6.0, 34.0));
        assert_eq!(net[5], (30.0, 30.0), "the inside stays");
    }

    #[test]
    fn an_unbent_render_gives_the_layer_back() {
        let (w, h) = (40u32, 30u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for (i, p) in rgba.chunks_exact_mut(4).enumerate() {
            p.copy_from_slice(&[(i % 251) as u8, (i * 7 % 256) as u8, 90, 255]);
        }
        let src = Source {
            rgba: &rgba,
            width: w,
            height: h,
            offset: (5, 7),
            frame: (5.0, 7.0, w as f32, h as f32),
        };
        let net = identity(5.0, 7.0, w as f32, h as f32);
        let out = render(&src, &net, (5, 7, w, h), Quality::Fine);
        assert_eq!(render(&src, &net, (5, 7, w, h), Quality::Draft(1)), rgba);
        assert_eq!(out, rgba);
    }

    /// A drag draws again only the part it moved: inside the part, the
    /// same pixels as the whole frame drawn; outside, what was there.
    #[test]
    fn a_part_drawn_again_matches_the_whole_and_leaves_the_rest() {
        let (w, h) = (80u32, 60u32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i * 3 % 256) as u8, (i % 7 * 30) as u8, 77, 255])
            .collect();
        let src = Source {
            rgba: &rgba,
            width: w,
            height: h,
            offset: (0, 0),
            frame: (0.0, 0.0, w as f32, h as f32),
        };
        let mut net = identity(0.0, 0.0, w as f32, h as f32);
        pull(&mut net, 0.3, 0.6, 9.0, -4.0);
        for quality in [Quality::Fine, Quality::Draft(2), Quality::Draft(4)] {
            let whole = render(&src, &net, (0, 0, w, h), quality);
            let mut part = vec![7u8; (w * h * 4) as usize];
            let rect = (10, 4, 50, 41);
            render_into(&src, &net, (0, 0, w, h), &mut part, rect, quality);
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let inside = x >= rect.0 && x < rect.2 && y >= rect.1 && y < rect.3;
                    if inside {
                        assert_eq!(part[i..i + 4], whole[i..i + 4], "{quality:?} at {x},{y}");
                    } else {
                        assert_eq!(part[i..i + 4], [7; 4], "{quality:?} at {x},{y}");
                    }
                }
            }
        }
        assert_eq!(draft_step(1.0), 1);
        assert_eq!(draft_step(0.33), 2);
        assert_eq!(draft_step(0.1), 4);
    }

    #[test]
    fn a_corner_pulled_in_leaves_the_corner_clear() {
        let (w, h) = (64u32, 64u32);
        let rgba = vec![200u8; (w * h * 4) as usize];
        let src = Source {
            rgba: &rgba,
            width: w,
            height: h,
            offset: (0, 0),
            frame: (0.0, 0.0, w as f32, h as f32),
        };
        let mut net = identity(0.0, 0.0, w as f32, h as f32);
        move_point(&mut net, 15, -30.0, -30.0);
        let out = render(&src, &net, (0, 0, w, h), Quality::Fine);
        let at = |x: u32, y: u32| out[((y * w + x) * 4 + 3) as usize];
        assert_eq!(at(62, 62), 0, "the bottom-right corner is bare");
        assert_eq!(at(2, 2), 200, "the top-left stays");
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    /// How long a big photo takes to bend (`cargo test --release -- --ignored`).
    #[test]
    #[ignore = "probe: timing"]
    fn probe_render_time() {
        let (w, h) = (6000u32, 4000u32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, 90, 40, 255])
            .collect();
        let src = Source {
            rgba: &rgba,
            width: w,
            height: h,
            offset: (0, 0),
            frame: (0.0, 0.0, w as f32, h as f32),
        };
        let mut net = identity(0.0, 0.0, w as f32, h as f32);
        pull(&mut net, 0.5, 0.5, 300.0, 200.0);
        let mut out = vec![0u8; (w * h * 4) as usize];
        for quality in [
            Quality::Fine,
            Quality::Draft(1),
            Quality::Draft(2),
            Quality::Draft(4),
        ] {
            let t = std::time::Instant::now();
            render_into(&src, &net, (0, 0, w, h), &mut out, (0, 0, w, h), quality);
            println!("render {quality:?} {:?}", t.elapsed());
        }
    }
}
