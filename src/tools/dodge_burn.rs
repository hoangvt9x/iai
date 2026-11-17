//! Dodge & Burn: paint to lighten (Dodge) or darken (Burn) the tones of one
//! range (Shadows / Midtones / Highlights), the way Photoshop's do. Measured
//! from Photoshop 2020 (`tests/fixtures/brush_pts/dodge_burn.json`, plan
//! `KE_HOACH_AUDIT_CO_VE_BRUSH_2026-11-16` §11):
//!
//! - a colour moves along the tool's curve for its Range and Protect Tones
//!   ([`CURVES`]): every channel on its own with Protect Tones off; with it
//!   on, keeping the hue and never clipping to white ([`protected`]);
//! - Exposure is the stroke's flow: one dab moves a pixel `exposure × tip`
//!   of the way to its curve, the dabs of one stroke build up to the whole
//!   curve and no further, and the next stroke starts from where the last
//!   left the picture;
//! - the tip is the Brush's (Photoshop's round tip).
//!
//! Both tools share one struct; the `burn` flag picks which (two toolbar
//! tools, one code path).

use super::brush::{BrushTool, MAX_TIP_SIZE, RoundTip, StrokeBuffer, q8};
use super::scrub;
use super::{PointerEvent, Tool, ToolCtx, ToolResponse};
use crate::core::canvas::Canvas;
use crate::core::tile::{TILE_SIZE, Tile, TilePos};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneRange {
    Shadows,
    Midtones,
    Highlights,
}

impl ToneRange {
    pub fn label(&self) -> &'static str {
        match self {
            ToneRange::Shadows => "Shadows",
            ToneRange::Midtones => "Midtones",
            ToneRange::Highlights => "Highlights",
        }
    }
    pub fn to_u8(self) -> u8 {
        match self {
            ToneRange::Shadows => 0,
            ToneRange::Midtones => 1,
            ToneRange::Highlights => 2,
        }
    }
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => ToneRange::Shadows,
            2 => ToneRange::Highlights,
            _ => ToneRange::Midtones,
        }
    }
}

/// Knots of a curve: at 0, 8, … 248, then 255.
const KNOTS: usize = 33;

