// Text tool flow: create / edit a text layer, drive the egui editing overlay,
// and rasterize the text into the layer on commit. The tool itself
// (tools/text_tool.rs) is a stub; all the real work happens here at App level.

use super::render::CanvasEvent;
use super::state::{App, TextEditState, TextFontPreviewSession, TextFontPreviewState};
use crate::core::canvas::text_layer::rasterize_into_layer;
use crate::core::command::LayerStructureCommand;
use crate::core::layer::{Layer, LayerType};
use crate::core::text::TextData;
use crate::core::text::rasterize_placed;
use crate::core::tile::TileMap;
use crate::tools::ToolId;

fn text_edit_origin_for_layer(td: &TextData, layer: &Layer) -> (i32, i32) {
    // The layer is the type as it was last written (its raster's size): the
    // origin is its offset less the placement delta, learnt from the layout
    // alone. Drawing the whole text to find it took 0.2–0.9 s on a large
    // heading (flight recorder 26/10–12/11).
    if let Some(((w, h), delta)) = crate::core::text::placed_size_and_delta(td)
        && (layer.width, layer.height) == (w, h)
    {
        return (
            layer.offset.0.saturating_sub(delta.0),
            layer.offset.1.saturating_sub(delta.1),
        );
    }
    let Some((placed, delta)) = rasterize_placed(td) else {
        return layer.offset;
    };
    let placed_tiles = TileMap::from_rgba(&placed.rgba, placed.width, placed.height);
    let Some((placed_min_x, placed_min_y, _, _)) = placed_tiles.content_bounds() else {
        return layer.offset;
    };
    let Some((layer_min_x, layer_min_y, _, _)) = layer.tiles.content_bounds() else {
        return layer.offset;
    };

    (
        layer
            .offset
            .0
            .saturating_add(layer_min_x)
            .saturating_sub(delta.0)
            .saturating_sub(placed_min_x),
        layer
            .offset
            .1
            .saturating_add(layer_min_y)
            .saturating_sub(delta.1)
            .saturating_sub(placed_min_y),
    )
}

/// Origin for re-fonting into `layer` that keeps the block's ink centre fixed
/// on the canvas, so text centred inside a frame stays centred when the new
/// font changes its width/height. Works for rotated/stretched text too: both
/// centres are measured on the *placed* raster and `delta` maps back to the
/// layer offset. Returns `None` when either side has no ink (blank), so the
/// caller can fall back to the plain corner anchor.
fn compute_refont_origin(layer: &Layer, new_td: &TextData) -> Option<(i32, i32)> {
    let old_bounds = layer.tiles.content_bounds()?;
    let (raster, delta) = rasterize_placed(new_td)?;
    let new_tiles = TileMap::from_rgba(&raster.rgba, raster.width, raster.height);
    let new_bounds = new_tiles.content_bounds()?;
    Some(refont_origin_from_bounds(
        layer.offset,
        old_bounds,
        new_bounds,
        delta,
    ))
}

/// Pure centre-anchor arithmetic: the origin at which a new placed raster whose
/// ink spans `new_bounds` must be drawn so its ink centre coincides with the
/// ink centre of the current tiles (`old_bounds` at `offset`). `delta` is the
/// placed raster's bbox offset, which `rasterize_into_layer` re-adds to the
/// origin.
fn refont_origin_from_bounds(
    offset: (i32, i32),
    old_bounds: (i32, i32, i32, i32),
    new_bounds: (i32, i32, i32, i32),
    delta: (i32, i32),
) -> (i32, i32) {
    let old_cx = offset.0 as f32 + (old_bounds.0 + old_bounds.2) as f32 * 0.5;
    let old_cy = offset.1 as f32 + (old_bounds.1 + old_bounds.3) as f32 * 0.5;
    let new_cx = (new_bounds.0 + new_bounds.2) as f32 * 0.5;
    let new_cy = (new_bounds.1 + new_bounds.3) as f32 * 0.5;
    (
        (old_cx - new_cx).round() as i32 - delta.0,
        (old_cy - new_cy).round() as i32 - delta.1,
    )
}

/// A type layer's own style, to tell when it changed (an undo, a restyle).
fn type_style_fingerprint(td: &TextData) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    td.font_px.to_bits().hash(&mut h);
    td.font_family.hash(&mut h);
    td.color.hash(&mut h);
    td.align.hash(&mut h);
    td.bold.hash(&mut h);
    td.italic.hash(&mut h);
    td.underline.hash(&mut h);
    td.line_height.to_bits().hash(&mut h);
    td.tracking_px.to_bits().hash(&mut h);
    td.opacity.to_bits().hash(&mut h);
    h.finish()
}

/// Where `td`'s anchor (see `TextEditState::anchor`) lies from its origin on
/// the page: the anchor in its upright raster, stretched, mirrored and turned
/// as `rasterize_placed` places the raster. `None` for text on a path.
fn anchor_offset(td: &TextData) -> Option<(f32, f32)> {
    let (ax, ay) = crate::core::text::anchor_in_raster(td)?;
    let stretch = if td.stretch_x.is_finite() && td.stretch_x > 0.001 {
        td.stretch_x
    } else {
        1.0
    };
    let x = if td.flip_x {
        -ax * stretch
    } else {
        ax * stretch
    };
    let y = if td.flip_y { -ay } else { ay };
    let angle = if td.rotation_deg.is_finite() {
        td.rotation_deg.to_radians()
    } else {
        0.0
    };
    let (s, c) = angle.sin_cos();
    Some((c * x - s * y, s * x + c * y))
}

/// Default font size that reads well on a canvas of the given dimensions
/// (~4% of the smaller side): 12pt-style constants are invisible on print-res
/// documents and huge on thumbnails.
fn auto_font_px(cw: u32, ch: u32) -> f32 {
    ((cw.min(ch) as f32) * 0.04).round().clamp(10.0, 256.0)
}

/// Human-readable label for a font family in a list (family name, plus the
/// style when it isn't the plain Regular face).
fn font_display_label(fam: &crate::core::text::TextFontFamily) -> String {
    let name = fam.name();
    let style = fam.style_name();
    if style.eq_ignore_ascii_case("Regular") {
        name.to_string()
    } else {
        format!("{name} {style}")
    }
}

/// Rebuild `session.glyph_styles` after the buffer changed from `old` to the
/// new buffer, using a prefix/suffix diff to find the inserted/removed span.
/// Inserted characters take the pending style (if the insertion is at the
/// pending caret), else inherit the neighbouring character, else `base`.
fn resync_glyph_styles(
    session: &mut TextEditState,
    old: &str,
    base: crate::core::text::GlyphStyle,
) {
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = session.buffer.chars().collect();

    // Nothing per-character to track and no pending style → stay uniform.
    if session.glyph_styles.is_empty() && session.pending_style.is_none() {
        return;
    }

    let mut p = 0usize;
    while p < old_chars.len() && p < new_chars.len() && old_chars[p] == new_chars[p] {
        p += 1;
    }
    let (mut so, mut sn) = (old_chars.len(), new_chars.len());
    while so > p && sn > p && old_chars[so - 1] == new_chars[sn - 1] {
        so -= 1;
        sn -= 1;
    }

    // The pending style is consumed by an insertion at its caret and survives
    // pure deletions (caret shifted with the removed span) — a Backspace
    // before typing must not lose it. Any other edit discards it.
    let inserted = sn > p;
    let pending = match session.pending_style.take() {
        Some((caret, style)) if inserted && caret == p => Some((caret, style)),
        Some((caret, style)) if !inserted && so > p => {
            let shifted = if caret >= so {
                caret - (so - p)
            } else {
                caret.min(p)
            };
            session.pending_style = Some((shifted, style));
            None
        }
        _ => None,
    };

    // Materialize old styles to full length so we can splice.
    let old_len = old_chars.len();
    let old_styles: Vec<crate::core::text::GlyphStyle> = if session.glyph_styles.len() == old_len {
        session.glyph_styles.clone()
    } else {
        vec![base.clone(); old_len]
    };

    let seed = || -> crate::core::text::GlyphStyle {
        if let Some((caret, style)) = pending.as_ref()
            && *caret == p
        {
            return style.clone();
        }
        if p > 0 {
            old_styles[p - 1].clone()
        } else if so < old_len {
            old_styles[so].clone()
        } else {
            base.clone()
        }
    };

    let mut out = Vec::with_capacity(new_chars.len());
    out.extend_from_slice(&old_styles[..p]);
    for _ in p..sn {
        out.push(seed());
    }
    out.extend_from_slice(&old_styles[so..old_len]);
    session.glyph_styles = out;
}

impl App {
    /// Convert the Text layer at `idx` into editable vector Path layers ("Convert
    /// Text to Curves"). Each distinct fill colour becomes one Path layer: the
    /// first reuses the text layer's slot IN PLACE (keeping its id / stacking order
    /// / opacity / blend / group / page — mirroring `convert_shape_to_path`), any
    /// extra colours are inserted just above, inheriting the text layer's
    /// compositing. Every glyph outline lands exactly where the rasterized text
    /// sat. One structural undo restores the Text layer. Returns true when it
    /// converted.
    ///
    /// The existing layer mask is preserved on the in-place Path layer, just like
    /// Shape-to-Curves; the vector opacity carries the layer's own
    /// `Layer::opacity`, while `TextData::opacity` is folded into each object's
    /// `VectorStyle::opacity` by [`crate::core::text::text_to_curves`].
    pub fn text_to_curves(&mut self, idx: usize) -> bool {
        self.layer_to_curves(idx)
    }

    /// `vector.to_curves` on the layer at `idx`.
    pub(crate) fn layer_to_curves(&mut self, idx: usize) -> bool {
        let stack = &self.docs.active().canvas.layer_stack;
        let Some(id) = stack.layers.get(idx).map(|l| l.id) else {
            return false;
        };
        self.run_with("vector.to_curves", serde_json::json!({"layer": id}))
            .is_ok()
    }

    /// What the Type options bar shows: while letters are selected, their
    /// style (the first one's size, font and colour; B / I / U lit when all
    /// of them have it), else the type's own.
    pub(crate) fn text_bar_style(&self) -> crate::ui::TextBarStyle {
        let base = crate::ui::TextBarStyle {
            font_px: self.edit.text_font_px,
            font_family: self.edit.text_font_family.clone(),
            color: self.edit.text_color,
            bold: self.edit.text_bold,
            italic: self.edit.text_italic,
            underline: self.edit.text_underline,
            tracking_px: self.edit.text_tracking_px,
            line_height: self.edit.text_line_height,
        };
        let Some(s) = self.edit.text_edit.as_ref() else {
            return base;
        };
        let n = s.buffer.chars().count();
        let Some(range) = s
            .selection
            .clone()
            .filter(|r| r.start < r.end && r.end <= n)
        else {
            return base;
        };
        if s.glyph_styles.len() != n {
            return base;
        }
        let Some(picked) = s.glyph_styles.get(range.clone()) else {
            return base;
        };
        let Some(first) = picked.first() else {
            return base;
        };
        crate::ui::TextBarStyle {
            font_px: first.font_px,
            font_family: first.font_family.clone(),
            color: first.color,
            bold: picked.iter().all(|g| g.bold),
            italic: picked.iter().all(|g| g.italic),
            underline: picked.iter().all(|g| g.underline),
            tracking_px: first.tracking_px.unwrap_or(base.tracking_px),
            line_height: first.line_height.unwrap_or(base.line_height),
        }
    }

