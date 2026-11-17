use super::brush::BrushSettings;
use super::{PointerEvent, Tool, ToolCtx, ToolResponse};
use crate::core::canvas::Canvas;

pub struct EraserTool {
    pub size: f32,
    pub hardness: f32,
    pub opacity: f32,
    pub spacing: f32,
    pub flow: f32,
    pub smoothing: f32,
    /// Index into `BRUSH_PRESETS` (eraser shares the brush tip presets) or
    /// `PRESET_CUSTOM` when the tip was hand-edited.
    pub preset_idx: usize,
    pub bg_color: [u8; 4],
    pub cached_settings: BrushSettings,
    /// Per-stroke coverage buffer (PS opacity/flow model); Some while the
    /// pointer is down.
    stroke: Option<crate::tools::brush::StrokeBuffer>,
    /// Distance walker for alpha-channel plane strokes, which do not use the
    /// layer-tile stroke buffer.
    stroke_residual: f32,
    /// Smoothed pointer (see `Smoother`), like the Brush's.
    smoother: crate::tools::brush::Smoother,
    /// Shift+click lines and Shift-drag axis.
    lines: crate::tools::brush::ShiftLines,
}

impl Default for EraserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl EraserTool {
    pub fn new() -> Self {
        Self {
            size: 20.0,
            hardness: 0.5,
            opacity: 1.0,
            spacing: 0.25,
            flow: 1.0,
            smoothing: 0.0,
            preset_idx: crate::tools::brush::PRESET_CUSTOM,
            bg_color: [255, 255, 255, 255],
            cached_settings: BrushSettings {
                size: 20.0,
                hardness: 0.8,
                opacity: 1.0,
                color: [255, 255, 255, 255],
                spacing: 0.25,
                is_eraser: true,
                smoothing: 0.0,
                flow: 1.0,
                ..BrushSettings::default()
            },
            stroke: None,
            stroke_residual: 0.0,
            smoother: crate::tools::brush::Smoother::default(),
            lines: crate::tools::brush::ShiftLines::default(),
        }
    }

    /// Apply a brush tip preset (eraser reuses the brush presets: hardness +
    /// spacing + flow). Size/opacity are kept.
    pub fn apply_preset(&mut self, idx: usize) {
        if let Some(p) = crate::tools::brush::BRUSH_PRESETS.get(idx) {
            self.hardness = p.hardness;
            self.spacing = p.spacing;
            self.flow = p.flow;
            self.preset_idx = idx;
        }
    }

    fn sync_cache(&mut self, canvas: &Canvas) {
        self.cached_settings.size = self.size;
        self.cached_settings.hardness = self.hardness;
        self.cached_settings.opacity = self.opacity;
        self.cached_settings.spacing = self.spacing;
        self.cached_settings.flow = self.flow;
        self.cached_settings.smoothing = self.smoothing;

        // Photoshop: on the Background, or on a layer whose transparent
        // pixels are locked, the Eraser paints the background colour instead
        // of making pixels transparent. (A layer locked whole is not painted
        // at all: `StrokeBuffer::begin` refuses it.)
        let layer = &canvas.layer_stack.layers[canvas.layer_stack.active_idx];
        let on_pixels = layer.paint_target == crate::core::layer::PaintTarget::Pixels;
        let to_background = layer.is_background || (on_pixels && layer.lock_alpha);
        self.cached_settings.is_eraser = !to_background;
        // Normal (alpha) erasing never reads the colour, but the
        // Channels-panel write gate erases a single channel by painting
        // the background colour's luma into it, and a mask is erased
        // toward the background colour's luma.
        self.cached_settings.color = self.bg_color;
    }
}