/// What a channel at `x` (0..255) becomes at 100 % Exposure (with Protect
/// Tones, the brightest and the darkest channel of a colour): Dodge then
/// Burn, each Shadows / Midtones / Highlights, each Protect Tones on then
/// off. Values past 0..255 are kept: they clip only after Exposure has
/// scaled the change (Photoshop, measured: a lower Exposure moves a channel
/// exactly that share of the way). Within 3 levels of Photoshop's at 20, 50
/// and 100 %.
#[rustfmt::skip]
const CURVES: [[f32; KNOTS]; 12] = [
    // Dodge, Shadows, Protect Tones on
    [
        0.0, 11.8, 23.2, 34.4, 45.0, 55.2, 64.7, 74.1, 82.5, 90.8, 98.4, 106.1, 112.7, 119.5,
        125.9, 132.1, 138.5, 144.8, 151.2, 157.8, 164.3, 171.4, 178.1, 185.8, 193.1, 200.8,
        208.0, 216.0, 224.0, 232.0, 240.0, 248.0, 255.0,
    ],
    // Dodge, Shadows, Protect Tones off
    [
        50.0, 56.6, 63.0, 69.3, 76.0, 82.1, 88.5, 95.2, 101.7, 108.0, 114.5, 120.9, 127.2,
        133.7, 140.2, 146.5, 153.0, 159.7, 165.9, 172.2, 178.6, 185.2, 191.7, 198.2, 204.5,
        210.9, 217.4, 223.7, 230.1, 236.6, 243.3, 249.6, 255.0,
    ],
    // Dodge, Midtones, Protect Tones on
    [
        0.0, 8.0, 16.8, 25.8, 35.6, 45.7, 56.4, 67.2, 78.3, 89.6, 100.8, 112.0, 123.0, 133.7,
        143.8, 153.6, 162.7, 171.2, 179.2, 186.2, 193.1, 199.4, 205.1, 210.4, 215.7, 220.6,
        225.6, 230.4, 235.3, 240.4, 245.3, 250.4, 255.0,
    ],
    // Dodge, Midtones, Protect Tones off
    [
        0.0, 16.1, 30.6, 42.8, 53.8, 64.0, 72.7, 81.2, 89.2, 97.0, 104.8, 112.0, 120.0, 127.2,
        133.9, 141.8, 148.5, 156.0, 162.5, 169.9, 176.1, 183.3, 190.2, 196.9, 203.4, 210.2,
        216.8, 223.3, 229.8, 236.3, 242.8, 249.4, 255.0,
    ],
    // Dodge, Highlights, Protect Tones on
    [
        0.0, 8.0, 16.0, 24.0, 32.0, 40.0, 48.5, 57.5, 66.8, 77.6, 89.4, 102.7, 117.0, 132.0,
        147.1, 162.1, 177.1, 191.8, 205.8, 219.7, 233.4, 246.7, 260.0, 272.8, 285.3, 298.0,
        310.9, 323.8, 336.8, 349.7, 362.6, 375.5, 386.9,
    ],
    // Dodge, Highlights, Protect Tones off
    [
        0.0, 9.2, 19.0, 29.0, 39.1, 48.8, 58.8, 68.7, 78.6, 88.5, 98.3, 108.1, 118.0, 128.1,
        138.0, 147.8, 157.7, 167.6, 177.4, 187.5, 197.4, 207.2, 217.2, 227.1, 236.9, 246.9,
        256.8, 266.7, 276.6, 286.5, 296.4, 306.2, 314.9,
    ],
    // Burn, Shadows, Protect Tones on
    [
        0.0, 0.5, 1.4, 3.4, 6.2, 9.8, 14.5, 20.2, 26.9, 34.9, 43.0, 52.5, 62.5, 73.2, 84.2,
        95.6, 107.0, 118.2, 129.6, 140.4, 151.0, 161.5, 171.2, 180.7, 190.1, 198.8, 207.2,
        216.0, 224.0, 232.0, 240.0, 248.0, 255.0,
    ],
    // Burn, Shadows, Protect Tones off
    [
        -60.0, -50.1, -40.2, -30.3, -20.4, -10.5, -0.6, 9.3, 19.3, 29.1, 39.0, 49.0, 58.7,
        68.7, 78.7, 88.7, 98.5, 108.4, 118.3, 128.1, 138.1, 148.1, 157.9, 168.0, 177.8, 187.7,
        197.6, 207.3, 217.1, 227.2, 237.1, 247.1, 255.0,
    ],
    // Burn, Midtones, Protect Tones on
    [
        0.0, 8.0, 15.2, 22.2, 28.4, 34.3, 39.6, 44.8, 49.7, 54.4, 59.2, 64.0, 69.0, 74.3, 80.2,
        86.4, 93.3, 100.8, 108.8, 117.8, 126.9, 136.6, 146.9, 157.6, 168.3, 179.4, 190.4,
        201.6, 212.7, 223.6, 234.7, 245.6, 255.0,
    ],
    // Burn, Midtones, Protect Tones off
    [
        0.0, 4.8, 10.2, 15.3, 21.2, 26.7, 32.7, 38.8, 45.4, 51.8, 58.2, 65.5, 72.1, 80.0, 87.2,
        95.0, 102.8, 111.0, 119.3, 128.0, 136.3, 145.7, 154.4, 164.0, 173.6, 183.3, 192.9,
        203.0, 213.5, 223.7, 234.6, 245.2, 255.0,
    ],
    // Burn, Highlights, Protect Tones on
    [
        0.0, 8.0, 16.0, 24.0, 32.0, 40.0, 48.0, 55.5, 62.9, 69.9, 76.1, 82.2, 87.6, 93.0, 98.0,
        103.1, 108.3, 113.6, 119.3, 124.8, 130.5, 136.5, 142.4, 148.5, 154.8, 160.7, 167.1,
        173.3, 179.6, 185.9, 192.1, 198.3, 204.0,
    ],
    // Burn, Highlights, Protect Tones off
    [
        0.0, 6.2, 12.5, 19.2, 25.7, 32.1, 38.5, 45.1, 51.3, 57.7, 64.2, 70.6, 77.2, 83.7, 90.1,
        96.1, 102.9, 109.3, 115.6, 122.2, 128.6, 135.0, 141.3, 148.0, 154.1, 160.5, 167.2,
        173.7, 180.0, 186.5, 192.9, 199.2, 205.0,
    ],
];

