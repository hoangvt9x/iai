//! Warp on a grid from Free Transform (Ctrl+T ▸ right-click ▸ Warp), as
//! Photoshop's: the layer's box under a 3 × 3 grid, its four corners and
//! their handles dragged to bend it, the surface dragged anywhere inside to
//! pull that place along. Enter (or ✓) applies one undo step, Esc (or ✗)
//! gives the layer back, Ctrl+Z steps back one drag. The pixels: see
//! [`crate::core::grid_warp`].
//!
//! While it is open the layer lies in a frame the bent picture may fill
//! (the page and the layer's own frame; for a layer with a mask its own
//! frame only, so the mask stays where it was), drawn into a buffer kept for
//! the session. A drag only moves the net; once a frame the part of the
//! picture that moved is drawn again (a draft, as fine as the zoom shows,
//! while the pointer is down; finely when it lets go) and only that part
//! goes to the screen (16/11: drawing the whole layer anew at every move of
//! the mouse lagged).

use super::render::CanvasEvent;
use super::state::App;
use crate::core::grid_warp::{self, ANCHORS, HANDLES, Net, Quality, Source};
use crate::core::tile::TileMap;

/// How near a corner or handle a press must be to take it, in screen pixels.
const GRAB: f32 = 14.0;
/// Drags Ctrl+Z can step back while the warp is open.
const UNDO_DEPTH: usize = 32;

#[derive(Clone, Copy)]
enum Grab {
    Point(usize),
    Surface(f32, f32),
}

pub(crate) struct GridWarpState {
    doc_id: crate::core::document::DocumentId,
    layer_id: u32,
    /// The layer as it was: what Esc gives back and the picture is bent from.
    tiles: TileMap,
    flat: Vec<u8>,
    width: u32,
    height: u32,
    offset: (i32, i32),
    /// Where the bent picture may go: the page and the layer's own frame.
    room: (i32, i32, i32, i32),
    /// The box the net bends, `(x, y, w, h)` on the canvas.
    frame: (f32, f32, f32, f32),
    pub(crate) net: Net,
    grab: Option<Grab>,
    last: (f32, f32),
    undo: Vec<Net>,
    bent: bool,
    /// The layer's pixels while warping: the frame `room`, RGBA.
    work: Vec<u8>,
    /// The part of `work` the patch was last drawn over (`x0, y0, x1, y1`,
    /// room pixels, end exclusive).
    shown: (u32, u32, u32, u32),
    /// The net moved since it was last drawn.
    pending: bool,
    /// The part last drawn as a draft, to draw finely once the drag ends.
    rough: Option<(u32, u32, u32, u32)>,
}

impl GridWarpState {
    /// The frame the layer lies in while warping, `(x, y, w, h)`.
    fn room_frame(&self) -> (i32, i32, u32, u32) {
        (
            self.room.0,
            self.room.1,
            (self.room.2 - self.room.0).max(1) as u32,
            (self.room.3 - self.room.1).max(1) as u32,
        )
    }
}

/// The smallest box holding both (`x0, y0, x1, y1`); an empty one counts
/// for nothing.
pub(in crate::app) fn union(
    a: (u32, u32, u32, u32),
    b: (u32, u32, u32, u32),
) -> (u32, u32, u32, u32) {
    let empty = |r: (u32, u32, u32, u32)| r.2 <= r.0 || r.3 <= r.1;
    match (empty(a), empty(b)) {
        (true, _) => b,
        (_, true) => a,
        _ => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
    }
}

