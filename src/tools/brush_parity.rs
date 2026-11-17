//! Brush parity with Photoshop: paints the cases of
//! `tests/fixtures/brush_pts/cases.json` (and Dodge / Burn's, over a base
//! picture, of `dodge_burn.json`) with iAi's own tools (through the
//! `ToolManager`, as the app does) and measures them against Photoshop's PNGs
//! of the same cases in `tests/fixtures/brush_pts/ps/` (painted by
//! `scripts/brush_parity/run_ps.ps1`).
//!
//! `cargo test --lib brush_parity_report -- --ignored --nocapture` prints one
//! line per case and writes iAi's PNGs and difference maps (grey = equal,
//! darker = iAi lighter) to `target/brush_parity/`.

use super::{PointerEvent, ToolCtx, ToolId, ToolManager};
use crate::core::canvas::Canvas;
use crate::core::document::{Document, DocumentId};
use std::path::PathBuf;

/// One stroke case: what Photoshop and iAi are both asked to paint.
#[derive(Clone, Debug)]
pub(crate) struct Case {
    pub id: String,
    pub w: u32,
    pub h: u32,
    pub bits: u32,
    pub tool: String,
    pub size: f32,
    /// Percent, like the options bar.
    pub hardness: f32,
    pub spacing: f32,
    pub opacity: f32,
    pub flow: f32,
    pub color: [u8; 3],
    /// Each stroke is one press, drags along its points, one release.
    pub strokes: Vec<Vec<(f32, f32)>>,
    /// Dodge / Burn: the picture they work on (a PNG beside the cases), and
    /// their Range (`shadows` / `midtones` / `highlights`), Exposure (%) and
    /// Protect Tones.
    pub base: Option<String>,
    pub range: String,
    pub exposure: f32,
    pub protect: bool,
}

pub(crate) fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/brush_pts")
}

/// The brush cases (`cases.json`).
pub(crate) fn load_cases() -> Vec<Case> {
    load_cases_from("cases.json")
}

/// The cases of one file of `tests/fixtures/brush_pts`.
pub(crate) fn load_cases_from(file: &str) -> Vec<Case> {
    let text = std::fs::read_to_string(fixtures().join(file)).expect("cases file");
    let json: serde_json::Value = serde_json::from_str(&text).expect("cases file parses");
    let num = |c: &serde_json::Value, k: &str, d: f64| c[k].as_f64().unwrap_or(d) as f32;
    json["cases"]
        .as_array()
        .expect("cases array")
        .iter()
        .map(|c| {
            let color = c["color"]
                .as_array()
                .map(|a| {
                    let ch = |i: usize| a.get(i).and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                    [ch(0), ch(1), ch(2)]
                })
                .unwrap_or([0, 0, 0]);
            let strokes = c["strokes"]
                .as_array()
                .expect("strokes")
                .iter()
                .map(|s| {
                    s.as_array()
                        .expect("points")
                        .iter()
                        .map(|p| {
                            let p = p.as_array().expect("point");
                            (
                                p[0].as_f64().expect("x") as f32,
                                p[1].as_f64().expect("y") as f32,
                            )
                        })
                        .collect()
                })
                .collect();
            Case {
                id: c["id"].as_str().expect("id").to_string(),
                w: c["w"].as_u64().expect("w") as u32,
                h: c["h"].as_u64().expect("h") as u32,
                bits: c["bits"].as_u64().unwrap_or(8) as u32,
                tool: c["tool"].as_str().unwrap_or("brush").to_string(),
                size: num(c, "size", 10.0),
                hardness: num(c, "hardness", 100.0),
                spacing: num(c, "spacing", 25.0),
                opacity: num(c, "opacity", 100.0),
                flow: num(c, "flow", 100.0),
                color,
                strokes,
                base: c["base"].as_str().map(str::to_string),
                range: c["range"].as_str().unwrap_or("midtones").to_string(),
                exposure: num(c, "exposure", 50.0),
                protect: c["protect"].as_bool().unwrap_or(true),
            }
        })
        .collect()
}