    /// Tracking / line height set while editing: with letters selected, as
    /// theirs (true); with none, every letter's own goes (the whole text
    /// takes what the caller sets: false). Not editing: false.
    pub(crate) fn apply_text_spacing_to_selection(
        &mut self,
        tracking_px: Option<f32>,
        line_height: Option<f32>,
    ) -> bool {
        let selection = self
            .edit
            .text_edit
            .as_ref()
            .and_then(|s| s.selection.clone())
            .or_else(|| {
                self.text_overlay_selection()
                    .filter(|range| range.start < range.end)
            });
        let base = crate::core::text::GlyphStyle {
            color: self.edit.text_color,
            font_px: self.edit.text_font_px,
            font_family: self.edit.text_font_family.clone(),
            bold: self.edit.text_bold,
            italic: self.edit.text_italic,
            underline: self.edit.text_underline,
            tracking_px: None,
            line_height: None,
        };
        let Some(s) = self.edit.text_edit.as_mut() else {
            return false;
        };
        let n = s.buffer.chars().count();
        match selection {
            Some(range) if range.start < range.end && range.start < n => {
                if s.glyph_styles.len() != n {
                    s.glyph_styles = (0..n)
                        .map(|i| s.glyph_styles.get(i).cloned().unwrap_or(base.clone()))
                        .collect();
                }
                for style in s
                    .glyph_styles
                    .iter_mut()
                    .take(range.end.min(n))
                    .skip(range.start)
                {
                    if tracking_px.is_some() {
                        style.tracking_px = tracking_px;
                    }
                    if line_height.is_some() {
                        style.line_height = line_height;
                    }
                }
                if let Some(w) = &self.win.window {
                    w.request_redraw();
                }
                true
            }
            _ => {
                for style in &mut s.glyph_styles {
                    if tracking_px.is_some() {
                        style.tracking_px = None;
                    }
                    if line_height.is_some() {
                        style.line_height = None;
                    }
                }
                false
            }
        }
    }

    /// True when the active layer is an editable Text layer.
    pub fn active_layer_is_text(&self) -> bool {
        let canvas = &self.docs.active().canvas;
        canvas
            .layer_stack
            .layers
            .get(canvas.layer_stack.active_idx)
            .is_some_and(|l| matches!(l.layer_type, LayerType::Text(_)))
    }

    /// Fill a Text layer's colour (Move-tool Alt/Ctrl+Delete): recolour the
    /// whole type to `color`, overriding any per-glyph colours, then re-rasterize
    /// in place so the layer stays an editable Text layer. Returns true when it
    /// recoloured; recorded as one undo step.
    pub fn recolor_text_layer(&mut self, idx: usize, color: [u8; 4]) -> bool {
        let (cw, ch) = {
            let d = self.docs.active();
            (d.canvas.width, d.canvas.height)
        };
        let (mut new_td, origin) = {
            let canvas = &self.docs.active().canvas;
            let Some(layer) = canvas.layer_stack.layers.get(idx) else {
                return false;
            };
            if layer.locked && !layer.is_background {
                return false;
            }
            let LayerType::Text(td) = &layer.layer_type else {
                return false;
            };
            // Already that colour everywhere → nothing to do.
            if td.color == color && td.glyph_styles.iter().all(|g| g.color == color) {
                return false;
            }
            let origin = text_edit_origin_for_layer(td, layer);
            (td.clone(), origin)
        };
        new_td.color = color;
        for g in &mut new_td.glyph_styles {
            g.color = color;
        }

        let canvas = &mut self.docs.active_mut().canvas;
        let before =
            LayerStructureCommand::capture_before("Fill Text Colour", &canvas.layer_stack, cw, ch);
        rasterize_into_layer(canvas, idx, origin, new_td);
        canvas.layer_revision += 1;
        let mut cmd = before;
        cmd.capture_after(&canvas.layer_stack, cw, ch);
        canvas.record(Box::new(cmd));

        self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
        true
    }

    /// Distinct fonts currently used across the active document's Text layers
    /// (each layer's base font plus any per-glyph overrides), returned as
    /// `(storage_name, display_label)` sorted by display label. Feeds the batch
    /// "Change font" dialog's *from* list so the user picks among fonts that are
    /// actually present.
    pub fn document_text_fonts_in_use(&self) -> Vec<(String, String)> {
        use std::collections::BTreeMap;
        let canvas = &self.docs.active().canvas;
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for layer in &canvas.layer_stack.layers {
            if let LayerType::Text(td) = &layer.layer_type {
                map.entry(td.font_family.storage_name())
                    .or_insert_with(|| font_display_label(&td.font_family));
                for g in &td.glyph_styles {
                    map.entry(g.font_family.storage_name())
                        .or_insert_with(|| font_display_label(&g.font_family));
                }
            }
        }
        let mut out: Vec<(String, String)> = map.into_iter().collect();
        out.sort_by_key(|a| a.1.to_lowercase());
        out
    }

    /// Convenience: batch-change only the font. `from == None` re-fonts every
    /// text layer to `to`; `from == Some(storage)` replaces only that font.
    /// Thin wrapper over [`Self::change_document_text_format`].
    pub fn change_document_text_font(
        &mut self,
        from: Option<String>,
        to: crate::core::text::TextFontFamily,
    ) -> usize {
        self.change_document_text_format(crate::ui::intent::TextBatchFormat {
            from_font: from,
            set_font: Some(to),
            ..Default::default()
        })
    }

    /// Batch-apply a text-formatting change across every editable Text layer in
    /// the active document (all pages). Each `TextBatchFormat` field is optional
    /// — `None` leaves that attribute alone. Font swaps honour `from_font`
    /// (all fonts, or one specific font); colour/size/bold/italic/underline are
    /// set on the base style and every per-character override; case rewrites the
    /// content (UPPER / lower / Title). Locked (non-background) layers are
    /// skipped.
    ///
    /// Each changed layer is re-rasterized in place, keeping its ink centred on
    /// its old position (frame-safe) via [`compute_refont_origin`]. The whole
    /// batch is a single undo step. Returns how many layers changed.
    pub fn change_document_text_format(
        &mut self,
        fmt: crate::ui::intent::TextBatchFormat,
    ) -> usize {
        if fmt.is_noop() {
            return 0;
        }
        if let Some(to) = &fmt.set_font {
            self.ensure_text_font_registered(to);
        }
        let (cw, ch) = {
            let d = self.docs.active();
            (d.canvas.width, d.canvas.height)
        };
        let font_matches = |fam: &crate::core::text::TextFontFamily| match &fmt.from_font {
            None => true,
            Some(want) => &fam.storage_name() == want,
        };

        // Collect the layers that actually change (index, anchor origin, new
        // text) before mutating, so one structural command spans the batch.
        let mut edits: Vec<(usize, (i32, i32), TextData)> = Vec::new();
        {
            let canvas = &self.docs.active().canvas;
            for (idx, layer) in canvas.layer_stack.layers.iter().enumerate() {
                if layer.locked && !layer.is_background {
                    continue;
                }
                let LayerType::Text(td) = &layer.layer_type else {
                    continue;
                };
                let mut new_td = td.clone();
                let mut changed = false;

                // Case first: it rewrites content (and may change its length), so
                // per-glyph styles must be re-spread before any per-glyph setter.
                if let Some(target) = fmt.set_case {
                    let chars: Vec<char> = new_td.content.chars().collect();
                    let n = chars.len();
                    if n > 0 {
                        let (new_chars, style_src) =
                            crate::core::text::transform_text_case(&chars, 0..n, target);
                        if new_chars != chars {
                            if new_td.glyph_styles.len() == n {
                                new_td.glyph_styles = style_src
                                    .iter()
                                    .map(|&i| new_td.glyph_styles[i].clone())
                                    .collect();
                            }
                            new_td.content = new_chars.into_iter().collect();
                            changed = true;
                        }
                    }
                }

                if let Some(to) = &fmt.set_font {
                    let to_storage = to.storage_name();
                    if font_matches(&new_td.font_family)
                        && new_td.font_family.storage_name() != to_storage
                    {
                        new_td.font_family = to.clone();
                        changed = true;
                    }
                    for g in &mut new_td.glyph_styles {
                        if font_matches(&g.font_family)
                            && g.font_family.storage_name() != to_storage
                        {
                            g.font_family = to.clone();
                            changed = true;
                        }
                    }
                }
                if let Some(c) = fmt.set_color {
                    if new_td.color != c {
                        new_td.color = c;
                        changed = true;
                    }
                    for g in &mut new_td.glyph_styles {
                        if g.color != c {
                            g.color = c;
                            changed = true;
                        }
                    }
                }
                if let Some(px) = fmt.set_size {
                    let px = px.clamp(
                        crate::core::text::MIN_EDITABLE_FONT_PX,
                        crate::core::text::MAX_EDITABLE_FONT_PX,
                    );
                    if new_td.font_px != px {
                        new_td.font_px = px;
                        changed = true;
                    }
                    for g in &mut new_td.glyph_styles {
                        if g.font_px != px {
                            g.font_px = px;
                            changed = true;
                        }
                    }
                }
                if let Some(v) = fmt.set_bold {
                    if new_td.bold != v {
                        new_td.bold = v;
                        changed = true;
                    }
                    for g in &mut new_td.glyph_styles {
                        if g.bold != v {
                            g.bold = v;
                            changed = true;
                        }
                    }
                }
                if let Some(v) = fmt.set_italic {
                    if new_td.italic != v {
                        new_td.italic = v;
                        changed = true;
                    }
                    for g in &mut new_td.glyph_styles {
                        if g.italic != v {
                            g.italic = v;
                            changed = true;
                        }
                    }
                }
                if let Some(v) = fmt.set_underline {
                    if new_td.underline != v {
                        new_td.underline = v;
                        changed = true;
                    }
                    for g in &mut new_td.glyph_styles {
                        if g.underline != v {
                            g.underline = v;
                            changed = true;
                        }
                    }
                }

                if changed {
                    // Drop per-glyph overrides that now equal the base style.
                    new_td.compact_glyph_styles();
                    // Centre-anchor keeps blocks (e.g. text in a frame) in place.
                    let origin = compute_refont_origin(layer, &new_td)
                        .unwrap_or_else(|| text_edit_origin_for_layer(td, layer));
                    edits.push((idx, origin, new_td));
                }
            }
        }

        if edits.is_empty() {
            return 0;
        }

        let canvas = &mut self.docs.active_mut().canvas;
        let before =
            LayerStructureCommand::capture_before("Format Text", &canvas.layer_stack, cw, ch);
        let count = edits.len();
        for (idx, origin, new_td) in edits {
            rasterize_into_layer(canvas, idx, origin, new_td);
        }
        canvas.layer_revision += 1;
        let mut cmd = before;
        cmd.capture_after(&canvas.layer_stack, cw, ch);
        canvas.record(Box::new(cmd));

        self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
        count
    }