impl App {
    /// Open the grid warp on the active layer: a pixel layer with something
    /// on it. False (and a word in the status bar) otherwise.
    pub(crate) fn begin_grid_warp(&mut self) -> bool {
        self.cancel_grid_warp();
        let doc_id = self.docs.active().id;
        let canvas = &mut self.docs.active_mut().canvas;
        canvas.layer_stack.normalize_active_idx();
        let (page_w, page_h) = (canvas.width as i32, canvas.height as i32);
        let Some(layer) = canvas.layer_stack.layers.get(canvas.layer_stack.active_idx) else {
            return false;
        };
        if !layer.is_raster() || (!layer.is_background && layer.locked) {
            self.shell.status_msg =
                "Warp: chỉ uốn được layer ảnh chưa khóa (chữ / hình thì Rasterize trước)"
                    .to_string();
            return false;
        }
        let Some((bx, by, bw, bh)) = layer.tiles.content_bounds() else {
            self.shell.status_msg = "Warp: layer trống, không có gì để uốn".to_string();
            return false;
        };
        let (bw, bh) = (bw - bx, bh - by);
        if bw <= 0 || bh <= 0 {
            return false;
        }
        let (ox, oy) = layer.offset;
        let (lw, lh) = (layer.width as i32, layer.height as i32);
        let room = if layer.mask.is_some() {
            (ox, oy, ox + lw, oy + lh)
        } else {
            (
                ox.min(0),
                oy.min(0),
                (ox + lw).max(page_w),
                (oy + lh).max(page_h),
            )
        };
        let frame = ((ox + bx) as f32, (oy + by) as f32, bw as f32, bh as f32);
        let flat = layer.tiles.flatten();
        let (rw, rh) = ((room.2 - room.0) as usize, (room.3 - room.1) as usize);
        // The layer as it is, laid in the room.
        let mut work = vec![0u8; rw * rh * 4];
        let (dx, dy) = ((ox - room.0) as usize, (oy - room.1) as usize);
        for y in 0..lh as usize {
            let from = y * lw as usize * 4;
            let to = ((y + dy) * rw + dx) * 4;
            work[to..to + lw as usize * 4].copy_from_slice(&flat[from..from + lw as usize * 4]);
        }
        let net = grid_warp::identity(frame.0, frame.1, frame.2, frame.3);
        let mut state = GridWarpState {
            doc_id,
            layer_id: layer.id,
            flat,
            tiles: layer.tiles.clone(),
            width: layer.width,
            height: layer.height,
            offset: layer.offset,
            room,
            frame,
            net,
            grab: None,
            last: (0.0, 0.0),
            undo: Vec::new(),
            bent: false,
            work,
            shown: (0, 0, 0, 0),
            pending: false,
            rough: None,
        };
        state.shown = grid_warp::covered(&state.net, state.room_frame());
        let moved = (room.0, room.1) != (ox, oy) || (rw, rh) != (lw as usize, lh as usize);
        if moved {
            let id = layer.id;
            if let Some(layer) = canvas.layer_stack.layers.iter_mut().find(|l| l.id == id) {
                layer.tiles = TileMap::from_rgba(&state.work, rw as u32, rh as u32);
                layer.width = rw as u32;
                layer.height = rh as u32;
                layer.offset = (room.0, room.1);
            }
            canvas.layer_revision += 1;
            canvas.dirty.expand_full(canvas.width, canvas.height);
        }
        self.edit.grid_warp = Some(state);
        if moved {
            self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        }
        self.edit.tools.select(crate::tools::ToolId::Transform);
        self.shell.status_msg =
            "Warp: kéo góc / tay nắm / bất kỳ chỗ nào trong lưới để uốn • Enter áp dụng • Esc hủy"
                .to_string();
        self.redraw_main();
        true
    }

    fn grid_warp_doc_idx(&self) -> Option<usize> {
        self.docs.index_of(self.edit.grid_warp.as_ref()?.doc_id)
    }

    pub(crate) fn grid_warp_pointer_down(&mut self) {
        let ev = self.tool_event();
        let (sx, sy) = (self.edit.input.mouse_x, self.edit.input.mouse_y);
        let (zoom, vx, vy) = (
            self.edit.view.zoom,
            self.edit.view.offset_x,
            self.edit.view.offset_y,
        );
        let Some(state) = self.edit.grid_warp.as_mut() else {
            return;
        };
        let screen = |p: (f32, f32)| (p.0 * zoom + vx, p.1 * zoom + vy);
        let near = |i: usize| {
            let (px, py) = screen(state.net[i]);
            ((px - sx).powi(2) + (py - sy).powi(2)).sqrt()
        };
        // Handles before corners: a handle left on its corner is still
        // reachable by dragging the corner away first, never the other way.
        let point = HANDLES
            .iter()
            .map(|&(h, _)| h)
            .chain(ANCHORS)
            .filter(|&i| near(i) <= GRAB)
            .min_by(|&a, &b| near(a).total_cmp(&near(b)));
        state.grab = match point {
            Some(i) => Some(Grab::Point(i)),
            None => grid_warp::locate(&state.net, ev.canvas_x, ev.canvas_y)
                .map(|(u, v)| Grab::Surface(u, v)),
        };
        if state.grab.is_some() {
            if state.undo.len() >= UNDO_DEPTH {
                state.undo.remove(0);
            }
            state.undo.push(state.net);
        }
        state.last = (ev.canvas_x, ev.canvas_y);
    }