/// A base picture beside the cases, straight RGBA bytes.
fn base_picture(name: &str, w: u32, h: u32) -> Vec<u8> {
    let img = image::open(fixtures().join(name))
        .expect("base picture")
        .to_rgba8();
    assert_eq!((img.width(), img.height()), (w, h), "{name}");
    img.into_raw()
}

/// Paint `case` on a transparent layer the way the app would (press, a drag
/// event every pixel or so, release, end of stroke) and return its pixels as
/// straight RGBA in 0..1.
pub(crate) fn paint(case: &Case) -> Vec<[f32; 4]> {
    let (w, h) = (case.w, case.h);
    // Dodge / Burn work on their base picture; the brushes on a clear layer.
    let (canvas, idx) = match &case.base {
        Some(name) => (Canvas::from_rgba(base_picture(name, w, h), w, h), 0),
        None => {
            let mut canvas = Canvas::new_blank(w, h);
            let idx = canvas.layer_stack.add_layer(w, h);
            (canvas, idx)
        }
    };
    let mut doc = Document::from_canvas(DocumentId(1), canvas, None);
    doc.canvas.layer_stack.active_idx = idx;
    let mut tools = ToolManager::new();
    tools.select(match case.tool.as_str() {
        "pencil" => ToolId::Pencil,
        "dodge" => ToolId::Dodge,
        "burn" => ToolId::Burn,
        _ => ToolId::Brush,
    });
    if matches!(case.tool.as_str(), "dodge" | "burn") {
        let t = tools.dodge_burn_mut();
        t.size = case.size;
        t.hardness = case.hardness / 100.0;
        t.spacing = case.spacing / 100.0;
        t.exposure = case.exposure / 100.0;
        t.protect_tones = case.protect;
        t.range = crate::tools::dodge_burn::ToneRange::from_u8(match case.range.as_str() {
            "shadows" => 0,
            "highlights" => 2,
            _ => 1,
        });
    }
    let [r, g, b] = case.color;
    let fg = [r, g, b, 255];
    {
        let s = &mut tools.brush_mut().settings;
        s.size = case.size;
        s.hardness = case.hardness / 100.0;
        s.spacing = case.spacing / 100.0;
        s.opacity = case.opacity / 100.0;
        s.flow = case.flow / 100.0;
        s.smoothing = 0.0;
        s.color = fg;
    }
    for stroke in &case.strokes {
        let Some(&first) = stroke.first() else {
            continue;
        };
        {
            let mut ctx = ToolCtx::new(&mut doc, fg, [255; 4], 1.0, 0.0, 0.0);
            tools.on_press(PointerEvent::new(first.0, first.1), &mut ctx);
            let mut last = first;
            for &p in &stroke[1..] {
                let len = ((p.0 - last.0).powi(2) + (p.1 - last.1).powi(2)).sqrt();
                let n = len.ceil().max(1.0) as usize;
                for k in 1..=n {
                    let f = k as f32 / n as f32;
                    let x = last.0 + (p.0 - last.0) * f;
                    let y = last.1 + (p.1 - last.1) * f;
                    tools.on_drag(PointerEvent::new(x, y), &mut ctx);
                }
                last = p;
            }
            tools.on_release(PointerEvent::new(last.0, last.1), &mut ctx);
        }
        doc.canvas.end_stroke();
    }
    let layer = &doc.canvas.layer_stack.layers[idx];
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b, a) = layer.tiles.get_pixel(x, y);
            out.push([
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
                a as f32 / 255.0,
            ]);
        }
    }
    out
}

/// Photoshop's PNG of a case, straight RGBA in 0..1 (8 or 16 bit).
pub(crate) fn photoshop(case: &Case) -> Option<Vec<[f32; 4]>> {
    let path = fixtures().join("ps").join(format!("{}.png", case.id));
    let img = image::open(path).ok()?.to_rgba32f();
    if img.width() != case.w || img.height() != case.h {
        return None;
    }
    Some(img.pixels().map(|p| p.0).collect())
}