    /// Build a `TextData` from the current app-level text style + the given
    /// content and per-glyph overrides (compacted when uniform).
    fn text_data_from_state(
        &self,
        content: String,
        glyph_styles: Vec<crate::core::text::GlyphStyle>,
    ) -> TextData {
        let mut td = TextData {
            content,
            font_px: self.edit.text_font_px,
            color: self.edit.text_color,
            font_family: self.edit.text_font_family.clone(),
            align: self.edit.text_align,
            line_height: self.edit.text_line_height,
            bold: self.edit.text_bold,
            italic: self.edit.text_italic,
            underline: self.edit.text_underline,
            tracking_px: self.edit.text_tracking_px,
            opacity: self.edit.text_opacity,
            stretch_x: 1.0,
            rotation_deg: 0.0,
            flip_x: false,
            flip_y: false,
            glyph_styles,
            path: None,
            fill: None,
            box_width: None,
            box_height: None,
        };
        // Keep glyph_styles either empty (uniform) or exactly one per char.
        if !td.glyph_styles.is_empty() {
            td.ensure_glyph_styles();
            td.compact_glyph_styles();
        }
        td
    }

    /// With the Type tool in hand and a type layer picked, not being edited:
    /// that layer's id, its style taken as the working style the first time
    /// (and again when the layer changes), so the options bar shows it and
    /// new type starts from it. `None` otherwise (and for a locked layer).
    pub(crate) fn adopt_picked_type_style(&mut self) -> Option<u32> {
        let picked = (self.edit.tools.active_id() == ToolId::Text && self.edit.text_edit.is_none())
            .then(|| {
                let stack = &self.docs.active().canvas.layer_stack;
                if stack.is_none_picked() {
                    return None;
                }
                let layer = stack.layers.get(stack.active_idx)?;
                let LayerType::Text(td) = &layer.layer_type else {
                    return None;
                };
                (!(layer.locked && !layer.is_background)).then(|| (layer.id, td.clone()))
            })
            .flatten();
        let Some((id, td)) = picked else {
            self.edit.text_style_layer = None;
            return None;
        };
        let key = (id, type_style_fingerprint(&td));
        if self.edit.text_style_layer != Some(key) {
            self.edit.text_style_layer = Some(key);
            self.ensure_text_font_registered(&td.font_family);
            self.edit.text_font_px = td.font_px;
            self.edit.text_font_px_auto = false;
            self.edit.text_font_family = td.font_family.clone();
            self.edit.text_color = td.color;
            self.edit.text_align = td.align;
            self.edit.text_bold = td.bold;
            self.edit.text_italic = td.italic;
            self.edit.text_underline = td.underline;
            self.edit.text_line_height = td.line_height;
            self.edit.text_tracking_px = td.tracking_px;
            self.edit.text_opacity = td.opacity;
        }
        Some(id)
    }

    /// Write the options bar's change into the picked type layer (see
    /// [`Self::adopt_picked_type_style`]) once the mouse is let go: what the
    /// bar now says, over the whole layer, its anchor kept still; one step.
    pub(crate) fn finish_picked_type_restyle(&mut self) {
        let Some(id) = self.edit.text_restyle_pending else {
            return;
        };
        if self.win.egui_ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        self.edit.text_restyle_pending = None;
        // Still the layer the bar shows (not another picked meanwhile).
        if self.edit.text_style_layer.map(|(picked, _)| picked) != Some(id) {
            return;
        }
        let doc = self.docs.active().id;
        let (td, origin) = {
            let canvas = &self.docs.active().canvas;
            let Some(layer) = canvas.layer_stack.layers.iter().find(|l| l.id == id) else {
                return;
            };
            let LayerType::Text(td) = &layer.layer_type else {
                return;
            };
            (td.clone(), text_edit_origin_for_layer(td, layer))
        };
        let mut new_td = td.clone();
        let base = |t: &TextData| crate::core::text::GlyphStyle {
            color: t.color,
            font_px: t.font_px,
            font_family: t.font_family.clone(),
            bold: t.bold,
            italic: t.italic,
            underline: t.underline,
            tracking_px: None,
            line_height: None,
        };
        let old_base = base(&td);
        new_td.font_px = self.edit.text_font_px;
        new_td.font_family = self.edit.text_font_family.clone();
        new_td.color = self.edit.text_color;
        new_td.bold = self.edit.text_bold;
        new_td.italic = self.edit.text_italic;
        new_td.underline = self.edit.text_underline;
        new_td.align = self.edit.text_align;
        new_td.line_height = self.edit.text_line_height;
        new_td.tracking_px = self.edit.text_tracking_px;
        let new_base = base(&new_td);
        // What changed goes to every letter, those styled on their own too.
        for g in &mut new_td.glyph_styles {
            if new_base.font_px != old_base.font_px {
                g.font_px = new_base.font_px;
            }
            if new_base.font_family != old_base.font_family {
                g.font_family = new_base.font_family.clone();
            }
            if new_base.color != old_base.color {
                g.color = new_base.color;
            }
            if new_base.bold != old_base.bold {
                g.bold = new_base.bold;
            }
            if new_base.italic != old_base.italic {
                g.italic = new_base.italic;
            }
            if new_base.underline != old_base.underline {
                g.underline = new_base.underline;
            }
            // The whole text's spacing: no letter keeps its own.
            if new_td.tracking_px != td.tracking_px {
                g.tracking_px = None;
            }
            if new_td.line_height != td.line_height {
                g.line_height = None;
            }
        }
        new_td.compact_glyph_styles();
        if new_td == td {
            return;
        }
        // The anchor (first baseline, at the alignment's side) stays still.
        let origin = match (anchor_offset(&td), anchor_offset(&new_td)) {
            (Some(old), Some(new)) => (
                (origin.0 as f32 + old.0 - new.0).round() as i32,
                (origin.1 as f32 + old.1 - new.1).round() as i32,
            ),
            _ => origin,
        };
        let args = serde_json::json!({
            "doc": doc.0,
            "layer": id,
            "type": iai_engine::cmds::text::TypeSpec::of(&new_td),
            "origin": [origin.0, origin.1],
            "label": "Format Text",
        });
        if let Err(error) = self.run_with("text.set", args) {
            crate::diag::note("text", &format!("restyle refused: {error:?}"));
        }
    }

    /// The type of the session open now, as it would be written.
    fn session_text_data(&self) -> Option<TextData> {
        let s = self.edit.text_edit.as_ref()?;
        let mut td = self.text_data_from_state(s.buffer.clone(), s.glyph_styles.clone());
        td.rotation_deg = s.rotation_deg;
        td.stretch_x = s.stretch_x;
        td.flip_x = s.flip_x;
        td.flip_y = s.flip_y;
        td.path = s.path.clone();
        td.box_width = s.box_width;
        td.box_height = s.box_width.and(s.box_height);
        Some(td)
    }

    /// Keep the open type's anchor still while it is typed, resized,
    /// realigned or restyled, as Photoshop's point type: `origin` follows it.
    /// Opened again, the anchor is first learnt from `origin`.
    pub(crate) fn sync_text_anchor(&mut self) {
        let Some(td) = self.session_text_data() else {
            return;
        };
        let Some((dx, dy)) = anchor_offset(&td) else {
            return;
        };
        let Some(s) = self.edit.text_edit.as_mut() else {
            return;
        };
        match s.anchor {
            None => s.anchor = Some((s.origin.0 as f32 + dx, s.origin.1 as f32 + dy)),
            Some((x, y)) => {
                let origin = ((x - dx).round() as i32, (y - dy).round() as i32);
                if origin != s.origin {
                    s.origin = origin;
                    if let Some(w) = &self.win.window {
                        w.request_redraw();
                    }
                }
            }
        }
    }

    /// Hit-test editable text layers from top to bottom at a canvas-space point.
    pub fn text_layer_at(&self, cx: f32, cy: f32) -> Option<usize> {
        // Some 8 screen points round the letters at any zoom: 8 page pixels
        // were one point on a large photo seen small, so a click just beside
        // the words made new type instead of editing them.
        const TEXT_HIT_PAD_POINTS: f32 = 8.0;
        let zoom = self.edit.view.zoom.max(0.01);
        let pad = (TEXT_HIT_PAD_POINTS / zoom).ceil().clamp(1.0, 100_000.0) as i32;

        let canvas = &self.docs.active().canvas;
        let x = cx.floor() as i32;
        let y = cy.floor() as i32;
        for (idx, layer) in canvas.layer_stack.layers.iter().enumerate().rev() {
            if !canvas.layer_stack.is_effectively_visible(idx)
                || !matches!(layer.layer_type, LayerType::Text(_))
            {
                continue;
            }
            let lx = x - layer.offset.0;
            let ly = y - layer.offset.1;

            // content_bounds is pixel-accurate, so the padded bbox test is the
            // whole hit-test (any opaque pixel lies inside the bbox).
            if let Some((min_x, min_y, max_x, max_y)) = layer.tiles.content_bounds()
                && lx >= min_x - pad
                && lx < max_x + pad
                && ly >= min_y - pad
                && ly < max_y + pad
            {
                return Some(idx);
            }
        }
        None
    }

    /// Click with the Type tool on the canvas. Under the strict modal lock an
    /// open session refuses the click (bell) — it ends only via Commit/Cancel
    /// (✓/✗, Esc, numpad Enter), so clicking away can't spawn or drop text.
    pub fn text_tool_click(&mut self, cx: f32, cy: f32) {
        if self.edit.text_edit.is_some() {
            self.deny_modal_action();
            return;
        }
        self.begin_new_text(cx, cy, None, None);
    }

    /// A paragraph box dragged out with the Type tool, corner to corner on
    /// the canvas: type that wraps in it (too narrow a drag: point type at
    /// its start, as a click).
    pub fn text_tool_box(&mut self, from: (f32, f32), to: (f32, f32)) {
        if self.edit.text_edit.is_some() {
            self.deny_modal_action();
            return;
        }
        let (x0, y0) = (from.0.min(to.0), from.1.min(to.1));
        let width = (from.0 - to.0).abs();
        if width < 4.0 {
            self.begin_new_text(from.0, from.1, None, None);
            return;
        }
        // A box at least a line tall keeps its height, as Photoshop's: the
        // lines that do not fit are hidden, with a mark on the box. A flat
        // drag gives a box that grows with its lines.
        let height = (from.1 - to.1).abs();
        let height = (height >= self.edit.text_font_px.max(4.0)).then_some(height);
        self.begin_new_text(x0, y0, Some(width), height);
    }

