use super::brush::{BrushSettings, BrushTool};
use super::{PointerEvent, Tool, ToolCtx, ToolResponse};

pub struct PencilTool {
    pub size: f32,
    pub opacity: f32,
    pub color: [u8; 4],
    /// The Brush's Mode (see `ToolManager::pencil_follows_brush`).
    pub mode: super::brush::BrushMode,
    cached_settings: BrushSettings,
    /// Per-stroke coverage buffer, as Brush and Eraser use: overlapping
    /// dabs of one stroke no longer build up past Opacity (14/11: the
    /// Pencil all but ignored it).
    stroke: Option<super::brush::StrokeBuffer>,
    /// Shift+click lines and Shift-drag axis.
    lines: super::brush::ShiftLines,
    /// Where the last drag left the (axis-held) stroke.
    last: (f32, f32),
}

impl Default for PencilTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PencilTool {
    pub fn new() -> Self {
        Self {
            size: 5.0,
            opacity: 1.0,
            color: [0, 0, 0, 255],
            mode: super::brush::BrushMode::Normal,
            cached_settings: BrushSettings {
                size: 5.0,
                hardness: 1.0,
                opacity: 1.0,
                color: [0, 0, 0, 255],
                spacing: 0.1,
                is_eraser: false,
                smoothing: 0.0,
                flow: 1.0,
                aliased: true,
                ..BrushSettings::default()
            },
            stroke: None,
            lines: super::brush::ShiftLines::default(),
            last: (0.0, 0.0),
        }
    }

    fn sync_cache(&mut self) {
        self.cached_settings.size = self.size;
        self.cached_settings.hardness = 1.0;
        self.cached_settings.opacity = self.opacity;
        self.cached_settings.color = self.color;
        self.cached_settings.spacing = 0.25;
        self.cached_settings.aliased = true;
        self.cached_settings.mode = self.mode;
    }

    /// Dab centres for the stretch from `(x0, y0)` to `(x1, y1)`, the way
    /// Photoshop's Pencil lays them: one per pixel along the stretch's longer
    /// axis, on that axis's pixel centres, after the pixel the stretch starts
    /// in up to the one it ends in. A 1 px line is then one pixel per column
    /// (or row), joined at the corners, and stretches that follow on from
    /// each other never stamp a pixel twice.
    fn pixel_steps(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<(f32, f32)> {
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
            return Vec::new();
        }
        let (dx, dy) = (x1 - x0, y1 - y0);
        let x_major = dx.abs() >= dy.abs();
        let (a0, a1, b0, da, db) = if x_major {
            (x0, x1, y0, dx, dy)
        } else {
            (y0, y1, x0, dy, dx)
        };
        let (c0, c1) = (a0.floor() as i64, a1.floor() as i64);
        if c0 == c1 || da.abs() < 1e-6 {
            return Vec::new();
        }
        let step = if c1 > c0 { 1 } else { -1 };
        // A huge jump is cut to a few thousand dabs, like the brush walker.
        let n = (c1 - c0).abs().min(16384);
        (1..=n)
            .map(|k| {
                let a = (c0 + k * step) as f32 + 0.5;
                let b = b0 + db * (a - a0) / da;
                if x_major { (a, b) } else { (b, a) }
            })
            .collect()
    }
}