/// How far iAi's paint is from Photoshop's for one case.
#[derive(Clone, Debug, Default)]
pub(crate) struct Score {
    /// Alpha difference in 0..255 units.
    pub mean: f32,
    pub p99: f32,
    pub max: f32,
    /// iAi's total coverage over Photoshop's (1 = same amount of paint).
    pub mass: f32,
    /// Centre of the paint, iAi minus Photoshop, px.
    pub shift: (f32, f32),
}

pub(crate) fn score(case: &Case, iai: &[[f32; 4]], ps: &[[f32; 4]]) -> Score {
    if let Some(name) = &case.base {
        return score_on_base(name, case, iai, ps);
    }
    let w = case.w as usize;
    let mut diffs: Vec<f32> = Vec::with_capacity(iai.len());
    let (mut sa, mut sb) = (0.0f64, 0.0f64);
    let (mut ax, mut ay, mut bx, mut by) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut touched = 0usize;
    let mut sum = 0.0f64;
    for (i, (a, b)) in iai.iter().zip(ps).enumerate() {
        let (x, y) = ((i % w) as f64 + 0.5, (i / w) as f64 + 0.5);
        let (ia, pa) = (a[3] as f64, b[3] as f64);
        sa += ia;
        sb += pa;
        ax += ia * x;
        ay += ia * y;
        bx += pa * x;
        by += pa * y;
        let d = (a[3] - b[3]).abs() * 255.0;
        if a[3] > 0.0 || b[3] > 0.0 {
            touched += 1;
            sum += d as f64;
        }
        diffs.push(d);
    }
    diffs.sort_by(|a, b| a.total_cmp(b));
    let p99 = diffs[(diffs.len() as f32 * 0.99) as usize..]
        .first()
        .copied()
        .unwrap_or(0.0);
    Score {
        mean: (sum / touched.max(1) as f64) as f32,
        p99,
        max: diffs.last().copied().unwrap_or(0.0),
        mass: if sb > 0.0 { (sa / sb) as f32 } else { 0.0 },
        shift: if sa > 0.0 && sb > 0.0 {
            ((ax / sa - bx / sb) as f32, (ay / sa - by / sb) as f32)
        } else {
            (0.0, 0.0)
        },
    }
}