impl Tool for EraserTool {
    fn id(&self) -> &'static str {
        "eraser"
    }
    fn name(&self) -> &str {
        "Eraser"
    }
    fn shortcut(&self) -> Option<char> {
        Some('E')
    }
    fn tool_id(&self) -> crate::tools::ToolId {
        crate::tools::ToolId::Eraser
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
        ctx.canvas_mut().begin_stroke("Eraser Stroke");
        self.sync_cache(ctx.canvas());
        let (cx, cy) = (event.canvas_x, event.canvas_y);
        // Shift+click: the stroke starts as a line from the last one's end.
        let line_from = self.lines.press(ctx.document.id, cx, cy, event.shift);
        let (sx, sy) = line_from.unwrap_or((cx, cy));
        self.smoother.begin(cx, cy);
        let alpha_plane = matches!(
            ctx.canvas().channels.view,
            crate::core::channels::ChannelView::Alpha(_)
        );
        if alpha_plane {
            self.stroke = None;
            crate::tools::brush::BrushTool::paint_cpu_dab(
                &self.cached_settings,
                ctx.canvas_mut(),
                sx,
                sy,
            );
            self.stroke_residual = self.cached_settings.dab_spacing();
        } else {
            self.stroke = crate::tools::brush::StrokeBuffer::begin(ctx.canvas());
        }
        if let Some(stroke) = self.stroke.as_mut() {
            crate::tools::brush::BrushTool::paint_cpu_dab_stroked(
                &self.cached_settings,
                ctx.canvas_mut(),
                sx,
                sy,
                stroke,
            );
            stroke.residual = self.cached_settings.dab_spacing();
        }
        if line_from.is_some() {
            self.erase_segment(ctx, sx, sy, cx, cy);
        }
        ToolResponse::repaint()
    }

    fn on_drag(
        &mut self,
        event: PointerEvent,
        _prev: &PointerEvent,
        ctx: &mut ToolCtx,
    ) -> ToolResponse {
        self.sync_cache(ctx.canvas());
        let (x, y) = self.lines.constrain(event.canvas_x, event.canvas_y);
        let ((x0, y0), (x1, y1)) = self.smoother.follow(x, y, self.smoothing, ctx.zoom);
        self.erase_segment(ctx, x0, y0, x1, y1);
        ToolResponse::repaint()
    }

    fn on_frame(&mut self, ctx: &mut ToolCtx) -> ToolResponse {
        // Smoothing catches up while the pointer rests; the answer asks for
        // more frames while it has not.
        let caught = self.smoother.catch_up(self.smoothing);
        if let Some(((x0, y0), (x1, y1))) = caught {
            self.sync_cache(ctx.canvas());
            self.erase_segment(ctx, x0, y0, x1, y1);
        }
        if caught.is_some() || self.smoother.pending(self.smoothing) {
            ToolResponse::repaint()
        } else {
            ToolResponse::none()
        }
    }

    fn on_release(&mut self, event: PointerEvent, ctx: &mut ToolCtx) -> ToolResponse {
        // The stroke ends where the pointer let go, smoothing or not.
        let (x, y) = self.lines.constrain(event.canvas_x, event.canvas_y);
        if let Some(((x0, y0), (x1, y1))) = self.smoother.finish(x, y) {
            self.sync_cache(ctx.canvas());
            self.erase_segment(ctx, x0, y0, x1, y1);
        }
        self.lines.release(ctx.document.id, x, y);
        self.stroke = None;
        self.stroke_residual = 0.0;
        ToolResponse::none()
    }
}

impl EraserTool {
    fn erase_segment(&mut self, ctx: &mut ToolCtx, x0: f32, y0: f32, x1: f32, y1: f32) {
        if let Some(stroke) = self.stroke.as_mut() {
            crate::tools::brush::BrushTool::paint_cpu_stroke_segment_stroked(
                &self.cached_settings,
                ctx.canvas_mut(),
                x0,
                y0,
                x1,
                y1,
                stroke,
            );
        } else if matches!(
            ctx.canvas().channels.view,
            crate::core::channels::ChannelView::Alpha(_)
        ) {
            crate::tools::brush::BrushTool::paint_cpu_stroke_segment(
                &self.cached_settings,
                ctx.canvas_mut(),
                x0,
                y0,
                x1,
                y1,
                &mut self.stroke_residual,
            );
        }
    }
}