    pub(crate) fn grid_warp_dragging(&self) -> bool {
        self.edit
            .grid_warp
            .as_ref()
            .is_some_and(|s| s.grab.is_some())
    }

    pub(crate) fn grid_warp_pointer_drag(&mut self) {
        let ev = self.tool_event();
        let Some(state) = self.edit.grid_warp.as_mut() else {
            return;
        };
        let Some(grab) = state.grab else {
            return;
        };
        let (dx, dy) = (ev.canvas_x - state.last.0, ev.canvas_y - state.last.1);
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        state.last = (ev.canvas_x, ev.canvas_y);
        match grab {
            Grab::Point(i) => grid_warp::move_point(&mut state.net, i, dx, dy),
            Grab::Surface(u, v) => grid_warp::pull(&mut state.net, u, v, dx, dy),
        }
        state.bent = true;
        state.pending = true;
        self.redraw_main();
    }

    pub(crate) fn grid_warp_pointer_up(&mut self) {
        let Some(state) = self.edit.grid_warp.as_mut() else {
            return;
        };
        if state.grab.take().is_none() {
            return;
        }
        // A press that bent nothing leaves no step to undo; one that did is
        // drawn again finely (the drag showed a draft).
        if state.undo.last() == Some(&state.net) {
            state.undo.pop();
        } else {
            state.pending = true;
            self.redraw_main();
        }
    }

    /// Ctrl+Z while warping: one drag back. True when the warp took it.
    pub(crate) fn grid_warp_undo(&mut self) -> bool {
        let Some(state) = self.edit.grid_warp.as_mut() else {
            return false;
        };
        match state.undo.pop() {
            Some(net) => {
                state.net = net;
                state.pending = true;
            }
            None => self.shell.status_msg = "Warp: không còn bước nào để lùi".to_string(),
        }
        self.redraw_main();
        true
    }

    /// The bent picture and the frame it lies in, `(pixels, x, y, w, h)`.
    fn grid_warp_picture(state: &GridWarpState) -> (Vec<u8>, i32, i32, u32, u32) {
        let (x0, y0, x1, y1) = grid_warp::bounds(&state.net);
        let fx0 = ((x0.floor() as i32) - 1).max(state.room.0);
        let fy0 = ((y0.floor() as i32) - 1).max(state.room.1);
        let fx1 = ((x1.ceil() as i32) + 1).min(state.room.2);
        let fy1 = ((y1.ceil() as i32) + 1).min(state.room.3);
        let (fw, fh) = ((fx1 - fx0).max(1) as u32, (fy1 - fy0).max(1) as u32);
        let src = Source {
            rgba: &state.flat,
            width: state.width,
            height: state.height,
            offset: state.offset,
            frame: state.frame,
        };
        let pixels = grid_warp::render(&src, &state.net, (fx0, fy0, fw, fh), Quality::Fine);
        (pixels, fx0, fy0, fw, fh)
    }

    /// Draw what the net moved since the last frame, once a frame: as a
    /// draft while the drag goes on, finely after; only the part that
    /// changed goes into the layer and to the screen.
    pub(crate) fn grid_warp_flush(&mut self) {
        let zoom = self.edit.view.zoom;
        let Some(idx) = self.grid_warp_doc_idx() else {
            return;
        };
        let Some(state) = self.edit.grid_warp.as_mut() else {
            return;
        };
        if !state.pending {
            return;
        }
        state.pending = false;
        let room = state.room_frame();
        let now = grid_warp::covered(&state.net, room);
        let mut dirty = union(state.shown, now);
        let quality = if state.grab.is_some() {
            Quality::Draft(grid_warp::draft_step(zoom))
        } else {
            if let Some(rough) = state.rough.take() {
                dirty = union(dirty, rough);
            }
            Quality::Fine
        };
        if dirty.2 <= dirty.0 || dirty.3 <= dirty.1 {
            state.shown = now;
            return;
        }
        let src = Source {
            rgba: &state.flat,
            width: state.width,
            height: state.height,
            offset: state.offset,
            frame: state.frame,
        };
        grid_warp::render_into(&src, &state.net, room, &mut state.work, dirty, quality);
        if quality != Quality::Fine {
            state.rough = Some(union(state.rough.unwrap_or((0, 0, 0, 0)), dirty));
        }
        state.shown = now;
        let (x0, y0, x1, y1) = dirty;
        let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let stride = room.2 as usize * 4;
        let mut part = vec![0u8; w * h * 4];
        for (row, line) in part.chunks_exact_mut(w * 4).enumerate() {
            let at = (y0 as usize + row) * stride + x0 as usize * 4;
            line.copy_from_slice(&state.work[at..at + w * 4]);
        }
        let layer_id = state.layer_id;
        let canvas = &mut self.docs.documents[idx].canvas;
        let Some(layer) = canvas
            .layer_stack
            .layers
            .iter_mut()
            .find(|l| l.id == layer_id)
        else {
            return;
        };
        layer.tiles.write_region(x0, y0, w as u32, h as u32, &part);
        canvas.layer_revision += 1;
        canvas.pixels_stale = true;
        self.warp_recomposite_canvas_rect(
            (room.0 + x0 as i32) as f32,
            (room.1 + y0 as i32) as f32,
            w as f32,
            h as f32,
        );
    }