/// Dodge / Burn: the colour difference over every channel of every pixel,
/// and (as `mass`) how much iAi changed the picture over how much Photoshop
/// did.
fn score_on_base(name: &str, case: &Case, iai: &[[f32; 4]], ps: &[[f32; 4]]) -> Score {
    let base = base_picture(name, case.w, case.h);
    let mut diffs: Vec<f32> = Vec::with_capacity(iai.len() * 3);
    let (mut moved_iai, mut moved_ps) = (0.0f64, 0.0f64);
    for ((a, b), o) in iai.iter().zip(ps).zip(base.chunks_exact(4)) {
        for c in 0..3 {
            let orig = o[c] as f32 / 255.0;
            diffs.push((a[c] - b[c]).abs() * 255.0);
            moved_iai += (a[c] - orig).abs() as f64;
            moved_ps += (b[c] - orig).abs() as f64;
        }
    }
    let mean = diffs.iter().map(|&d| d as f64).sum::<f64>() / diffs.len().max(1) as f64;
    diffs.sort_by(|a, b| a.total_cmp(b));
    let p99 = diffs[(diffs.len() as f32 * 0.99) as usize..]
        .first()
        .copied()
        .unwrap_or(0.0);
    Score {
        mean: mean as f32,
        p99,
        max: diffs.last().copied().unwrap_or(0.0),
        mass: if moved_ps > 0.0 {
            (moved_iai / moved_ps) as f32
        } else {
            1.0
        },
        shift: (0.0, 0.0),
    }
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: Vec<u8>) {
    if let Some(img) = image::RgbaImage::from_raw(w, h, rgba) {
        let _ = img.save(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Brush paints these cases like Photoshop: on average within a few
    /// levels of 255, and the same amount of paint within 3 %.
    #[test]
    fn brush_paints_like_photoshop() {
        let wanted = [
            "dab_50_h0",
            "dab_50_h25",
            "dab_50_h50",
            "dab_50_h75",
            "dab_50_h100",
            "dab_300_h0",
            "dab_300_h50",
            "dab_50_h0_sub",
            "line_50_h0_sp1",
            "line_50_h0_sp25",
            "line_50_h100_sp25",
            "line_50_h0_op100_fl30",
            "line_50_h0_op50_fl30",
            "zigzag_40_h50_op50",
            "cross_40_h100_op50",
        ];
        let cases = load_cases();
        for id in wanted {
            let case = cases.iter().find(|c| c.id == id).expect("case");
            let ps = photoshop(case).expect("Photoshop PNG");
            let s = score(case, &paint(case), &ps);
            assert!(s.mean <= 4.0, "{id}: mean {:.2}", s.mean);
            assert!((s.mass - 1.0).abs() <= 0.03, "{id}: mass {:.3}", s.mass);
        }
    }

    /// Speed of big tips on a 24 MP picture (plan §3.4, E1–E5): a stroke
    /// dragged 1500 px in one second at 60 frames, the paint work of each
    /// frame timed as the app does it.
    ///
    /// `CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
    ///  cargo test --release --lib --target-dir target/probe probe_big_tips
    ///  -- --ignored --nocapture`
    #[test]
    #[ignore = "probe: prints timings"]
    fn probe_big_tips() {
        use crate::tools::brush::{BrushSettings, BrushTool, StrokeBuffer};
        let (w, h) = (6000u32, 4000u32);
        let picture = || Canvas::from_rgba(vec![180u8; (w * h * 4) as usize], w, h);
        println!("size hard spacing target   frames  total ms  worst frame ms  per dab ms");
        for (size, hardness) in [
            (100.0f32, 0.0f32),
            (500.0, 0.0),
            (1500.0, 0.0),
            (3000.0, 0.0),
            (500.0, 1.0),
            (1500.0, 1.0),
        ] {
            for spacing in [0.1f32, 0.25] {
                for mask in [false, true] {
                    let settings = BrushSettings {
                        size,
                        hardness,
                        spacing,
                        flow: 0.5,
                        opacity: 1.0,
                        ..Default::default()
                    };
                    let mut canvas = picture();
                    if mask {
                        let i = canvas.layer_stack.active_idx;
                        canvas.layer_stack.layers[i].add_mask(true);
                        canvas.layer_stack.layers[i].paint_target =
                            crate::core::layer::PaintTarget::Mask;
                    }
                    let Some(mut stroke) = StrokeBuffer::begin(&canvas) else {
                        continue;
                    };
                    let (mut x, y) = (2250.0f32, 2000.0f32);
                    let t0 = std::time::Instant::now();
                    BrushTool::paint_cpu_dab_stroked(&settings, &mut canvas, x, y, &mut stroke);
                    stroke.residual = settings.dab_spacing();
                    let mut worst = 0.0f64;
                    for _ in 0..60 {
                        let f = std::time::Instant::now();
                        let nx = x + 25.0;
                        BrushTool::paint_cpu_stroke_segment_stroked(
                            &settings,
                            &mut canvas,
                            x,
                            y,
                            nx,
                            y,
                            &mut stroke,
                        );
                        x = nx;
                        worst = worst.max(f.elapsed().as_secs_f64() * 1e3);
                    }
                    let total = t0.elapsed().as_secs_f64() * 1e3;
                    let dabs = 1.0 + 1500.0 / settings.dab_spacing();
                    println!(
                        "{size:>5} {hardness:>4} {spacing:>6} {:<7} {:>6} {total:>9.0} {worst:>14.1} {:>11.2}",
                        if mask { "mask" } else { "pixels" },
                        60,
                        total / dabs as f64
                    );
                }
            }
        }
    }

    /// Dodge over a 6000 × 4000 picture: the same 1500 px drag in 60 frames
    /// as `probe_big_tips`, through the tool's own pointer events.
    #[test]
    #[ignore = "probe: prints timings"]
    fn probe_dodge_big_tips() {
        use crate::tools::Tool;
        use crate::tools::dodge_burn::DodgeBurnTool;
        let (w, h) = (6000u32, 4000u32);
        println!("size hard protect  total ms  worst frame ms");
        for (size, hardness) in [
            (100.0f32, 0.0f32),
            (500.0, 0.0),
            (1500.0, 0.0),
            (3000.0, 0.0),
            (500.0, 1.0),
            (1500.0, 1.0),
        ] {
            for protect in [true, false] {
                let canvas =
                    Canvas::from_rgba([200u8, 150, 120, 255].repeat((w * h) as usize), w, h);
                let mut doc = Document::from_canvas(DocumentId(1), canvas, None);
                let mut tool = DodgeBurnTool::dodge();
                tool.size = size;
                tool.hardness = hardness;
                tool.protect_tones = protect;
                let mut ctx = ToolCtx::new(&mut doc, [0, 0, 0, 255], [255; 4], 1.0, 0.0, 0.0);
                let (mut x, y) = (2250.0f32, 2000.0f32);
                let t0 = std::time::Instant::now();
                let mut prev = PointerEvent::new(x, y);
                tool.on_press(prev, &mut ctx);
                let mut worst = 0.0f64;
                for _ in 0..60 {
                    let f = std::time::Instant::now();
                    x += 25.0;
                    let e = PointerEvent::new(x, y);
                    tool.on_drag(e, &prev, &mut ctx);
                    prev = e;
                    worst = worst.max(f.elapsed().as_secs_f64() * 1e3);
                }
                tool.on_release(prev, &mut ctx);
                let total = t0.elapsed().as_secs_f64() * 1e3;
                println!("{size:>5} {hardness:>4} {protect:>7} {total:>9.0} {worst:>14.1}");
            }
        }
    }

    #[test]
    #[ignore = "report: iAi against Photoshop, case by case"]
    fn brush_parity_report() {
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/brush_parity");
        let _ = std::fs::create_dir_all(&out);
        println!(
            "{:<26} {:>7} {:>7} {:>7} {:>6} {:>13}",
            "case", "mean", "p99", "max", "mass", "shift px"
        );
        let mut all = Vec::new();
        let cases = load_cases()
            .into_iter()
            .chain(load_cases_from("dodge_burn.json"));
        for case in cases {
            let t = std::time::Instant::now();
            let iai = paint(&case);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            write_png(
                &out.join(format!("{}.png", case.id)),
                case.w,
                case.h,
                iai.iter().flat_map(|p| p.map(to8)).collect(),
            );
            let Some(ps) = photoshop(&case) else {
                println!("{:<26} (no Photoshop PNG)", case.id);
                continue;
            };
            let s = score(&case, &iai, &ps);
            let diff: Vec<u8> = iai
                .iter()
                .zip(&ps)
                .flat_map(|(a, b)| {
                    // Alpha for the brushes, colour for Dodge / Burn.
                    let d = if case.base.is_some() {
                        (a[0] + a[1] + a[2] - b[0] - b[1] - b[2]) / 3.0
                    } else {
                        a[3] - b[3]
                    };
                    let g = to8(0.5 - d * 4.0);
                    [g, g, g, 255]
                })
                .collect();
            write_png(
                &out.join(format!("{}_diff.png", case.id)),
                case.w,
                case.h,
                diff,
            );
            // Photoshop painted a 16-bit case at 16 bits; iAi paints 8.
            let depth = if case.bits == 16 { " 16-bit" } else { "" };
            println!(
                "{:<26} {:>7.2} {:>7.1} {:>7.1} {:>6.3} {:>6.2},{:>6.2}  ({ms:.1} ms){depth}",
                case.id, s.mean, s.p99, s.max, s.mass, s.shift.0, s.shift.1
            );
            all.push(s);
        }
        let n = all.len().max(1) as f32;
        println!(
            "average mean {:.2}, worst max {:.1}",
            all.iter().map(|s| s.mean).sum::<f32>() / n,
            all.iter().map(|s| s.max).fold(0.0, f32::max)
        );
    }
}