    fn begin_new_text(
        &mut self,
        cx: f32,
        cy: f32,
        box_width: Option<f32>,
        box_height: Option<f32>,
    ) {
        self.edit.text_font_preview = None;
        let (cw, ch) = {
            let d = self.docs.active();
            (d.canvas.width, d.canvas.height)
        };
        let doc_id = self.docs.active().id;
        let origin = (cx.round() as i32, cy.round() as i32);
        if self.edit.text_font_px_auto {
            // Until the user picks a size, adapt the default to the canvas so
            // the first click yields readable text on any document size.
            self.edit.text_font_px = auto_font_px(cw, ch);
        }

        let td = self.text_data_from_state(String::new(), Vec::new());
        let canvas = &mut self.docs.active_mut().canvas;
        let before = LayerStructureCommand::capture_before("Type", &canvas.layer_stack, cw, ch);
        let idx = canvas.layer_stack.add_layer(cw, ch);
        let layer = &mut canvas.layer_stack.layers[idx];
        layer.name = "Text".to_string();
        layer.offset = origin;
        let layer_id = layer.id;
        layer.layer_type = LayerType::Text(td);
        canvas.layer_revision += 1;

        self.edit.text_edit = Some(TextEditState {
            doc_id,
            layer_id,
            buffer: String::new(),
            origin,
            rotation_deg: 0.0,
            stretch_x: 1.0,
            flip_x: false,
            flip_y: false,
            path: None,
            is_new: true,
            orig: None,
            before_cmd: Some(before),
            glyph_styles: Vec::new(),
            selection: None,
            caret: Some(0),
            pending_style: None,
            // Where the click was: the first line's baseline sits on it
            // (a paragraph box's top-left corner).
            anchor: Some((cx, cy)),
            box_width,
            box_height,
        });
        self.sync_text_anchor();
        self.edit.text_focus_pending = true;

        self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }

    /// Re-open an existing text layer for editing (double-click).
    pub fn begin_edit_text_layer(&mut self, idx: usize) {
        // A locked type layer is not opened (its words could be changed
        // under the lock): it says so instead.
        let locked = self
            .docs
            .active()
            .canvas
            .layer_stack
            .layers
            .get(idx)
            .is_some_and(|l| l.locked && !l.is_background);
        if locked {
            self.shell.status_msg = "Layer chữ đang khóa — mở khóa rồi mới sửa chữ".to_string();
            if let Some(w) = &self.win.window {
                w.request_redraw();
            }
            return;
        }
        self.edit.text_font_preview = None;
        if self.edit.text_edit.is_some() {
            self.commit_text_edit();
        }
        let (cw, ch) = {
            let d = self.docs.active();
            (d.canvas.width, d.canvas.height)
        };
        let doc_id = self.docs.active().id;

        let (td, layer_id, origin) = {
            let canvas = &self.docs.active().canvas;
            if idx >= canvas.layer_stack.layers.len() {
                return;
            }
            let layer = &canvas.layer_stack.layers[idx];
            let LayerType::Text(td) = layer.layer_type.clone() else {
                return;
            };
            let layer_id = layer.id;
            let origin = text_edit_origin_for_layer(&td, layer);
            (td, layer_id, origin)
        };

        self.edit.text_font_px = td.font_px;
        // Re-editing adopts the layer's size as the working default.
        self.edit.text_font_px_auto = false;
        self.ensure_text_font_registered(&td.font_family);
        self.edit.text_font_family = td.font_family.clone();
        self.edit.text_align = td.align;
        self.edit.text_color = td.color;
        self.edit.text_bold = td.bold;
        self.edit.text_italic = td.italic;
        self.edit.text_underline = td.underline;
        self.edit.text_line_height = td.line_height;
        self.edit.text_tracking_px = td.tracking_px;
        self.edit.text_opacity = td.opacity;
        self.edit.fg_color = td.color;

        let canvas = &mut self.docs.active_mut().canvas;
        let before =
            LayerStructureCommand::capture_before("Edit Text", &canvas.layer_stack, cw, ch);
        // Blank placeholder while the overlay shows the live preview; keep
        // width/height in sync with the tiles (commit/cancel restore both).
        canvas.layer_stack.layers[idx].tiles = TileMap::new(cw, ch);
        canvas.layer_stack.layers[idx].width = cw;
        canvas.layer_stack.layers[idx].height = ch;
        canvas.layer_stack.active_idx = idx;
        canvas.layer_revision += 1;

        // Seed the edit buffer as canonical NFC so the on-screen caret lines up
        // with the fused glyphs and a later commit stays precomposed (see
        // `update_text_buffer`). Legacy text saved as decomposed diacritics is
        // repaired the moment it is re-opened.
        let (nfc_chars, style_src) = crate::core::text::normalize_nfc_with_style_src(&td.content);
        let buffer: String = nfc_chars.iter().collect();
        let glyph_styles = if td.glyph_styles.is_empty() {
            Vec::new()
        } else {
            style_src
                .iter()
                .map(|&i| td.glyph_style(i))
                .collect::<Vec<_>>()
        };
        let caret = Some(nfc_chars.len());

        self.edit.tools.select(ToolId::Text);
        self.edit.text_edit = Some(TextEditState {
            doc_id,
            layer_id,
            buffer,
            origin,
            rotation_deg: td.rotation_deg,
            stretch_x: td.stretch_x,
            flip_x: td.flip_x,
            flip_y: td.flip_y,
            path: td.path.clone(),
            is_new: false,
            glyph_styles,
            selection: None,
            caret,
            pending_style: None,
            anchor: None,
            box_width: td.box_width,
            box_height: td.box_height,
            orig: Some(td),
            before_cmd: Some(before),
        });
        self.edit.text_focus_pending = true;

        self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }

    /// Live buffer update from the editing overlay (no rasterization yet — the
    /// overlay shows the text until commit). Keeps per-character styles aligned
    /// with the edited string.
    pub fn update_text_buffer(&mut self, new: String) {
        // Store canonical (precomposed) text: decomposed diacritics would render
        // with detached accents (ab_glyph has no mark positioning) and the caret
        // would count the invisible combining marks. NFC also matches how most
        // Vietnamese IMEs deliver text.
        let new = crate::core::text::normalize_nfc(&new);
        let base = crate::core::text::GlyphStyle {
            color: self.edit.text_color,
            font_px: self.edit.text_font_px,
            font_family: self.edit.text_font_family.clone(),
            bold: self.edit.text_bold,
            italic: self.edit.text_italic,
            underline: self.edit.text_underline,
            tracking_px: None,
            line_height: None,
        };
        let Some(s) = &mut self.edit.text_edit else {
            return;
        };
        if s.buffer == new {
            return;
        }
        let old = std::mem::replace(&mut s.buffer, new);
        resync_glyph_styles(s, &old, base);
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }

    /// Current selection (sorted char range) in the editing overlay, if any.
    fn text_overlay_selection(&self) -> Option<std::ops::Range<usize>> {
        let te_id = egui::Id::new("text_overlay_te");
        let st = egui::TextEdit::load_state(&self.win.egui_ctx, te_id)?;
        Some(st.cursor.char_range()?.as_sorted_char_range())
    }

    pub fn update_text_cursor_cache(
        &mut self,
        selection: Option<std::ops::Range<usize>>,
        caret: Option<usize>,
    ) {
        let Some(s) = &mut self.edit.text_edit else {
            return;
        };
        let n = s.buffer.chars().count();
        s.selection = selection.and_then(|range| {
            let start = range.start.min(n);
            let end = range.end.min(n);
            (start < end).then_some(start..end)
        });
        s.caret = caret.map(|idx| idx.min(n)).or(s.caret).or(Some(n));
    }

    /// Resolve the case-switch target while editing: the selected char range
    /// (else the whole buffer), the buffer's chars, and whether a real selection
    /// was in play. `None` when there's nothing editable to recase.
    fn text_case_target(&self) -> Option<(Vec<char>, std::ops::Range<usize>, bool)> {
        let selection = self
            .edit
            .text_edit
            .as_ref()
            .and_then(|s| s.selection.clone())
            .or_else(|| {
                self.text_overlay_selection()
                    .filter(|range| range.start < range.end)
            });
        let session = self.edit.text_edit.as_ref()?;
        let chars: Vec<char> = session.buffer.chars().collect();
        let n = chars.len();
        if n == 0 {
            return None;
        }
        let range = match &selection {
            Some(r) => r.start.min(n)..r.end.min(n),
            None => 0..n,
        };
        if range.start >= range.end {
            return None;
        }
        Some((chars, range, selection.is_some()))
    }

    /// Recase `chars[range]` to `target` and write it back into the live edit
    /// session. Per-glyph styles stay aligned (re-spread across any case
    /// expansion like ß→SS), the selection is preserved so repeated presses keep
    /// working, and the change lands in the buffer (committed with the rest of
    /// the edit; no separate undo step).
    fn apply_text_case(
        &mut self,
        chars: Vec<char>,
        range: std::ops::Range<usize>,
        had_selection: bool,
        target: crate::core::text::TextCase,
    ) {
        let n = chars.len();
        let (new_chars, style_src) =
            crate::core::text::transform_text_case(&chars, range.clone(), target);
        if new_chars == chars {
            // Uncased text (digits/punctuation), or already that case.
            return;
        }
        // New selection end after any expansion; the tail length is unchanged.
        let new_end = new_chars.len().saturating_sub(n - range.end);
        let new_selection = had_selection.then_some(range.start..new_end);

        let Some(session) = self.edit.text_edit.as_mut() else {
            return;
        };
        // Re-spread aligned per-glyph styles across the (possibly expanded) span.
        let old_styles = std::mem::take(&mut session.glyph_styles);
        if old_styles.len() == n {
            session.glyph_styles = style_src.iter().map(|&i| old_styles[i].clone()).collect();
        }
        session.buffer = new_chars.into_iter().collect();
        session.selection = new_selection.clone();
        session.caret = Some(new_end);
        // The pending style is pinned to a caret index in the old buffer.
        session.pending_style = None;

        // Push the new caret/selection into egui's invisible TextEdit so the
        // visible selection survives and further presses keep working.
        let te_id = egui::Id::new("text_overlay_te");
        if let Some(mut state) = egui::TextEdit::load_state(&self.win.egui_ctx, te_id) {
            use egui::text::{CCursor, CCursorRange};
            let cursor_range = match &new_selection {
                Some(r) => CCursorRange::two(CCursor::new(r.start), CCursor::new(r.end)),
                None => CCursorRange::one(CCursor::new(new_end)),
            };
            state.cursor.set_char_range(Some(cursor_range));
            state.store(&self.win.egui_ctx, te_id);
        }

        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }

    /// Shift+F3 while editing: cycle the case of the selected text — or the
    /// whole buffer when nothing is selected — through lowercase → UPPERCASE →
    /// Title Case.
    pub fn text_cycle_case(&mut self) {
        let Some((chars, range, had_selection)) = self.text_case_target() else {
            return;
        };
        let span: String = chars[range.clone()].iter().collect();
        let target = crate::core::text::next_text_case(&span);
        self.apply_text_case(chars, range, had_selection, target);
    }

    /// Set an explicit case (lowercase / UPPERCASE / Title Case) on the selected
    /// text — or the whole buffer — while editing. Drives the Text panel's case
    /// buttons (the discoverable form of the Shift+F3 shortcut).
    pub fn text_set_case(&mut self, target: crate::core::text::TextCase) {
        let Some((chars, range, had_selection)) = self.text_case_target() else {
            return;
        };
        self.apply_text_case(chars, range, had_selection, target);
    }

    /// Apply a colour and/or size override to the current text selection while
    /// editing: true, and the caller leaves the text's own style alone (the
    /// letters not selected, the lines' measures, keep it — setting it too
    /// made every line as tall as the letters last enlarged, 17/11). With no
    /// selection it goes to the whole block (false): every letter styled on
    /// its own here, the rest through the new default the caller sets. Must
    /// be called BEFORE updating the app-level default so the
    /// materialization seed reflects the unselected characters' prior style.
    pub fn apply_text_style_to_selection(
        &mut self,
        color: Option<[u8; 4]>,
        font_px: Option<f32>,
        font_family: Option<crate::core::text::TextFontFamily>,
        bold: Option<bool>,
        italic: Option<bool>,
        underline: Option<bool>,
    ) -> bool {
        let base = crate::core::text::GlyphStyle {
            color: self.edit.text_color,
            font_px: self.edit.text_font_px,
            font_family: self.edit.text_font_family.clone(),
            bold: self.edit.text_bold,
            italic: self.edit.text_italic,
            underline: self.edit.text_underline,
            tracking_px: None,
            line_height: None,
        };
        let selection = self
            .edit
            .text_edit
            .as_ref()
            .and_then(|s| s.selection.as_ref().cloned())
            .or_else(|| {
                self.text_overlay_selection()
                    .filter(|range| range.start < range.end)
            });
        let Some(s) = &mut self.edit.text_edit else {
            return false;
        };
        let n = s.buffer.chars().count();
        let apply = |style: &mut crate::core::text::GlyphStyle| {
            if let Some(c) = color {
                style.color = c;
            }
            if let Some(px) = font_px {
                style.font_px = px.clamp(
                    crate::core::text::MIN_EDITABLE_FONT_PX,
                    crate::core::text::MAX_EDITABLE_FONT_PX,
                );
            }
            if let Some(family) = &font_family {
                style.font_family = family.clone();
            }
            if let Some(v) = bold {
                style.bold = v;
            }
            if let Some(v) = italic {
                style.italic = v;
            }
            if let Some(v) = underline {
                style.underline = v;
            }
        };
        let on_selection = match selection {
            Some(range) if range.start < range.end && range.start < n => {
                if s.glyph_styles.len() != n {
                    s.glyph_styles = (0..n)
                        .map(|i| s.glyph_styles.get(i).cloned().unwrap_or(base.clone()))
                        .collect();
                }
                for i in range.start..range.end.min(n) {
                    if let Some(style) = s.glyph_styles.get_mut(i) {
                        apply(style);
                    }
                }
                s.pending_style = None;
                true
            }
            _ => {
                // Nothing selected: the whole block, as the caller's new
                // default does for letters without a style of their own —
                // letters styled one by one change too (they used to keep
                // theirs, so the same click changed all of one block and
                // none of another).
                for style in &mut s.glyph_styles {
                    apply(style);
                }
                s.pending_style = None;
                false
            }
        };
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
        on_selection
    }

    fn apply_text_font_family_choice(&mut self, family: crate::core::text::TextFontFamily) {
        self.ensure_text_font_registered(&family);
        let on_selection = self.edit.text_edit.is_some()
            && self.apply_text_style_to_selection(
                None,
                None,
                Some(family.clone()),
                None,
                None,
                None,
            );
        if !on_selection {
            self.edit.text_font_family = family;
        }
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }

    pub fn preview_text_font_family(&mut self, family: crate::core::text::TextFontFamily) {
        if self.edit.text_font_family == family {
            return;
        }
        if self.edit.text_font_preview.is_none() {
            self.edit.text_font_preview = Some(TextFontPreviewState {
                font_family: self.edit.text_font_family.clone(),
                session: self
                    .edit
                    .text_edit
                    .as_ref()
                    .map(|session| TextFontPreviewSession {
                        glyph_styles: session.glyph_styles.clone(),
                        pending_style: session.pending_style.clone(),
                        selection: session.selection.clone(),
                        caret: session.caret,
                    }),
            });
        }
        self.apply_text_font_family_choice(family);
    }

    pub fn commit_text_font_family(&mut self, family: crate::core::text::TextFontFamily) {
        self.edit.text_font_preview = None;
        self.apply_text_font_family_choice(family);
    }

    pub fn cancel_text_font_family_preview(&mut self) {
        let Some(snapshot) = self.edit.text_font_preview.take() else {
            return;
        };
        let family = snapshot.font_family;
        self.ensure_text_font_registered(&family);
        self.edit.text_font_family = family;
        if let (Some(session), Some(session_snapshot)) =
            (self.edit.text_edit.as_mut(), snapshot.session)
        {
            let n = session.buffer.chars().count();
            if session_snapshot.glyph_styles.is_empty() || session_snapshot.glyph_styles.len() == n
            {
                session.glyph_styles = session_snapshot.glyph_styles;
            }
            session.pending_style = session_snapshot.pending_style;
            session.selection = session_snapshot.selection;
            session.caret = session_snapshot.caret;
        }
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }

    /// Finalize the current text session: rasterize the buffer into the layer
    /// and push an undo entry.
    ///
    /// Resolves the session's document by id — the session may outlive a tab
    /// switch, and dropping it against the wrong document would leave the
    /// layer as the blank editing placeholder (losing the original text).
    pub fn commit_text_edit(&mut self) {
        let Some(mut session) = self.edit.text_edit.take() else {
            return;
        };
        self.edit.text_font_preview = None;
        self.edit.text_focus_pending = false;
        let Some(doc_idx) = self.docs.index_of(session.doc_id) else {
            return;
        };
        let is_active = doc_idx == self.docs.active_doc_idx;
        let mut td =
            self.text_data_from_state(session.buffer.clone(), session.glyph_styles.clone());
        td.rotation_deg = session.rotation_deg;
        td.stretch_x = session.stretch_x;
        td.flip_x = session.flip_x;
        td.flip_y = session.flip_y;
        td.path = session.path.clone();
        td.box_width = session.box_width;
        td.box_height = session.box_width.and(session.box_height);
        // A gradient across the letters is not the Type tool's: it stays.
        td.fill = session.orig.as_ref().and_then(|o| o.fill.clone());

        let empty = session.buffer.trim().is_empty();
        let changed = session.is_new || session.orig.as_ref() != Some(&td);
        // Back to the page as it was when the session began, with no step:
        // `text.set` below makes the one step, as it would for an agent.
        let label = {
            let canvas = &mut self.docs.documents[doc_idx].canvas;
            let label = match session.before_cmd.take() {
                Some(mut before) => {
                    use crate::core::command::Command as _;
                    let label = before.label().to_string();
                    let (mut w, mut h) = (canvas.width, canvas.height);
                    let mut ctx = crate::core::command::EditContext::new(
                        &mut canvas.layer_stack,
                        &mut w,
                        &mut h,
                        None,
                    );
                    let _ = before.undo(&mut ctx);
                    label
                }
                None if session.is_new => {
                    if let Some(idx) = canvas
                        .layer_stack
                        .layers
                        .iter()
                        .position(|l| l.id == session.layer_id)
                    {
                        canvas.layer_stack.remove_layer(idx);
                    }
                    "Type".to_string()
                }
                None => {
                    if let (Some(idx), Some(orig)) = (
                        canvas
                            .layer_stack
                            .layers
                            .iter()
                            .position(|l| l.id == session.layer_id),
                        session.orig.clone(),
                    ) {
                        rasterize_into_layer(canvas, idx, session.origin, orig);
                    }
                    "Edit Text".to_string()
                }
            };
            canvas.layer_revision += 1;
            label
        };

        if !empty && changed {
            let mut args = serde_json::json!({
                "doc": session.doc_id.0,
                "type": iai_engine::cmds::text::TypeSpec::of(&td),
                "origin": [session.origin.0, session.origin.1],
                "label": label,
            });
            if !session.is_new {
                args["layer"] = session.layer_id.into();
            }
            if let Err(error) = self.run_with("text.set", args) {
                // What was typed is never lost: written as the tool always
                // wrote it.
                crate::diag::note("text", &format!("text.set refused: {error:?}"));
                self.write_text_directly(doc_idx, &session, td, &label);
            }
        } else if empty && !session.is_new {
            // Every letter of a type layer deleted, then committed: the
            // layer goes, as one step (it used to come back unchanged, as if
            // the deleting had not been heard).
            let args = serde_json::json!({"doc": session.doc_id.0, "layer": session.layer_id});
            if let Err(error) = self.run_with("layer.delete", args) {
                crate::diag::note("text", &format!("emptied type layer kept: {error:?}"));
            }
        }

        if is_active {
            self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        }
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }

    /// The type of `session`, written into its document as one step without
    /// the command (should the command refuse it).
    fn write_text_directly(
        &mut self,
        doc_idx: usize,
        session: &TextEditState,
        td: TextData,
        label: &str,
    ) {
        let Some(doc) = self.docs.documents.get_mut(doc_idx) else {
            return;
        };
        let canvas = &mut doc.canvas;
        let (cw, ch) = (canvas.width, canvas.height);
        let mut cmd = LayerStructureCommand::capture_before(label, &canvas.layer_stack, cw, ch);
        let found = canvas
            .layer_stack
            .layers
            .iter()
            .position(|l| l.id == session.layer_id);
        let idx = match found {
            Some(idx) if !session.is_new => idx,
            _ => {
                let idx = canvas.layer_stack.add_layer(cw, ch);
                let layer = &mut canvas.layer_stack.layers[idx];
                layer.name = "Text".to_string();
                layer.offset = session.origin;
                idx
            }
        };
        rasterize_into_layer(canvas, idx, session.origin, td);
        cmd.capture_after(&canvas.layer_stack, cw, ch);
        canvas.record(Box::new(cmd));
        canvas.layer_revision += 1;
    }