    /// Put the layer back as it was when the warp opened.
    fn grid_warp_put_back(&mut self, state: GridWarpState) -> Option<usize> {
        let idx = self.docs.index_of(state.doc_id)?;
        let canvas = &mut self.docs.documents[idx].canvas;
        if let Some(layer) = canvas
            .layer_stack
            .layers
            .iter_mut()
            .find(|l| l.id == state.layer_id)
        {
            layer.tiles = state.tiles;
            layer.width = state.width;
            layer.height = state.height;
            layer.offset = state.offset;
        }
        canvas.layer_revision += 1;
        canvas.dirty.expand_full(canvas.width, canvas.height);
        Some(idx)
    }

    /// Apply the warp: one undo step.
    pub(crate) fn commit_grid_warp(&mut self) {
        let Some(state) = self.edit.grid_warp.take() else {
            return;
        };
        let unbent = !state.bent
            || state.net
                == grid_warp::identity(state.frame.0, state.frame.1, state.frame.2, state.frame.3);
        if unbent {
            if let Some(idx) = self.grid_warp_put_back(state) {
                self.docs.documents[idx].canvas.flatten_full();
            }
            self.shell.status_msg = "Warp: chưa uốn gì".to_string();
            self.after_grid_warp();
            return;
        }
        let (pixels, x, y, w, h) = Self::grid_warp_picture(&state);
        let layer_id = state.layer_id;
        let Some(idx) = self.grid_warp_put_back(state) else {
            return;
        };
        let canvas = &mut self.docs.documents[idx].canvas;
        let mut step = crate::core::command::LayerStructureCommand::capture_before(
            "Warp",
            &canvas.layer_stack,
            canvas.width,
            canvas.height,
        );
        if let Some(layer) = canvas
            .layer_stack
            .layers
            .iter_mut()
            .find(|l| l.id == layer_id)
        {
            layer.tiles = TileMap::from_rgba(&pixels, w, h);
            layer.width = w;
            layer.height = h;
            layer.offset = (x, y);
        }
        step.capture_after(&canvas.layer_stack, canvas.width, canvas.height);
        canvas.record(Box::new(step));
        canvas.layer_revision += 1;
        canvas.dirty.expand_full(canvas.width, canvas.height);
        canvas.flatten_full();
        // The layer's pixels are new: an earlier transform's kept source no
        // longer describes them.
        self.docs.documents[idx].transform_lineage.remove(&layer_id);
        self.shell.status_msg = "Đã áp dụng Warp".to_string();
        self.after_grid_warp();
    }

    /// Close the warp, the layer as it was.
    pub(crate) fn cancel_grid_warp(&mut self) {
        let Some(state) = self.edit.grid_warp.take() else {
            return;
        };
        if let Some(idx) = self.grid_warp_put_back(state) {
            self.docs.documents[idx].canvas.flatten_full();
        }
        self.after_grid_warp();
    }

    fn after_grid_warp(&mut self) {
        self.edit.tools.select(crate::tools::ToolId::Move);
        self.apply_canvas_event(CanvasEvent::LayerStructureChanged);
        self.redraw_main();
    }
}

#[cfg(test)]
pub(in crate::app) mod tests {
    use super::*;