fn knot_x(i: usize) -> f32 {
    if i + 1 >= KNOTS {
        255.0
    } else {
        i as f32 * 8.0
    }
}

/// The curve for this tool, Range and Protect Tones, as a target for each
/// of the 256 channel values.
fn targets(burn: bool, range: ToneRange, protect: bool) -> [f32; 256] {
    let curve = &CURVES[usize::from(burn) * 6 + range.to_u8() as usize * 2 + usize::from(!protect)];
    let mut out = [0.0f32; 256];
    for (v, slot) in out.iter_mut().enumerate() {
        let x = v as f32;
        let i = ((x / 8.0) as usize).min(KNOTS - 2);
        let (x0, x1) = (knot_x(i), knot_x(i + 1));
        let t = ((x - x0) / (x1 - x0)).clamp(0.0, 1.0);
        *slot = curve[i] + (curve[i + 1] - curve[i]) * t;
    }
    out
}

/// Protect Tones on a colour pixel, `k` of the way (0..255 out, unrounded).
/// Photoshop moves the brightest and the darkest channel along the curve,
/// puts the middle one where the hue stays put, and when the brightest
/// would pass white scales the whole colour down instead of clipping it:
/// skin dodged in the highlights keeps its colour rather than going white.
/// Measured against Photoshop 2020 on grey, skin and saturated colours:
/// within 1 level, 2 on bright skin at 100 %.
#[inline]
fn protected(rgb: [u8; 3], targets: &[f32; 256], k: f32) -> [f32; 3] {
    let hi = rgb[0].max(rgb[1]).max(rgb[2]);
    let lo = rgb[0].min(rgb[1]).min(rgb[2]);
    let (t_hi, t_lo) = (targets[hi as usize], targets[lo as usize]);
    let (hi_f, lo_f) = (hi as f32, lo as f32);
    // Where each channel sits between the darkest and the brightest (a grey
    // has no spread: every channel takes the curve's value). A table of
    // `1 / spread` in place of the division measured slower (17/11).
    let stretch = if hi > lo {
        (t_hi - t_lo) / (hi_f - lo_f)
    } else {
        0.0
    };
    let mut out = rgb.map(|v| {
        let x = v as f32;
        let full = t_lo + (x - lo_f) * stretch;
        x + k * (full - x)
    });
    let top = out[0].max(out[1]).max(out[2]);
    if top > 255.0 {
        let s = 255.0 / top;
        for v in &mut out {
            *v *= s;
        }
    }
    out
}

pub struct DodgeBurnTool {
    pub burn: bool,
    pub size: f32,
    pub hardness: f32,
    /// The stroke's flow (Photoshop's Exposure), 0..1.
    pub exposure: f32,
    pub range: ToneRange,
    pub protect_tones: bool,
    pub spacing: f32,
    last: (f32, f32),
    /// Distance to the next dab along the stroke.
    residual: f32,
    /// The picture as the stroke began, and how far each pixel has gone.
    stroke: Option<StrokeBuffer>,
    /// Where each channel value is headed, for this stroke's settings.
    targets: [f32; 256],
}

impl DodgeBurnTool {
    fn new(burn: bool) -> Self {
        Self {
            burn,
            size: 60.0,
            hardness: 0.0,
            exposure: 0.5,
            range: ToneRange::Midtones,
            protect_tones: true,
            spacing: 0.25,
            last: (0.0, 0.0),
            residual: 0.0,
            stroke: None,
            targets: [0.0; 256],
        }
    }
    pub fn dodge() -> Self {
        Self::new(false)
    }
    pub fn burn() -> Self {
        Self::new(true)
    }

    fn dab_spacing(&self) -> f32 {
        (self.size * self.spacing).max(1.0)
    }