    /// Install `family` into the egui context if it isn't yet. Only builtin
    /// families load at startup; anything else (System fonts, fonts referenced
    /// by opened documents) registers here the first time it is used. The
    /// canvas preview rasterizes from the font file directly, so this only
    /// affects the invisible editing TextEdit (click→caret mapping).
    pub fn ensure_text_font_registered(&mut self, family: &crate::core::text::TextFontFamily) {
        let id = family.egui_family_name();
        if self.edit.text_fonts_registered.contains(&id)
            || self.edit.text_fonts_failed.contains(&id)
        {
            return;
        }
        // Startup fonts not installed yet: builtins arrive with FontsReady,
        // and the family setters re-run this once the app is interactive.
        let Some(fonts) = self.win.ui_fonts.as_mut() else {
            return;
        };
        // The face's own number: a collection's (.ttc) later faces too.
        let Some((data, index)) = crate::core::text::validated_font_face_for(family) else {
            self.edit.text_fonts_failed.insert(id);
            return;
        };
        fonts.font_data.insert(
            id.clone(),
            std::sync::Arc::new(egui::FontData {
                index,
                ..egui::FontData::from_owned(data)
            }),
        );
        fonts
            .families
            .insert(egui::FontFamily::Name(id.clone().into()), vec![id.clone()]);
        self.win.egui_ctx.set_fonts(fonts.clone());
        self.edit.text_fonts_registered.insert(id);
    }

    /// Abort the current text session, discarding changes. Like commit, the
    /// session's document is resolved by id so a stale session still restores
    /// the original layer content.
    pub fn cancel_text_edit(&mut self) {
        let Some(session) = self.edit.text_edit.take() else {
            return;
        };
        self.edit.text_font_preview = None;
        self.edit.text_focus_pending = false;
        let Some(doc_idx) = self.docs.index_of(session.doc_id) else {
            return;
        };
        let is_active = doc_idx == self.docs.active_doc_idx;
        let canvas = &mut self.docs.documents[doc_idx].canvas;
        if let Some(mut before) = session.before_cmd {
            // The page as it was when the session began, kept whole: no
            // redrawing of the old text (a large heading took 0.8 s).
            use crate::core::command::Command as _;
            let (mut w, mut h) = (canvas.width, canvas.height);
            let mut ctx = crate::core::command::EditContext::new(
                &mut canvas.layer_stack,
                &mut w,
                &mut h,
                None,
            );
            let _ = before.undo(&mut ctx);
            canvas.layer_revision += 1;
        } else if let Some(idx) = canvas
            .layer_stack
            .layers
            .iter()
            .position(|l| l.id == session.layer_id)
        {
            if session.is_new {
                canvas.layer_stack.remove_layer(idx);
            } else if let Some(orig) = session.orig {
                rasterize_into_layer(canvas, idx, session.origin, orig);
            }
            canvas.layer_revision += 1;
        }
        if is_active {
            self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        }
        if let Some(w) = &self.win.window {
            w.request_redraw();
        }
    }
}