impl Tool for PencilTool {
    fn id(&self) -> &'static str {
        "pencil"
    }
    fn name(&self) -> &str {
        "Pencil"
    }
    fn shortcut(&self) -> Option<char> {
        Some('B')
    }
    fn tool_id(&self) -> crate::tools::ToolId {
        crate::tools::ToolId::Pencil
    }
    fn paints(&self) -> bool {
        true
    }
    fn cursor_size(&self) -> f32 {
        self.size * 0.5
    }

    fn on_press(&mut self, event: PointerEvent, ctx: &mut ToolCtx) -> ToolResponse {
        ctx.canvas_mut().begin_stroke("Pencil Stroke");
        self.sync_cache();
        let (cx, cy) = (event.canvas_x, event.canvas_y);
        // Shift+click: the stroke starts as a line from the last one's end.
        let line_from = self.lines.press(ctx.document.id, cx, cy, event.shift);
        let (sx, sy) = line_from.unwrap_or((cx, cy));
        self.last = (cx, cy);
        let alpha_plane = matches!(
            ctx.canvas().channels.view,
            crate::core::channels::ChannelView::Alpha(_)
        );
        self.stroke = if alpha_plane {
            None
        } else {
            super::brush::StrokeBuffer::begin(ctx.canvas())
        };
        if let Some(stroke) = self.stroke.as_mut() {
            BrushTool::paint_cpu_dab_stroked(
                &self.cached_settings,
                ctx.canvas_mut(),
                sx,
                sy,
                stroke,
            );
        } else if alpha_plane {
            BrushTool::paint_cpu_dab(&self.cached_settings, ctx.canvas_mut(), sx, sy);
        }
        if line_from.is_some() {
            self.pixel_segment(ctx, sx, sy, cx, cy);
        }
        ToolResponse::repaint()
    }

    fn on_drag(
        &mut self,
        event: PointerEvent,
        _prev: &PointerEvent,
        ctx: &mut ToolCtx,
    ) -> ToolResponse {
        self.sync_cache();
        let (x0, y0) = self.last;
        let (x1, y1) = self.lines.constrain(event.canvas_x, event.canvas_y);
        self.last = (x1, y1);
        self.pixel_segment(ctx, x0, y0, x1, y1);
        ToolResponse::repaint()
    }

    fn on_release(&mut self, _event: PointerEvent, ctx: &mut ToolCtx) -> ToolResponse {
        let (x, y) = self.last;
        self.lines.release(ctx.document.id, x, y);
        self.stroke = None;
        ToolResponse::none()
    }
}

impl PencilTool {
    /// Pencil dabs from `(x0, y0)` to `(x1, y1)`, pixel by pixel.
    fn pixel_segment(&mut self, ctx: &mut ToolCtx, x0: f32, y0: f32, x1: f32, y1: f32) {
        let alpha_plane = matches!(
            ctx.canvas().channels.view,
            crate::core::channels::ChannelView::Alpha(_)
        );
        for (x, y) in Self::pixel_steps(x0, y0, x1, y1) {
            if let Some(stroke) = self.stroke.as_mut() {
                BrushTool::paint_cpu_dab_stroked(
                    &self.cached_settings,
                    ctx.canvas_mut(),
                    x,
                    y,
                    stroke,
                );
            } else if alpha_plane {
                BrushTool::paint_cpu_dab(&self.cached_settings, ctx.canvas_mut(), x, y);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::canvas::Canvas;
    use crate::core::document::{Document, DocumentId};

    /// A Pencil stroke at 50 % Opacity on white leaves mid grey where its
    /// dabs overlap, not near black (14/11).
    #[test]
    fn pencil_keeps_to_its_opacity() {
        let px = [255u8; 64 * 64 * 4].to_vec();
        let mut doc = Document::from_canvas(DocumentId(1), Canvas::from_rgba(px, 64, 64), None);
        let mut pencil = PencilTool::new();
        pencil.size = 8.0;
        pencil.opacity = 0.5;
        let mut ctx = ToolCtx::new(&mut doc, [0, 0, 0, 255], [255; 4], 1.0, 0.0, 0.0);
        let mut prev = PointerEvent::new(10.0, 32.0);
        pencil.on_press(prev, &mut ctx);
        for x in 11..54 {
            let ev = PointerEvent::new(x as f32, 32.0);
            pencil.on_drag(ev, &prev, &mut ctx);
            prev = ev;
        }
        pencil.on_release(prev, &mut ctx);
        let (r, ..) = doc
            .canvas
            .layer_stack
            .active_layer()
            .tiles
            .get_pixel(32, 32);
        assert!((118..=138).contains(&r), "got {r}");
    }
}