    /// One dab at canvas `(cx, cy)`. Like the Brush's, each tile the dab
    /// touches is taken out of the layer (with its coverage) for the length
    /// of the dab and the tiles are painted side by side on every core.
    fn dab(&mut self, canvas: &mut Canvas, cx: f32, cy: f32) {
        let flow = self.exposure.clamp(0.0, 1.0);
        let Some(stroke) = self.stroke.as_mut() else {
            return;
        };
        if flow <= 0.0 {
            return;
        }
        let Some(((ox, oy), (lw, lh))) = scrub::paint_frame(canvas) else {
            return;
        };
        let tip = RoundTip::new(
            (self.size * 0.5).clamp(0.5, MAX_TIP_SIZE * 0.5),
            self.hardness,
        );
        let reach = tip.reach();
        let r2 = reach * reach;
        let (lut, lut_scale) = tip.table();
        let (lcx, lcy) = (cx - ox as f32, cy - oy as f32);
        let lx0 = ((lcx - reach).floor().max(0.0) as u32).min(lw);
        let ly0 = ((lcy - reach).floor().max(0.0) as u32).min(lh);
        let lx1 = ((lcx + reach).ceil().max(0.0) as u32).min(lw);
        let ly1 = ((lcy + reach).ceil().max(0.0) as u32).min(lh);
        if lx1 <= lx0 || ly1 <= ly0 {
            return;
        }
        let paint_mask = stroke.paint_mask;
        let channel_wm = if paint_mask {
            None
        } else {
            canvas.channels.write_gate()
        };
        let idx = canvas.layer_stack.active_idx;
        let sel = &canvas.selection;
        let Some(tiles) = canvas.layer_stack.layers[idx].get_paint_tiles_mut() else {
            return;
        };
        let (base_tiles, cov_tiles) = (&stroke.base.tiles, &mut stroke.cov);
        let ts = TILE_SIZE;
        let mut jobs: Vec<TileJob> = Vec::new();
        for ty in (ly0 / ts)..=((ly1 - 1) / ts) {
            let (py_min, py_max) = ((ty * ts).max(ly0), (ty * ts + ts).min(ly1));
            let ny = lcy.clamp(py_min as f32 + 0.5, py_max as f32 - 0.5);
            let ndy2 = (ny - lcy) * (ny - lcy);
            for tx in (lx0 / ts)..=((lx1 - 1) / ts) {
                let (px_min, px_max) = ((tx * ts).max(lx0), (tx * ts + ts).min(lx1));
                let nx = lcx.clamp(px_min as f32 + 0.5, px_max as f32 - 0.5);
                if (nx - lcx) * (nx - lcx) + ndy2 > r2 {
                    continue;
                }
                let pos = TilePos {
                    x: tx as i32,
                    y: ty as i32,
                };
                // An empty tile has nothing to lighten or darken.
                let Some(base) = base_tiles.get(&pos) else {
                    continue;
                };
                // Copy-on-write and a fresh revision, as any 8-bit write.
                tiles.get_tile_mut(pos);
                let Some(tile) = tiles.tiles.remove(&pos) else {
                    continue;
                };
                let cov = cov_tiles
                    .remove(&pos)
                    .unwrap_or_else(|| vec![0.0f32; (ts * ts) as usize].into_boxed_slice());
                jobs.push(TileJob {
                    pos,
                    tile,
                    cov,
                    base,
                    tile_x0: tx * ts,
                    tile_y0: ty * ts,
                });
            }
        }
        let dab = Dab {
            lcx,
            lcy,
            r2,
            lut: &lut,
            lut_scale,
            flow,
            ox,
            oy,
            sel: sel.active.then_some(sel),
            channel_wm,
            paint_mask,
            protect: self.protect_tones,
            targets: &self.targets,
            lx0,
            ly0,
            lx1,
            ly1,
        };
        if jobs.len() >= 4 {
            use rayon::prelude::*;
            jobs.par_iter_mut().for_each(|job| dab.fill(job));
        } else {
            jobs.iter_mut().for_each(|job| dab.fill(job));
        }
        for job in jobs {
            tiles.tiles.insert(job.pos, job.tile);
            cov_tiles.insert(job.pos, job.cov);
        }
        let cx0 = (lx0 as i32 + ox).max(0) as u32;
        let cy0 = (ly0 as i32 + oy).max(0) as u32;
        let cx1 = ((lx1 as i32 + ox).max(0) as u32).min(canvas.width);
        let cy1 = ((ly1 as i32 + oy).max(0) as u32).min(canvas.height);
        canvas.mark_dirty(cx0, cy0, cx1, cy1);
    }
}

