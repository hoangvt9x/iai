//! Live PowerClip rendering (Bước 6, feature layer).
//!
//! The clipping *relation* is `Layer::clip_parent_id` (see [`super::clip_ops`]);
//! this turns that relation into something visible. Rather than teach the GPU
//! compositor to sample a second layer's alpha (a large shader change), we bake
//! the frame's coverage into a *managed raster mask* on each content layer. The
//! existing mask path — which both the GPU shader and the CPU flatten already
//! honour — then clips the content for free, with no shader changes.
//!
//! The mask is DERIVED state, like the vector raster cache: it is rebuilt from
//! the frame whenever geometry changes (called on the structural recomposite
//! path and on load) and is never the source of truth. Because it is baked into
//! the content layer's OWN local space, moving the content re-samples the frame
//! at the content's new position on the next refresh, so the clip stays pinned
//! to the frame's shape.

use rayon::prelude::*;

use super::Canvas;
use iai_doc::layer::{Layer, LayerMask};
use iai_raster::tile::TileMap;

impl Canvas {
    /// Whether any layer is PowerClip content (has a clip frame). A cheap gate so
    /// the common no-clip document pays nothing.
    pub fn has_clip_content(&self) -> bool {
        self.layer_stack
            .layers
            .iter()
            .any(|l| l.clip_parent_id.is_some())
    }

    /// Fingerprint of the clip-relevant geometry: each content layer's identity/
    /// offset/size and its frame's identity/offset/size/coverage. Content PIXELS
    /// are intentionally excluded — painting inside a clip does not move the clip.
    fn clip_state_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for layer in &self.layer_stack.layers {
            let Some(fid) = layer.clip_parent_id else {
                continue;
            };
            // Content being edited in place is intentionally left unclipped.
            if self.powerclip_edit_frame == Some(fid) {
                continue;
            }
            layer.id.hash(&mut h);
            layer.offset.hash(&mut h);
            layer.width.hash(&mut h);
            layer.height.hash(&mut h);
            if let Some(frame) = self.layer_stack.layers.iter().find(|l| l.id == fid) {
                frame.id.hash(&mut h);
                frame.offset.hash(&mut h);
                frame.width.hash(&mut h);
                frame.height.hash(&mut h);
                frame.tiles.revision_fingerprint().hash(&mut h);
            }
        }
        h.finish()
    }

    /// Rebuild the managed clip mask on every PowerClip content layer from its
    /// frame's current coverage. No-op (returns `false`) when nothing is clipped
    /// or nothing clip-relevant changed since the last bake. Mutates masks
    /// directly — this is derived state, outside history.
    pub fn refresh_clip_masks(&mut self) -> bool {
        if !self.has_clip_content() {
            self.clip_fp = 0;
            return false;
        }
        let fp = self.clip_state_fingerprint();
        if fp == self.clip_fp {
            return false;
        }
        self.clip_fp = fp;
        // Read pass: bake each content layer's mask against its frame. Collect
        // first so we never hold an immutable borrow (the frame) while mutating a
        // content layer.
        let mut baked: Vec<(usize, LayerMask)> = Vec::new();
        let mut shifted: Vec<(usize, (i32, i32), (i32, i32))> = Vec::new();
        for (i, layer) in self.layer_stack.layers.iter().enumerate() {
            let Some(fid) = layer.clip_parent_id else {
                continue;
            };
            // Skip the frame whose contents are being edited in place — it stays
            // unclipped until the user finishes editing.
            if self.powerclip_edit_frame == Some(fid) {
                continue;
            }
            let Some(frame) = self.layer_stack.layers.iter().find(|l| l.id == fid) else {
                continue;
            };
            // Frame and content moved together (both picked, or a group
            // dragged): the mask is the same pixels, only its place is new.
            // Baking it again on every drag frame was the Move lag of 11/11.
            if let Some(mask) = layer.mask.as_ref()
                && mask_still_fits(mask, layer, frame)
            {
                shifted.push((i, layer.offset, frame.offset));
                continue;
            }
            baked.push((
                i,
                bake_clip_mask(frame, layer.offset, layer.width, layer.height),
            ));
        }
        for (i, offset, frame_offset) in shifted {
            if let Some(mask) = self.layer_stack.layers[i].mask.as_mut() {
                mask.bake_offset = offset;
                mask.bake_frame_offset = frame_offset;
            }
        }
        let any = !baked.is_empty();
        for (i, mask) in baked {
            let layer = &self.layer_stack.layers[i];
            if let Some(frame) = layer
                .clip_parent_id
                .and_then(|fid| self.layer_stack.layers.iter().find(|l| l.id == fid))
            {
                remember_bake(&mask, layer, frame);
            }
            self.layer_stack.layers[i].mask = Some(mask);
        }
        any
    }
}