/// Widen the space after the letter before the caret by `delta` px: the
/// letter's own tracking, from the text's when it has none (Photoshop's
/// kerning between two letters). Nothing before the caret, nothing done.
pub(in crate::app) fn kern_at_caret(
    session: &mut crate::app::state::TextEditState,
    (base, text_tracking): (crate::core::text::GlyphStyle, f32),
    at: usize,
    delta: f32,
) {
    let n = session.buffer.chars().count();
    if at == 0 || at > n || !delta.is_finite() {
        return;
    }
    if session.glyph_styles.len() != n {
        session.glyph_styles = (0..n)
            .map(|i| session.glyph_styles.get(i).cloned().unwrap_or(base.clone()))
            .collect();
    }
    let style = &mut session.glyph_styles[at - 1];
    let now = style.tracking_px.unwrap_or(text_tracking);
    style.tracking_px = Some((now + delta).clamp(-200.0, 500.0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::canvas::Canvas;

    fn app_with_text(content: &str) -> (App, usize) {
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        let canvas = &mut app.docs.documents[0].canvas;
        let idx = canvas.layer_stack.add_layer(200, 200);
        let td = TextData {
            content: content.to_string(),
            font_family: crate::core::text::TextFontFamily::DejaVuSans,
            font_px: 48.0,
            ..TextData::default()
        };
        rasterize_into_layer(canvas, idx, (20, 30), td);
        canvas.layer_stack.active_idx = idx;
        (app, idx)
    }

    #[test]
    fn text_to_curves_preserves_mask_and_undo_redo_selection() {
        let (mut app, idx) = app_with_text("Aa");
        {
            let layer = &mut app.docs.documents[0].canvas.layer_stack.layers[idx];
            layer.add_mask(false);
            layer.selected = false;
        }

        assert!(app.text_to_curves(idx));
        let canvas = &mut app.docs.documents[0].canvas;
        assert!(
            canvas.layer_stack.layers[idx].mask.is_some(),
            "conversion must preserve the text layer mask"
        );
        assert!(canvas.layer_stack.layers[idx].selected);
        assert!(matches!(
            canvas.layer_stack.layers[idx].layer_type,
            LayerType::Vector(crate::core::vector::object::VectorGeometry::Path(_))
        ));

        canvas.undo().expect("undo text-to-curves");
        assert!(matches!(
            canvas.layer_stack.layers[idx].layer_type,
            LayerType::Text(_)
        ));
        assert!(
            !canvas.layer_stack.layers[idx].selected,
            "undo restores the original selection"
        );

        canvas.redo().expect("redo text-to-curves");
        assert!(canvas.layer_stack.layers[idx].mask.is_some());
        assert!(
            canvas.layer_stack.layers[idx].selected,
            "redo restores the conversion's selection"
        );
    }

    #[test]
    fn click_outside_is_refused_while_editing() {
        let mut app = App::new();
        app.text_tool_click(10.0, 10.0);
        assert!(app.edit.text_edit.is_some());
        let layers_during = app.docs.documents[0].canvas.layer_stack.layers.len();

        app.update_text_buffer("Hi".to_string());
        app.text_tool_click(200.0, 150.0);

        assert!(
            app.edit.text_edit.is_some(),
            "the session stays open until an explicit commit/cancel"
        );
        assert_eq!(
            app.docs.documents[0].canvas.layer_stack.layers.len(),
            layers_during,
            "a refused click must not spawn another text layer"
        );

        app.commit_text_edit();
        assert!(
            app.edit.text_edit.is_none(),
            "explicit commit ends the session"
        );
    }

    #[test]
    fn shift_f3_cycles_case_of_whole_buffer() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("hello world".to_string());
        // No selection → operate on the whole buffer.
        app.update_text_cursor_cache(None, Some(11));

        app.text_cycle_case();
        assert_eq!(app.edit.text_edit.as_ref().unwrap().buffer, "HELLO WORLD");
        app.text_cycle_case();
        assert_eq!(app.edit.text_edit.as_ref().unwrap().buffer, "Hello World");
        app.text_cycle_case();
        assert_eq!(app.edit.text_edit.as_ref().unwrap().buffer, "hello world");
    }

    #[test]
    fn text_panel_case_buttons_set_explicit_case() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("hello world".to_string());
        app.update_text_cursor_cache(None, Some(11));

        app.text_set_case(crate::core::text::TextCase::Upper);
        assert_eq!(app.edit.text_edit.as_ref().unwrap().buffer, "HELLO WORLD");
        // Idempotent: the same button again is a no-op, not a cycle.
        app.text_set_case(crate::core::text::TextCase::Upper);
        assert_eq!(app.edit.text_edit.as_ref().unwrap().buffer, "HELLO WORLD");
        app.text_set_case(crate::core::text::TextCase::Title);
        assert_eq!(app.edit.text_edit.as_ref().unwrap().buffer, "Hello World");
        app.text_set_case(crate::core::text::TextCase::Lower);
        assert_eq!(app.edit.text_edit.as_ref().unwrap().buffer, "hello world");
    }

    #[test]
    fn shift_f3_recases_only_the_selection_and_keeps_glyph_styles_aligned() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("abcd".to_string());
        // Give char 1 a distinct colour so we can prove styles stay aligned.
        app.update_text_cursor_cache(Some(1..2), Some(2));
        app.apply_text_style_to_selection(Some([200, 30, 30, 255]), None, None, None, None, None);

        // Select "bc" and cycle case → "BC".
        app.update_text_cursor_cache(Some(1..3), Some(3));
        app.text_cycle_case();

        let s = app.edit.text_edit.as_ref().unwrap();
        assert_eq!(s.buffer, "aBCd", "only the selected span is recased");
        assert_eq!(s.selection, Some(1..3), "the selection is preserved");
        assert_eq!(s.glyph_styles.len(), 4);
        assert_eq!(
            s.glyph_styles[1].color,
            [200, 30, 30, 255],
            "per-glyph colour follows its character through the recase"
        );
    }

    #[test]
    fn recolor_text_layer_recolours_type_and_is_undoable() {
        let (mut app, idx) = app_with_text("Hi");
        let orig = match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => td.color,
            _ => panic!("expected a text layer"),
        };
        let new_color = [10, 120, 200, 255];
        assert_ne!(orig, new_color);

        assert!(app.recolor_text_layer(idx, new_color));
        match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => assert_eq!(td.color, new_color),
            _ => panic!("still a text layer"),
        }
        // Re-applying the same colour is a no-op (nothing to record).
        assert!(!app.recolor_text_layer(idx, new_color));

        app.docs.documents[0]
            .canvas
            .undo()
            .expect("undo the recolour");
        match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => assert_eq!(td.color, orig, "undo restores the original colour"),
            _ => panic!("undo keeps it a text layer"),
        }
    }

    /// Add a Text layer with the given font to doc 0, returning its index.
    fn add_text_layer(
        app: &mut App,
        content: &str,
        font: crate::core::text::TextFontFamily,
    ) -> usize {
        let canvas = &mut app.docs.documents[0].canvas;
        let (cw, ch) = (canvas.width, canvas.height);
        let idx = canvas.layer_stack.add_layer(cw, ch);
        let td = TextData {
            content: content.to_string(),
            font_family: font,
            font_px: 48.0,
            ..TextData::default()
        };
        rasterize_into_layer(canvas, idx, (10, 10), td);
        idx
    }

    fn layer_font(app: &App, idx: usize) -> crate::core::text::TextFontFamily {
        match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => td.font_family.clone(),
            _ => panic!("expected a text layer"),
        }
    }

    #[test]
    fn refont_origin_centres_new_block_on_old_centre() {
        // Old ink centre (canvas) = (100+25, 100+10) = (125, 110).
        let origin = refont_origin_from_bounds((100, 100), (0, 0, 50, 20), (0, 0, 80, 30), (0, 0));
        assert_eq!(origin, (85, 95));
        // Placing the new block (ink centre local (40, 15)) at that origin lands
        // its centre exactly on the old centre.
        assert_eq!((origin.0 + 40, origin.1 + 15), (125, 110));
    }

    #[test]
    fn refont_origin_accounts_for_placed_delta() {
        // Non-zero placed delta (rotated/stretched): origin subtracts it so
        // layer.offset (= origin + delta) still centres the block. Same bounds →
        // centres equal → final offset 0 → origin = -delta.
        let delta = (7, -3);
        let origin = refont_origin_from_bounds((0, 0), (0, 0, 40, 40), (0, 0, 40, 40), delta);
        assert_eq!(origin, (-7, 3));
        assert_eq!((origin.0 + delta.0, origin.1 + delta.1), (0, 0));
    }

    #[test]
    fn change_all_text_font_switches_every_layer_and_is_one_undo() {
        use crate::core::text::TextFontFamily;
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        let a = add_text_layer(&mut app, "Alpha", TextFontFamily::DejaVuSans);
        let b = add_text_layer(&mut app, "Beta", TextFontFamily::Arial);

        // from = None → every text layer becomes LiberationSans.
        let n = app.change_document_text_font(None, TextFontFamily::LiberationSans);
        assert_eq!(n, 2, "both text layers change");
        assert_eq!(layer_font(&app, a), TextFontFamily::LiberationSans);
        assert_eq!(layer_font(&app, b), TextFontFamily::LiberationSans);

        // A single undo restores BOTH original fonts.
        app.docs.documents[0]
            .canvas
            .undo()
            .expect("undo batch font");
        assert_eq!(layer_font(&app, a), TextFontFamily::DejaVuSans);
        assert_eq!(layer_font(&app, b), TextFontFamily::Arial);
    }

    #[test]
    fn replace_specific_font_leaves_other_fonts_untouched() {
        use crate::core::text::TextFontFamily;
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        let a = add_text_layer(&mut app, "Alpha", TextFontFamily::DejaVuSans);
        let b = add_text_layer(&mut app, "Beta", TextFontFamily::Arial);

        // Replace only DejaVuSans → Tahoma; Arial layer must stay Arial.
        let n = app.change_document_text_font(
            Some(TextFontFamily::DejaVuSans.storage_name()),
            TextFontFamily::Tahoma,
        );
        assert_eq!(n, 1, "only the DejaVuSans layer changes");
        assert_eq!(layer_font(&app, a), TextFontFamily::Tahoma);
        assert_eq!(layer_font(&app, b), TextFontFamily::Arial);
    }

    #[test]
    fn change_font_covers_per_glyph_overrides() {
        use crate::core::text::TextFontFamily;
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        let idx = add_text_layer(&mut app, "Hi", TextFontFamily::DejaVuSans);
        // Give the second character a distinct per-glyph font.
        {
            let layer = &mut app.docs.documents[0].canvas.layer_stack.layers[idx];
            if let LayerType::Text(td) = &mut layer.layer_type {
                td.glyph_styles = vec![td.glyph_style(0), td.glyph_style(1)];
                td.glyph_styles[1].font_family = TextFontFamily::Arial;
            }
        }

        let n = app.change_document_text_font(None, TextFontFamily::Calibri);
        assert_eq!(n, 1);
        match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => {
                assert_eq!(td.font_family, TextFontFamily::Calibri);
                assert!(
                    td.glyph_styles
                        .iter()
                        .all(|g| g.font_family == TextFontFamily::Calibri),
                    "per-glyph fonts switch too"
                );
            }
            _ => panic!("still a text layer"),
        }
    }

    #[test]
    fn fonts_in_use_lists_base_and_glyph_fonts() {
        use crate::core::text::TextFontFamily;
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        let idx = add_text_layer(&mut app, "Hi", TextFontFamily::DejaVuSans);
        {
            let layer = &mut app.docs.documents[0].canvas.layer_stack.layers[idx];
            if let LayerType::Text(td) = &mut layer.layer_type {
                td.glyph_styles = vec![td.glyph_style(0), td.glyph_style(1)];
                td.glyph_styles[1].font_family = TextFontFamily::Arial;
            }
        }
        let storages: Vec<String> = app
            .document_text_fonts_in_use()
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(storages.contains(&TextFontFamily::DejaVuSans.storage_name()));
        assert!(storages.contains(&TextFontFamily::Arial.storage_name()));
    }

    #[test]
    fn batch_format_applies_bold_and_uppercase_in_one_undo() {
        use crate::core::text::{TextCase, TextFontFamily};
        use crate::ui::intent::TextBatchFormat;
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        let idx = add_text_layer(&mut app, "hello", TextFontFamily::DejaVuSans);

        let n = app.change_document_text_format(TextBatchFormat {
            set_bold: Some(true),
            set_case: Some(TextCase::Upper),
            ..Default::default()
        });
        assert_eq!(n, 1);
        match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => {
                assert!(td.bold);
                assert_eq!(td.content, "HELLO");
            }
            _ => panic!("text layer"),
        }
        // The colour/size/bold/case batch is a single undo.
        app.docs.documents[0]
            .canvas
            .undo()
            .expect("undo batch format");
        match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => {
                assert!(!td.bold);
                assert_eq!(td.content, "hello");
            }
            _ => panic!("text layer"),
        }
    }

    #[test]
    fn batch_format_noop_changes_nothing() {
        use crate::core::text::TextFontFamily;
        use crate::ui::intent::TextBatchFormat;
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        add_text_layer(&mut app, "hi", TextFontFamily::DejaVuSans);
        assert_eq!(
            app.change_document_text_format(TextBatchFormat::default()),
            0
        );
    }

    #[test]
    fn batch_format_color_and_size_override_per_glyph() {
        use crate::core::text::TextFontFamily;
        use crate::ui::intent::TextBatchFormat;
        let mut app = App::new();
        app.docs.documents[0].canvas = Canvas::new(200, 200);
        let idx = add_text_layer(&mut app, "Hi", TextFontFamily::DejaVuSans);
        // Seed a distinct per-glyph colour to prove the batch overrides it.
        {
            let layer = &mut app.docs.documents[0].canvas.layer_stack.layers[idx];
            if let LayerType::Text(td) = &mut layer.layer_type {
                td.glyph_styles = vec![td.glyph_style(0), td.glyph_style(1)];
                td.glyph_styles[0].color = [10, 20, 30, 255];
            }
        }
        let n = app.change_document_text_format(TextBatchFormat {
            set_color: Some([200, 0, 0, 255]),
            set_size: Some(72.0),
            ..Default::default()
        });
        assert_eq!(n, 1);
        match &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type {
            LayerType::Text(td) => {
                assert_eq!(td.color, [200, 0, 0, 255]);
                assert_eq!(td.font_px, 72.0);
                assert!(td.glyph_styles.iter().all(|g| g.color == [200, 0, 0, 255]));
            }
            _ => panic!("text layer"),
        }
    }

    #[test]
    fn text_session_engages_the_modal_lock() {
        let mut app = App::new();
        assert!(!app.modal_lock_active());
        app.text_tool_click(10.0, 10.0);
        assert!(app.modal_lock_active(), "editing text locks other features");
        app.cancel_text_edit();
        assert!(!app.modal_lock_active());
    }

    /// Typing and committing is `text.set`: one undo step named as before,
    /// on the layer the session made; editing it again is another.
    #[test]
    fn typed_text_commits_through_the_command_one_step_at_a_time() {
        let mut app = App::new();
        app.shell.ui.show_welcome = false;
        let steps = |app: &App| app.docs.documents[0].canvas.undo_count();
        let before = steps(&app);
        app.text_tool_click(10.0, 10.0);
        let layer_id = app.edit.text_edit.as_ref().unwrap().layer_id;
        app.update_text_buffer("Xin chào".to_string());
        app.commit_text_edit();
        let canvas = &app.docs.documents[0].canvas;
        assert_eq!(steps(&app), before + 1);
        assert_eq!(canvas.history_entries().last().unwrap().label, "Type");
        let idx = canvas
            .layer_stack
            .layers
            .iter()
            .position(|l| l.id == layer_id)
            .expect("the session's layer");
        assert!(matches!(
            &canvas.layer_stack.layers[idx].layer_type,
            LayerType::Text(td) if td.content == "Xin chào"
        ));

        app.begin_edit_text_layer(idx);
        app.update_text_buffer("Xin chào bạn".to_string());
        app.commit_text_edit();
        assert_eq!(steps(&app), before + 2);
        let canvas = &app.docs.documents[0].canvas;
        assert_eq!(canvas.history_entries().last().unwrap().label, "Edit Text");
        // Opened and left as it was: no step.
        app.begin_edit_text_layer(idx);
        app.commit_text_edit();
        assert_eq!(steps(&app), before + 2);
        // Undo goes back one edit at a time.
        app.docs.documents[0].canvas.undo();
        assert!(matches!(
            &app.docs.documents[0].canvas.layer_stack.layers[idx].layer_type,
            LayerType::Text(td) if td.content == "Xin chào"
        ));
    }

    #[test]
    fn commit_lands_in_the_sessions_document_after_a_doc_switch() {
        let mut app = App::new();
        app.open_new_doc_tab();
        assert_eq!(app.docs.active_doc_idx, 1);
        app.text_tool_click(10.0, 10.0);
        app.update_text_buffer("giữ chữ".to_string());
        // A doc switch that bypasses switch_to_doc's implicit commit
        // (async/AI paths assign active_doc_idx directly).
        app.docs.active_doc_idx = 0;
        app.commit_text_edit();

        assert!(app.edit.text_edit.is_none());
        let doc1 = &app.docs.documents[1];
        assert!(
            doc1.canvas.layer_stack.layers.iter().any(|l| matches!(
                &l.layer_type,
                LayerType::Text(td) if td.content == "giữ chữ"
            )),
            "text must be finalized into its own document"
        );
        assert!(doc1.is_modified(), "the owning document carries the change");
    }

    /// Nothing selected: a style goes to the whole block, letters styled on
    /// their own included (it used to go to the next letter typed only, which
    /// in a uniform block changed all of it and in a mixed one none of it).
    #[test]
    fn a_style_with_nothing_selected_goes_to_the_whole_block() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("abcd".to_string());
        // "bc" red on their own.
        app.update_text_cursor_cache(Some(1..3), Some(3));
        app.apply_text_style_to_selection(Some([200, 30, 30, 255]), None, None, None, None, None);
        // Then blue with nothing selected, as the options bar does it (the
        // letters first, then the default).
        app.update_text_cursor_cache(None, Some(4));
        app.apply_text_style_to_selection(Some([0, 0, 200, 255]), None, None, None, None, None);
        app.edit.text_color = [0, 0, 200, 255];
        let s = app.edit.text_edit.as_ref().expect("session still open");
        assert_eq!(s.glyph_styles.len(), 4);
        assert!(s.glyph_styles.iter().all(|g| g.color == [0, 0, 200, 255]));
        assert!(s.pending_style.is_none());
    }

    /// The options bar shows the selected letters' style.
    #[test]
    fn the_options_bar_shows_the_selected_letters_style() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("abcd".to_string());
        app.update_text_cursor_cache(Some(1..3), Some(3));
        app.apply_text_style_to_selection(None, Some(90.0), None, Some(true), None, None);
        let bar = app.text_bar_style();
        assert_eq!((bar.font_px, bar.bold), (90.0, true));
        // "cd": not all bold, the first one's size.
        app.update_text_cursor_cache(Some(2..4), Some(4));
        let bar = app.text_bar_style();
        assert_eq!((bar.font_px, bar.bold), (90.0, false));
        // Nothing selected: the type's own.
        app.update_text_cursor_cache(None, Some(4));
        let bar = app.text_bar_style();
        assert_eq!(
            (bar.font_px, bar.bold),
            (app.edit.text_font_px, app.edit.text_bold)
        );
    }

    /// A type layer emptied of its words and committed goes, as one step.
    #[test]
    fn an_emptied_type_layer_goes() {
        let (mut app, idx) = app_with_text("Xin chào");
        app.shell.ui.show_welcome = false;
        let id = app.docs.documents[0].canvas.layer_stack.layers[idx].id;
        let steps = app.docs.documents[0].canvas.undo_count();
        app.begin_edit_text_layer(idx);
        app.update_text_buffer(String::new());
        app.commit_text_edit();
        let canvas = &app.docs.documents[0].canvas;
        assert!(canvas.layer_stack.layers.iter().all(|l| l.id != id));
        assert_eq!(canvas.undo_count(), steps + 1);
        app.docs.documents[0].canvas.undo();
        assert!(matches!(
            app.docs.documents[0]
                .canvas
                .layer_stack
                .layers
                .iter()
                .find(|l| l.id == id)
                .map(|l| &l.layer_type),
            Some(LayerType::Text(td)) if td.content == "Xin chào"
        ));
    }

    /// With the Type tool and a type layer picked, the options bar shows its
    /// style, and a change made there goes to the layer, as one step.
    #[test]
    fn a_picked_type_layer_takes_the_options_bars_change() {
        let (mut app, idx) = app_with_text("Tiêu đề");
        app.shell.ui.show_welcome = false;
        app.edit.tools.select(ToolId::Text);
        app.edit.text_font_px = 12.0;
        let id = app
            .adopt_picked_type_style()
            .expect("the picked type layer");
        assert_eq!(app.edit.text_font_px, 48.0);
        let steps = app.docs.documents[0].canvas.undo_count();
        app.edit.text_font_px = 80.0;
        app.edit.text_restyle_pending = Some(id);
        app.finish_picked_type_restyle();
        let canvas = &app.docs.documents[0].canvas;
        assert!(matches!(
            &canvas.layer_stack.layers[idx].layer_type,
            LayerType::Text(td) if td.font_px == 80.0 && td.content == "Tiêu đề"
        ));
        assert_eq!(canvas.undo_count(), steps + 1);
        assert!(app.edit.text_restyle_pending.is_none());
    }

    /// Tracking and line height go to the selected letters while some are
    /// selected, else to the whole text (and no letter keeps its own).
    #[test]
    fn spacing_goes_to_the_selected_letters_or_the_whole_text() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("abcd".to_string());
        app.update_text_cursor_cache(Some(1..3), Some(3));
        assert!(app.apply_text_spacing_to_selection(Some(12.0), Some(2.0)));
        let s = app.edit.text_edit.as_ref().expect("a session");
        let own: Vec<_> = s.glyph_styles.iter().map(|g| g.tracking_px).collect();
        assert_eq!(own, vec![None, Some(12.0), Some(12.0), None]);
        assert_eq!(app.text_bar_style().tracking_px, 12.0);
        assert_eq!(app.text_bar_style().line_height, 2.0);
        // Nothing selected: the letters' own go; the caller sets the text's.
        app.update_text_cursor_cache(None, Some(4));
        assert!(!app.apply_text_spacing_to_selection(Some(3.0), None));
        let s = app.edit.text_edit.as_ref().expect("a session");
        assert!(s.glyph_styles.iter().all(|g| g.tracking_px.is_none()));
        assert_eq!(s.glyph_styles[1].line_height, Some(2.0));
    }

    /// Alt+arrows with nothing selected: the space after the letter before
    /// the caret, from the text's own tracking (Photoshop's kerning).
    #[test]
    fn kerning_widens_the_space_at_the_caret() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("abcd".to_string());
        app.edit.text_tracking_px = 4.0;
        let base = (app.text_kern_base_style(), app.edit.text_tracking_px);
        let s = app.edit.text_edit.as_mut().expect("a session");
        super::kern_at_caret(s, base.clone(), 2, 1.0);
        super::kern_at_caret(s, base.clone(), 2, 1.0);
        super::kern_at_caret(s, base.clone(), 0, 1.0);
        let own: Vec<_> = s.glyph_styles.iter().map(|g| g.tracking_px).collect();
        assert_eq!(own, vec![None, Some(6.0), None, None]);
    }

    /// A drag with the Type tool makes paragraph type: a box as wide as the
    /// drag, its top-left corner where the drag began, the words wrapping in
    /// it; the box can be made another width while editing.
    #[test]
    fn a_drag_makes_a_paragraph_box() {
        let mut app = App::new();
        app.shell.ui.show_welcome = false;
        app.edit.text_font_family = crate::core::text::TextFontFamily::DejaVuSans;
        app.edit.text_font_px = 30.0;
        app.edit.text_font_px_auto = false;
        // Flatter than a line: a box that grows with its lines.
        app.text_tool_box((300.0, 200.0), (100.0, 220.0));
        if crate::core::text::font_for(&app.edit.text_font_family).is_none() {
            return; // no font here
        }
        let s = app.edit.text_edit.as_ref().expect("a session");
        assert_eq!(s.box_width, Some(200.0));
        assert_eq!(s.box_height, None);
        app.update_text_buffer("Một hai ba bốn năm sáu bảy tám chín mười".to_string());
        app.sync_text_anchor();
        let td = app.session_text_data().expect("the type");
        let (left, top, ..) = crate::core::text::box_in_raster(&td).expect("a box");
        let s = app.edit.text_edit.as_ref().expect("a session");
        assert!((s.origin.0 as f32 + left - 100.0).abs() <= 1.0);
        assert!((s.origin.1 as f32 + top - 200.0).abs() <= 1.0);
        let (_, h) = crate::core::text::upright_size(&td).expect("size");
        assert!(h > 80, "wrapped: {h}");
        app.commit_text_edit();
        let canvas = &app.docs.documents[0].canvas;
        assert!(canvas.layer_stack.layers.iter().any(|l| matches!(
            &l.layer_type,
            LayerType::Text(td) if td.box_width == Some(200.0)
        )));

        // A line tall or more: the box keeps that height, the rest is hidden.
        app.text_tool_box((100.0, 400.0), (300.0, 460.0));
        app.update_text_buffer("Một hai ba bốn năm sáu bảy tám chín mười".to_string());
        let s = app.edit.text_edit.as_ref().expect("a session");
        assert_eq!(s.box_height, Some(60.0));
        let td = app.session_text_data().expect("the type");
        assert!(crate::core::text::text_overset(&td));
        let (_, h) = crate::core::text::upright_size(&td).expect("size");
        assert!(h < 80, "kept to the box: {h}");
    }

    /// A locked type layer is not opened for editing.
    #[test]
    fn a_locked_type_layer_is_not_opened() {
        let (mut app, idx) = app_with_text("Khóa");
        app.docs.documents[0].canvas.layer_stack.layers[idx].locked = true;
        app.begin_edit_text_layer(idx);
        assert!(app.edit.text_edit.is_none());
    }

    /// Centred type keeps its anchor (the click, on the first baseline)
    /// still while it is typed and resized, as Photoshop's point type.
    #[test]
    fn centred_type_grows_round_its_anchor() {
        let mut app = App::new();
        app.shell.ui.show_welcome = false;
        app.edit.text_align = crate::core::text::TextAlign::Center;
        app.edit.text_font_family = crate::core::text::TextFontFamily::DejaVuSans;
        app.edit.text_font_px = 40.0;
        app.edit.text_font_px_auto = false;
        app.text_tool_click(400.0, 300.0);
        if crate::core::text::font_for(&app.edit.text_font_family).is_none() {
            return; // no font here
        }
        let anchor_now = |app: &App| {
            let td = app.session_text_data().expect("a session");
            let (dx, dy) = super::anchor_offset(&td).expect("straight type");
            let s = app.edit.text_edit.as_ref().expect("a session");
            (s.origin.0 as f32 + dx, s.origin.1 as f32 + dy)
        };
        let close =
            |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() <= 1.0 && (a.1 - b.1).abs() <= 1.0;
        app.update_text_buffer("Tiêu đề".to_string());
        app.sync_text_anchor();
        assert!(
            close(anchor_now(&app), (400.0, 300.0)),
            "{:?}",
            anchor_now(&app)
        );
        let narrow = app.edit.text_edit.as_ref().expect("a session").origin;
        app.edit.text_font_px = 80.0;
        app.sync_text_anchor();
        assert!(
            close(anchor_now(&app), (400.0, 300.0)),
            "{:?}",
            anchor_now(&app)
        );
        // Bigger, centred: it grew to the left as much as to the right.
        assert!(app.edit.text_edit.as_ref().expect("a session").origin.0 < narrow.0);
    }

    #[test]
    fn selected_bold_applies_only_to_selected_chars() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("abcd".to_string());
        app.update_text_cursor_cache(Some(1..3), Some(3));

        app.apply_text_style_to_selection(None, None, None, Some(true), None, None);

        let s = app.edit.text_edit.as_ref().expect("session still open");
        assert_eq!(s.glyph_styles.len(), 4);
        assert!(!s.glyph_styles[0].bold);
        assert!(s.glyph_styles[1].bold);
        assert!(s.glyph_styles[2].bold);
        assert!(!s.glyph_styles[3].bold);
    }

    #[test]
    fn font_hover_preview_cancel_restores_mixed_glyph_styles() {
        let mut app = App::new();
        app.text_tool_click(5.0, 5.0);
        app.update_text_buffer("abcd".to_string());
        app.update_text_cursor_cache(Some(1..3), Some(3));

        let original_family = app.edit.text_font_family.clone();
        let original_styles: Vec<_> = "abcd"
            .chars()
            .enumerate()
            .map(|(i, _)| crate::core::text::GlyphStyle {
                color: [0, 0, 0, 255],
                font_px: 48.0,
                font_family: if i % 2 == 0 {
                    crate::core::text::TextFontFamily::SegoeUi
                } else {
                    crate::core::text::TextFontFamily::Tahoma
                },
                bold: i == 0,
                italic: i == 1,
                underline: i == 2,
                tracking_px: None,
                line_height: None,
            })
            .collect();
        app.edit.text_edit.as_mut().unwrap().glyph_styles = original_styles.clone();

        app.preview_text_font_family(crate::core::text::TextFontFamily::Arial);
        assert!(
            app.edit.text_edit.as_ref().unwrap().glyph_styles[1].font_family
                == crate::core::text::TextFontFamily::Arial
        );

        app.cancel_text_font_family_preview();

        let s = app.edit.text_edit.as_ref().expect("session still open");
        assert_eq!(app.edit.text_font_family, original_family);
        assert_eq!(s.glyph_styles, original_styles);
    }
}
