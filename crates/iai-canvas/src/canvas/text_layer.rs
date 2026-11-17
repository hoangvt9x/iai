//! A text layer's pixels, made from its text.

use iai_doc::layer::LayerType;
use iai_doc::model::text::TextData;
use iai_raster::tile::TileMap;
use iai_text::text::rasterize_placed;

use super::Canvas;

/// Rasterize `td` and replace the layer's tiles with it. The text is written at
/// tile-local (0,0); `origin` is the upright text anchor, and rotated tiles
/// receive a bbox offset. Also updates the layer name from the content, as
/// long as the name is still the one the words gave it (a layer the person
/// named keeps its name, as in Photoshop).
///
/// The tiles are raster-sized (not canvas-sized): a canvas-sized TileMap would
/// silently crop text extending past the canvas edge, and the transform-commit
/// path already produces raster-sized text layers — width/height must track
/// the tiles for hit-testing and .iai export.
pub fn rasterize_into_layer(canvas: &mut Canvas, idx: usize, origin: (i32, i32), td: TextData) {
    let (cw, ch) = (canvas.width, canvas.height);
    let (tiles, w, h, offset) = match rasterize_placed(&td) {
        Some((raster, delta)) => (
            TileMap::from_rgba(&raster.rgba, raster.width, raster.height),
            raster.width,
            raster.height,
            (
                origin.0.saturating_add(delta.0),
                origin.1.saturating_add(delta.1),
            ),
        ),
        None => (TileMap::new(cw, ch), cw, ch, origin),
    };
    let name = layer_name_from(&td.content);
    let layer = &mut canvas.layer_stack.layers[idx];
    if name_follows_words(&layer.name, &layer.layer_type) {
        layer.name = name;
    }
    layer.tiles = tiles;
    layer.width = w;
    layer.height = h;
    layer.offset = offset;
    layer.layer_type = LayerType::Text(td);
}

/// Whether a layer named `name` takes its name from its words: a new one
/// ("Text", or no name), or a type layer whose name is still its words'.
fn name_follows_words(name: &str, layer_type: &LayerType) -> bool {
    let name = name.trim();
    name.is_empty()
        || name == "Text"
        || match layer_type {
            LayerType::Text(old) => name == layer_name_from(&old.content),
            _ => true,
        }
}

/// First line of the text, trimmed and truncated, for the layer panel name.
pub fn layer_name_from(content: &str) -> String {
    let first = content.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "Text".to_string();
    }
    let truncated: String = first.chars().take(24).collect();
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_layer_named_by_hand_keeps_its_name() {
        let mut canvas = Canvas::new(200, 100);
        let idx = canvas.layer_stack.add_layer(200, 100);
        let td = |words: &str| TextData {
            content: words.to_string(),
            font_px: 20.0,
            ..TextData::default()
        };
        canvas.layer_stack.layers[idx].name = "Text".to_string();
        rasterize_into_layer(&mut canvas, idx, (10, 10), td("Hello"));
        assert_eq!(canvas.layer_stack.layers[idx].name, "Hello");
        // Still its words' name: it follows the words.
        rasterize_into_layer(&mut canvas, idx, (10, 10), td("Hello there"));
        assert_eq!(canvas.layer_stack.layers[idx].name, "Hello there");
        // Named by hand: kept.
        canvas.layer_stack.layers[idx].name = "Title".to_string();
        rasterize_into_layer(&mut canvas, idx, (10, 10), td("Goodbye"));
        assert_eq!(canvas.layer_stack.layers[idx].name, "Title");
    }
}