    /// A layer of the whole page with a pattern, for the warps to bend.
    pub(in crate::app) fn patterned_app() -> App {
        let mut app = App::new();
        app.shell.ui.show_welcome = false;
        let canvas = &mut app.docs.active_mut().canvas;
        let (w, h) = (canvas.width, canvas.height);
        let idx = canvas.layer_stack.active_idx;
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, (i / w % 199) as u8, 60, 255])
            .collect();
        canvas.layer_stack.layers[idx].tiles = TileMap::from_rgba(&rgba, w, h);
        app.edit.view.zoom = 0.2;
        app.edit.view.offset_x = 300.0;
        app.edit.view.offset_y = 300.0;
        app
    }

    /// 16/11: a drag draws a draft once a frame, the part it moved only;
    /// letting go draws it finely. The layer then holds exactly the fine
    /// picture of the whole room, and Enter keeps it.
    #[test]
    fn a_drag_drawn_in_parts_ends_as_the_fine_picture() {
        let mut app = patterned_app();
        assert!(app.begin_grid_warp());
        let at = |app: &App, p: (f32, f32)| {
            (
                p.0 * app.edit.view.zoom + app.edit.view.offset_x,
                p.1 * app.edit.view.zoom + app.edit.view.offset_y,
            )
        };
        // Pull a handle, then the surface: drafts in between frames.
        let net = app.edit.grid_warp.as_ref().expect("open").net;
        let (sx, sy) = at(&app, net[5]);
        app.edit.input.mouse_x = sx;
        app.edit.input.mouse_y = sy;
        app.grid_warp_pointer_down();
        for k in 1..=5 {
            app.edit.input.mouse_x = sx + 7.0 * k as f32;
            app.edit.input.mouse_y = sy - 4.0 * k as f32;
            app.grid_warp_pointer_drag();
            app.grid_warp_flush();
        }
        assert!(app.edit.grid_warp.as_ref().expect("open").rough.is_some());
        app.grid_warp_pointer_up();
        app.grid_warp_flush();
        let state = app.edit.grid_warp.as_ref().expect("open");
        assert!(state.rough.is_none(), "the draft is drawn finely");
        let src = Source {
            rgba: &state.flat,
            width: state.width,
            height: state.height,
            offset: state.offset,
            frame: state.frame,
        };
        let fine = grid_warp::render(&src, &state.net, state.room_frame(), Quality::Fine);
        assert_eq!(state.work, fine, "what the session holds");
        let layer = &app.docs.active().canvas.layer_stack.layers
            [app.docs.active().canvas.layer_stack.active_idx];
        assert_eq!(layer.tiles.flatten(), fine, "what the layer shows");
        app.commit_grid_warp();
        assert!(app.edit.grid_warp.is_none());
    }

    /// 16/11: a corner sits on the picture's edge, half of it out on the
    /// grey pasteboard; a press there takes it, and the drag moves it.
    #[test]
    fn a_corner_on_the_picture_edge_is_taken_and_dragged() {
        let mut app = App::new();
        app.shell.ui.show_welcome = false;
        let canvas = &mut app.docs.active_mut().canvas;
        let (w, h) = (canvas.width, canvas.height);
        let idx = canvas.layer_stack.active_idx;
        canvas.layer_stack.layers[idx].tiles =
            TileMap::from_rgba(&vec![200u8; (w * h * 4) as usize], w, h);
        assert!(app.begin_grid_warp());
        app.edit.view.zoom = 0.5;
        app.edit.view.offset_x = 300.0;
        app.edit.view.offset_y = 300.0;
        app.win.cursor_ownership.pointer_inside = true;
        // Just outside the top-left corner, on the pasteboard.
        let (mx, my) = (295.0, 296.0);
        app.edit.input.mouse_x = mx;
        app.edit.input.mouse_y = my;
        app.refresh_pointer_ui_state(mx, my);
        assert!(!app.edit.input.was_over_ui, "the pasteboard is not UI");
        app.grid_warp_pointer_down();
        assert!(app.grid_warp_dragging());
        app.edit.input.mouse_x = 275.0;
        app.edit.input.mouse_y = 286.0;
        app.grid_warp_pointer_drag();
        app.grid_warp_pointer_up();
        let net = app.edit.grid_warp.as_ref().expect("open").net;
        assert!((net[0].0 - -40.0).abs() < 1e-3 && (net[0].1 - -20.0).abs() < 1e-3);
        app.cancel_grid_warp();
    }
}