/// A tile of the layer, out for the length of one dab.
struct TileJob<'a> {
    pos: TilePos,
    tile: std::sync::Arc<Tile>,
    /// How far each pixel of the tile has gone in this stroke.
    cov: Box<[f32]>,
    /// The tile as the stroke began.
    base: &'a Tile,
    tile_x0: u32,
    tile_y0: u32,
}

/// What every tile of one dab is painted with.
struct Dab<'a> {
    lcx: f32,
    lcy: f32,
    r2: f32,
    lut: &'a [f32],
    lut_scale: f32,
    flow: f32,
    ox: i32,
    oy: i32,
    /// The selection, when there is one.
    sel: Option<&'a crate::core::selection::Selection>,
    channel_wm: Option<[bool; 4]>,
    paint_mask: bool,
    protect: bool,
    targets: &'a [f32; 256],
    lx0: u32,
    ly0: u32,
    lx1: u32,
    ly1: u32,
}

impl Dab<'_> {
    fn fill(&self, job: &mut TileJob) {
        let ts = TILE_SIZE;
        let (tile_x0, tile_y0) = (job.tile_x0, job.tile_y0);
        let (px_min, px_max) = (tile_x0.max(self.lx0), (tile_x0 + ts).min(self.lx1));
        let (py_min, py_max) = (tile_y0.max(self.ly0), (tile_y0 + ts).min(self.ly1));
        if px_max <= px_min || py_max <= py_min {
            return;
        }
        let pixels = &mut std::sync::Arc::make_mut(&mut job.tile).pixels;
        let (lcx, r2, targets) = (self.lcx, self.r2, self.targets);
        for py in py_min..py_max {
            let dy = py as f32 + 0.5 - self.lcy;
            let dy2 = dy * dy;
            if dy2 > r2 {
                continue;
            }
            // Only the pixels of this row the dab can reach, one to spare
            // on each side: the test on `d2` below still decides.
            let half = (r2 - dy2).sqrt();
            let x_min = ((lcx - 0.5 - half).floor() - 1.0).max(px_min as f32) as u32;
            let x_max = ((lcx - 0.5 + half).ceil() + 2.0).min(px_max as f32) as u32;
            if x_max <= x_min {
                continue;
            }
            let row = (py - tile_y0) * ts;
            let first = (row + x_min - tile_x0) as usize;
            let last = (row + x_max - tile_x0) as usize;
            let (Some(row_pix), Some(row_cov), Some(row_base)) = (
                pixels.get_mut(first * 4..last * 4),
                job.cov.get_mut(first..last),
                job.base.pixels.get(first * 4..last * 4),
            ) else {
                continue;
            };
            let canvas_py = (py as i32 + self.oy) as u32;
            for (((px, d), k), b) in (x_min..x_max)
                .zip(row_pix.chunks_exact_mut(4))
                .zip(row_cov)
                .zip(row_base.chunks_exact(4))
            {
                let dx = px as f32 + 0.5 - lcx;
                let d2 = dx * dx + dy2;
                if d2 > r2 {
                    continue;
                }
                let mut a = self
                    .lut
                    .get((d2 * self.lut_scale) as usize)
                    .copied()
                    .unwrap_or(0.0)
                    * self.flow;
                if let Some(sel) = self.sel {
                    a *= sel.sample((px as i32 + self.ox) as u32, canvas_py);
                }
                if a < 1e-4 {
                    continue;
                }
                if self.paint_mask {
                    *k += a * (1.0 - *k);
                    // A mask is grey: every rule below gives the same.
                    let x = b[0] as f32;
                    let g = q8((x + *k * (targets[b[0] as usize] - x)) / 255.0);
                    d.copy_from_slice(&[g, g, g, 255]);
                    continue;
                }
                if b[3] == 0 {
                    continue;
                }
                *k += a * (1.0 - *k);
                let k = *k;
                let rgb = [b[0], b[1], b[2]];
                let out = if self.protect {
                    protected(rgb, targets, k)
                } else {
                    rgb.map(|v| {
                        let x = v as f32;
                        x + k * (targets[v as usize] - x)
                    })
                };
                for c in 0..3 {
                    if self.channel_wm.is_none_or(|wm| wm[c]) {
                        d[c] = q8(out[c] / 255.0);
                    }
                }
            }
        }
    }
}