/// What a baked clip mask was made from: the frame's pixels and sizes, and
/// where the content sat against the frame.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BakeSource {
    frame_id: u32,
    frame_tiles: u64,
    frame_size: (u32, u32),
    content_size: (u32, u32),
    content_minus_frame: (i32, i32),
}

impl BakeSource {
    fn of(content: &Layer, frame: &Layer) -> Self {
        Self {
            frame_id: frame.id,
            frame_tiles: frame.tiles.revision_fingerprint(),
            frame_size: (frame.width, frame.height),
            content_size: (content.width, content.height),
            content_minus_frame: (
                content.offset.0 - frame.offset.0,
                content.offset.1 - frame.offset.1,
            ),
        }
    }
}

thread_local! {
    /// Each baked mask (by its tiles' revisions, unique to one bake) and what
    /// it was baked from.
    static BAKES: std::cell::RefCell<Vec<(u64, BakeSource)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn remember_bake(mask: &LayerMask, content: &Layer, frame: &Layer) {
    let key = mask.tiles.revision_fingerprint();
    let source = BakeSource::of(content, frame);
    BAKES.with(|b| {
        let mut b = b.borrow_mut();
        b.retain(|e| e.0 != key);
        if b.len() >= 64 {
            b.remove(0);
        }
        b.push((key, source));
    });
}

/// Whether `mask` was baked from this very frame with the content in the same
/// place against it, so baking again would give the same pixels.
fn mask_still_fits(mask: &LayerMask, content: &Layer, frame: &Layer) -> bool {
    if mask.width != content.width || mask.height != content.height || mask.inverted {
        return false;
    }
    let key = mask.tiles.revision_fingerprint();
    let source = BakeSource::of(content, frame);
    BAKES.with(|b| b.borrow().iter().any(|e| e.0 == key && e.1 == source))
}

/// Build a layer-local mask whose value at `(lx, ly)` is the frame's coverage
/// (alpha) at the same CANVAS position, so the content shows only inside the
/// frame's shape. Areas outside the frame are hidden.
fn bake_clip_mask(frame: &Layer, content_offset: (i32, i32), cw: u32, ch: u32) -> LayerMask {
    use iai_raster::tile::{TILE_SIZE, Tile, TilePos, next_tile_revision};
    use std::sync::Arc;
    let (fw, fh) = (frame.width, frame.height);
    let (fox, foy) = frame.offset;
    let ts = TILE_SIZE as usize;

    // The frame's alpha once, as a flat buffer, read tile row by tile row (a
    // hash lookup per pixel was most of the bake).
    let mut frame_alpha = vec![0u8; (fw as usize) * (fh as usize)];
    if fw > 0 {
        frame_alpha
            .par_chunks_mut(fw as usize)
            .enumerate()
            .for_each(|(y, row)| {
                let ty = (y / ts) as i32;
                let r = y % ts;
                for (tx, part) in row.chunks_mut(ts).enumerate() {
                    let pos = TilePos {
                        x: tx as i32,
                        y: ty,
                    };
                    if let Some(tile) = frame.tiles.tiles.get(&pos) {
                        let src = &tile.pixels[r * ts * 4..(r + 1) * ts * 4];
                        for (o, px) in part.iter_mut().zip(src.chunks_exact(4)) {
                            *o = px[3];
                        }
                    }
                }
            });
    }

    // Each mask tile straight from that buffer, in parallel; inside the layer
    // a pixel is (a, a, a, 255), past its edge (0, 0, 0, 0), as before.
    let rev = next_tile_revision();
    let (cols, rows) = (cw.div_ceil(TILE_SIZE), ch.div_ceil(TILE_SIZE));
    let tiles: Vec<(TilePos, Tile)> = (0..rows * cols)
        .into_par_iter()
        .map(|k| {
            let (tx, ty) = ((k % cols) as usize, (k / cols) as usize);
            let mut tile = Tile::new_empty();
            tile.revision = rev;
            for r in 0..ts {
                let ly = ty * ts + r;
                if ly >= ch as usize {
                    break;
                }
                let fy = content_offset.1 + ly as i32 - foy;
                let frame_row = (fy >= 0 && (fy as u32) < fh).then(|| {
                    &frame_alpha[fy as usize * fw as usize..(fy as usize + 1) * fw as usize]
                });
                let out = &mut tile.pixels[r * ts * 4..(r + 1) * ts * 4];
                for c in 0..ts {
                    let lx = tx * ts + c;
                    if lx >= cw as usize {
                        break;
                    }
                    let fx = content_offset.0 + lx as i32 - fox;
                    let a = match frame_row {
                        Some(row) if fx >= 0 && (fx as u32) < fw => row[fx as usize],
                        _ => 0,
                    };
                    out[c * 4..c * 4 + 4].copy_from_slice(&[a, a, a, 255]);
                }
            }
            let pos = TilePos {
                x: tx as i32,
                y: ty as i32,
            };
            (pos, tile)
        })
        .collect();
    let mut map = TileMap::new(cw, ch);
    for (pos, tile) in tiles {
        map.tiles.insert(pos, Arc::new(tile));
    }

    LayerMask {
        tiles: map,
        width: cw,
        height: ch,
        enabled: true,
        inverted: false,
        // Remember where the content AND its frame sat when this mask was baked.
        // The live compositor pins the clip with
        //   shift = (content.offset − bake_offset) − (frame.offset − bake_frame_offset)
        // so dragging either the image or the frame keeps the clip live with no
        // per-frame re-bake (that per-frame re-bake was the Move lag).
        bake_offset: content_offset,
        bake_frame_offset: (fox, foy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iai_raster::tile::TileMap;

    /// A frame layer: opaque disc-ish square in the middle of a 40×40 canvas.
    fn make_frame(stack_w: u32, stack_h: u32) -> (Canvas, u32, u32) {
        let mut canvas = Canvas::new(stack_w, stack_h);
        // Frame: a 20×20 opaque square placed at (10,10).
        let frame_idx = canvas.layer_stack.add_layer(stack_w, stack_h);
        let opaque = vec![255u8; 20 * 20 * 4];
        {
            let f = &mut canvas.layer_stack.layers[frame_idx];
            f.tiles = TileMap::from_rgba(&opaque, 20, 20);
            f.width = 20;
            f.height = 20;
            f.offset = (10, 10);
        }
        let frame_id = canvas.layer_stack.layers[frame_idx].id;

        // Content: a full-canvas opaque layer.
        let content_idx = canvas.layer_stack.add_layer(stack_w, stack_h);
        let full = vec![255u8; (stack_w * stack_h * 4) as usize];
        {
            let c = &mut canvas.layer_stack.layers[content_idx];
            c.tiles = TileMap::from_rgba(&full, stack_w, stack_h);
            c.width = stack_w;
            c.height = stack_h;
            c.offset = (0, 0);
        }
        let content_id = canvas.layer_stack.layers[content_idx].id;
        canvas
            .layer_stack
            .attach_clipped_child(content_id, frame_id)
            .unwrap();
        (canvas, frame_id, content_id)
    }

    #[test]
    fn clip_mask_reveals_only_inside_the_frame() {
        let (mut canvas, _frame_id, content_id) = make_frame(40, 40);
        assert!(canvas.refresh_clip_masks(), "something was clipped");
        let content = canvas
            .layer_stack
            .layers
            .iter()
            .find(|l| l.id == content_id)
            .unwrap();
        let mask = content.mask.as_ref().expect("clip mask baked");
        // Inside the frame square (15,15) → revealed.
        assert!(mask.sample(15, 15) > 0.9, "inside frame revealed");
        // Outside the frame (2,2) and (35,35) → hidden.
        assert!(mask.sample(2, 2) < 0.1, "top-left hidden");
        assert!(mask.sample(35, 35) < 0.1, "bottom-right hidden");
        // Frame edge boundary: (9,9) just outside, (10,10) just inside.
        assert!(mask.sample(9, 9) < 0.1);
        assert!(mask.sample(10, 10) > 0.9);
    }

    #[test]
    fn frame_and_content_moved_together_keep_their_mask() {
        let (mut canvas, frame_id, content_id) = make_frame(300, 280);
        canvas.refresh_clip_masks();
        let idx = |c: &Canvas, id: u32| {
            c.layer_stack
                .layers
                .iter()
                .position(|l| l.id == id)
                .unwrap()
        };
        let (fi, ci) = (idx(&canvas, frame_id), idx(&canvas, content_id));
        let before = canvas.layer_stack.layers[ci]
            .mask
            .as_ref()
            .unwrap()
            .tiles
            .revision_fingerprint();
        for i in [fi, ci] {
            canvas.layer_stack.layers[i].offset.0 += 7;
            canvas.layer_stack.layers[i].offset.1 -= 3;
        }
        canvas.refresh_clip_masks();
        let mask = canvas.layer_stack.layers[ci].mask.clone().unwrap();
        assert_eq!(
            mask.tiles.revision_fingerprint(),
            before,
            "same pixels kept"
        );
        assert_eq!(mask.bake_offset, (7, -3));
        assert_eq!(mask.bake_frame_offset, (17, 7));
        // The same as a fresh bake at the new place.
        let fresh = bake_clip_mask(&canvas.layer_stack.layers[fi], (7, -3), 300, 280);
        for (x, y) in [
            (2, 2),
            (15, 15),
            (10, 10),
            (9, 9),
            (29, 29),
            (30, 30),
            (299, 279),
        ] {
            assert_eq!(mask.sample(x, y), fresh.sample(x, y), "({x},{y})");
        }
        // Content alone moved: baked again, the frame now sits elsewhere on it.
        canvas.layer_stack.layers[ci].offset.0 += 5;
        canvas.refresh_clip_masks();
        let moved = canvas.layer_stack.layers[ci].mask.as_ref().unwrap();
        assert_ne!(moved.tiles.revision_fingerprint(), before);
        assert!(moved.sample(5, 13) > 0.9 && moved.sample(24, 13) > 0.9);
        assert!(moved.sample(4, 13) < 0.1 && moved.sample(25, 13) < 0.1);
    }

    #[test]
    fn no_clip_content_is_a_cheap_noop() {
        let mut canvas = Canvas::new(16, 16);
        canvas.layer_stack.add_layer(16, 16);
        assert!(!canvas.has_clip_content());
        assert!(!canvas.refresh_clip_masks());
    }

    #[test]
    fn clip_mask_records_bake_offsets_for_live_pin() {
        // The live compositor pins a dragged clip by sampling the (unmoved) mask
        // at layer_local + shift, where
        //   shift = (content.offset − bake_offset) − (frame.offset − bake_frame_offset).
        // So the bake must stamp BOTH the content's and the frame's offset; a later
        // move that skips the re-bake then yields the exact live shift.
        let (mut canvas, frame_id, content_id) = make_frame(40, 40);
        canvas.refresh_clip_masks();
        let ci = canvas
            .layer_stack
            .layers
            .iter()
            .position(|l| l.id == content_id)
            .unwrap();
        let fi = canvas
            .layer_stack
            .layers
            .iter()
            .position(|l| l.id == frame_id)
            .unwrap();
        {
            let mask = canvas.layer_stack.layers[ci].mask.as_ref().unwrap();
            assert_eq!(mask.bake_offset, (0, 0), "content offset at bake");
            assert_eq!(mask.bake_frame_offset, (10, 10), "frame offset at bake");
        }

        // Mirror the compositor's shift formula.
        let shift = |canvas: &Canvas| {
            let m = canvas.layer_stack.layers[ci].mask.as_ref().unwrap();
            let c = canvas.layer_stack.layers[ci].offset;
            let f = canvas.layer_stack.layers[fi].offset;
            (
                (c.0 - m.bake_offset.0) - (f.0 - m.bake_frame_offset.0),
                (c.1 - m.bake_offset.1) - (f.1 - m.bake_frame_offset.1),
            )
        };

        // Drag the CONTENT (frame fixed), no re-bake: shift == drag delta.
        canvas.layer_stack.layers[ci].offset = (7, -3);
        assert_eq!(shift(&canvas), (7, -3), "content drag pins by +delta");

        // Drag the FRAME (content back at 0,0), no re-bake: shift == −frame delta,
        // so the clip window follows the frame instead of lagging behind.
        canvas.layer_stack.layers[ci].offset = (0, 0);
        canvas.layer_stack.layers[fi].offset = (15, 12);
        assert_eq!(shift(&canvas), (-5, -2), "frame drag pins by −delta");

        // Re-baking at the new positions resets the shift to zero.
        canvas.refresh_clip_masks();
        assert_eq!(shift(&canvas), (0, 0), "fresh bake ⇒ no shift");
    }

    #[test]
    fn moving_content_repins_clip_to_the_frame() {
        // After shifting the content, a refresh must re-pin the visible region to
        // the frame's fixed canvas position, not drag it along with the content.
        let (mut canvas, _fid, content_id) = make_frame(40, 40);
        canvas.refresh_clip_masks();
        // Shift content right by 10px.
        let ci = canvas
            .layer_stack
            .layers
            .iter()
            .position(|l| l.id == content_id)
            .unwrap();
        canvas.layer_stack.layers[ci].offset = (10, 0);
        canvas.refresh_clip_masks();
        let mask = canvas.layer_stack.layers[ci].mask.as_ref().unwrap();
        // Frame covers canvas x∈[10,30). Content now starts at x=10, so local
        // lx=0 maps to canvas x=10 (inside) and lx=25 maps to x=35 (outside).
        assert!(
            mask.sample(0, 15) > 0.9,
            "content-local 0 is inside frame now"
        );
        assert!(
            mask.sample(25, 15) < 0.1,
            "content-local 25 is outside frame"
        );
    }
}