impl Tool for DodgeBurnTool {
    fn id(&self) -> &'static str {
        if self.burn { "burn" } else { "dodge" }
    }
    fn name(&self) -> &str {
        if self.burn { "Burn" } else { "Dodge" }
    }
    fn shortcut(&self) -> Option<char> {
        Some('O')
    }
    fn tool_id(&self) -> crate::tools::ToolId {
        if self.burn {
            crate::tools::ToolId::Burn
        } else {
            crate::tools::ToolId::Dodge
        }
    }
    fn paints(&self) -> bool {
        true
    }
    fn cursor_size(&self) -> f32 {
        self.size * 0.5
    }
    fn tip_hardness(&self) -> Option<f32> {
        Some(self.hardness)
    }

    fn on_press(&mut self, event: PointerEvent, ctx: &mut ToolCtx) -> ToolResponse {
        // Photoshop: Alt held at the press turns Dodge into Burn and Burn
        // into Dodge for that stroke.
        let burn = self.burn != event.alt;
        ctx.canvas_mut()
            .begin_stroke(if burn { "Burn" } else { "Dodge" });
        self.targets = targets(burn, self.range, self.protect_tones);
        self.stroke = StrokeBuffer::begin(ctx.canvas());
        self.last = (event.canvas_x, event.canvas_y);
        self.dab(ctx.canvas_mut(), event.canvas_x, event.canvas_y);
        self.residual = self.dab_spacing();
        ToolResponse::repaint()
    }

    fn on_drag(
        &mut self,
        event: PointerEvent,
        _prev: &PointerEvent,
        ctx: &mut ToolCtx,
    ) -> ToolResponse {
        let (x0, y0) = self.last;
        let (x1, y1) = (event.canvas_x, event.canvas_y);
        let mut residual = self.residual;
        let spacing = self.dab_spacing();
        let canvas = ctx.canvas_mut();
        BrushTool::walk_dabs(spacing, x0, y0, x1, y1, &mut residual, |x, y| {
            self.dab(canvas, x, y)
        });
        self.residual = residual;
        self.last = (x1, y1);
        ToolResponse::repaint()
    }

    fn on_release(&mut self, _event: PointerEvent, _ctx: &mut ToolCtx) -> ToolResponse {
        self.stroke = None;
        ToolResponse::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::document::{Document, DocumentId};

    fn grey_doc(v: u8) -> Document {
        let canvas = Canvas::from_rgba([v, v, v, 255].repeat(64 * 64), 64, 64);
        Document::from_canvas(DocumentId(1), canvas, None)
    }

    fn stroke(tool: &mut DodgeBurnTool, doc: &mut Document, pts: &[(f32, f32)]) {
        {
            let mut ctx = ToolCtx::new(doc, [0, 0, 0, 255], [255; 4], 1.0, 0.0, 0.0);
            let (x, y) = pts[0];
            tool.on_press(PointerEvent::new(x, y), &mut ctx);
            let mut prev = PointerEvent::new(x, y);
            for &(x, y) in &pts[1..] {
                let e = PointerEvent::new(x, y);
                tool.on_drag(e, &prev, &mut ctx);
                prev = e;
            }
            tool.on_release(prev, &mut ctx);
        }
        doc.canvas.end_stroke();
    }

    fn at(doc: &Document, x: u32, y: u32) -> u8 {
        doc.canvas.layer_stack.layers[0].tiles.get_pixel(x, y).0
    }

    fn tool(burn: bool) -> DodgeBurnTool {
        let mut t = if burn {
            DodgeBurnTool::burn()
        } else {
            DodgeBurnTool::dodge()
        };
        t.size = 20.0;
        t.hardness = 1.0;
        t
    }

    #[test]
    fn a_dab_goes_exposure_of_the_way_like_photoshops() {
        // Photoshop 2020, Midtones, Protect Tones, 50 %: grey 128 -> 145
        // (Dodge) or 111 (Burn) in one dab.
        let (mut d, mut b) = (grey_doc(128), grey_doc(128));
        stroke(&mut tool(false), &mut d, &[(32.0, 32.0)]);
        stroke(&mut tool(true), &mut b, &[(32.0, 32.0)]);
        assert!((144..=146).contains(&at(&d, 32, 32)), "{}", at(&d, 32, 32));
        assert!((110..=112).contains(&at(&b, 32, 32)), "{}", at(&b, 32, 32));
    }

    #[test]
    fn one_stroke_stops_at_the_curve_and_the_next_goes_on() {
        // Photoshop: a dense 50 % stroke reaches 163 and no further; a second
        // stroke goes on from there.
        let mut d = grey_doc(128);
        let mut t = tool(false);
        t.spacing = 0.01;
        let line: Vec<(f32, f32)> = (4..60).map(|x| (x as f32, 32.0)).collect();
        stroke(&mut t, &mut d, &line);
        let once = at(&d, 32, 32);
        assert!((161..=164).contains(&once), "{once}");
        stroke(&mut t, &mut d, &line);
        assert!(at(&d, 32, 32) > once + 15, "{}", at(&d, 32, 32));
    }

    #[test]
    fn the_range_picks_the_tones() {
        // Dodge Highlights leaves black black; Burn Shadows leaves white white.
        let mut black = grey_doc(0);
        let mut t = tool(false);
        t.range = ToneRange::Highlights;
        t.exposure = 1.0;
        stroke(&mut t, &mut black, &[(32.0, 32.0)]);
        assert_eq!(at(&black, 32, 32), 0);
        let mut white = grey_doc(255);
        let mut t = tool(true);
        t.range = ToneRange::Shadows;
        t.exposure = 1.0;
        stroke(&mut t, &mut white, &[(32.0, 32.0)]);
        assert_eq!(at(&white, 32, 32), 255);
    }

    #[test]
    fn protect_tones_keeps_the_colour_like_photoshops() {
        // Photoshop 2020, Highlights, 100 %, Protect Tones: bright skin keeps
        // its colour instead of going white, and a red's hue holds.
        let t = targets(false, ToneRange::Highlights, true);
        let px = |rgb: [u8; 3]| protected(rgb, &t, 1.0).map(|v| q8(v / 255.0));
        let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(&b).all(|(x, y)| x.abs_diff(*y) <= 2);
        assert!(
            near(px([240, 187, 154]), [255, 195, 157]),
            "{:?}",
            px([240, 187, 154])
        );
        assert!(
            near(px([178, 89, 36]), [255, 117, 34]),
            "{:?}",
            px([178, 89, 36])
        );
    }

    #[test]
    fn alt_turns_dodge_into_burn_for_the_stroke() {
        let mut d = grey_doc(128);
        let mut t = tool(false);
        let mut ctx = ToolCtx::new(&mut d, [0, 0, 0, 255], [255; 4], 1.0, 0.0, 0.0);
        let mut e = PointerEvent::new(32.0, 32.0);
        e.alt = true;
        t.on_press(e, &mut ctx);
        t.on_release(e, &mut ctx);
        drop(ctx);
        d.canvas.end_stroke();
        assert!((110..=112).contains(&at(&d, 32, 32)), "{}", at(&d, 32, 32));
        assert!(!t.burn, "the tool stays Dodge");
    }

    #[test]
    fn transparent_pixels_stay_untouched() {
        let canvas = Canvas::from_rgba([10, 20, 30, 0].repeat(64 * 64), 64, 64);
        let mut doc = Document::from_canvas(DocumentId(1), canvas, None);
        stroke(&mut tool(false), &mut doc, &[(32.0, 32.0)]);
        assert_eq!(
            doc.canvas.layer_stack.layers[0].tiles.get_pixel(32, 32),
            (10, 20, 30, 0)
        );
    }

    #[test]
    fn dodge_never_darkens_and_burn_never_lightens() {
        for burn in [false, true] {
            for range in [
                ToneRange::Shadows,
                ToneRange::Midtones,
                ToneRange::Highlights,
            ] {
                for protect in [false, true] {
                    let t = targets(burn, range, protect);
                    for (v, &f) in t.iter().enumerate() {
                        let x = v as f32;
                        assert!(
                            if burn { f <= x + 1.0 } else { f >= x - 1.0 },
                            "{burn} {range:?} {protect}: {v} -> {f}"
                        );
                    }
                }
            }
        }
    }
}
