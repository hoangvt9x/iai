//! Disk cache of finished RAW decodes, across sessions (like Camera Raw's
//! cache): opening a RAW that was decoded before reads its scene master and
//! default-look pixels back instead of demosaicing again.
//!
//! Off until the app calls [`configure`]. An entry is keyed by the file (path,
//! size, modification time), the decode recipe, the app build and every `IAI_*`
//! environment override, so a changed file, a new build or a different decode
//! path never reuses old pixels. Entries are written on a background thread
//! after a decode, read back by [`lookup`] inside the decode once the per-file
//! colour constants are known, and dropped oldest-first past the size budget.

use iai_canvas::canvas::Canvas;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAGIC: &[u8; 8] = b"IAIRAWC1";
const EXT: &str = "rawc";
/// f16 1.0, the alpha of every opaque scene-master pixel.
const SCENE_OPAQUE: u16 = 0x3c00;
/// Rows packed or unpacked per I/O call.
const ROWS_PER_CHUNK: usize = 64;

/// Where the cache lives and how much of the disk it may use.
#[derive(Clone, Debug)]
pub struct RawCacheConfig {
    pub dir: PathBuf,
    /// Total bytes the entries may take; the oldest go first past it.
    pub budget: u64,
    /// Free space the volume must keep after a write, or nothing is written.
    pub free_reserve: u64,
    /// The app build: a new build never reads an older build's pixels.
    pub revision: String,
}

static CONFIG: Mutex<Option<RawCacheConfig>> = Mutex::new(None);
/// One write at a time: each one streams hundreds of MB.
static WRITING: Mutex<()> = Mutex::new(());

/// Turn the cache on (`Some`) or off (`None`).
pub fn configure(config: Option<RawCacheConfig>) {
    if let Ok(mut slot) = CONFIG.lock() {
        *slot = config;
    }
}

fn config() -> Option<RawCacheConfig> {
    CONFIG.lock().ok().and_then(|slot| slot.clone())
}

/// A cache hit: the oriented scene master (RGBA f16 bits, opaque) and its
/// default-look render (RGBA16), plus the one fitted colour constant that is
/// not recomputed from the file.
pub(crate) struct CachedRaw {
    pub width: u32,
    pub height: u32,
    pub half: Vec<u16>,
    pub px16: Vec<u16>,
    pub camera_rgb_curve: Option<Box<[[f32; 256]; 3]>>,
}

/// What an entry must match: the file as it is on disk now, the recipe, the
/// build and the environment overrides.
fn identity(path: &Path, recipe: &str, revision: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let mut overrides: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("IAI_") && k != "IAI_RAW_TIMING")
        .collect();
    overrides.sort();
    let path = path.to_string_lossy().to_lowercase();
    Some(format!(
        "{path}|{}|{modified}|{recipe}|{revision}|{overrides:?}",
        meta.len()
    ))
}

fn entry_path(dir: &Path, identity: &str) -> PathBuf {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in identity.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    dir.join(format!("{hash:016x}.{EXT}"))
}

/// Read the cached decode of `path`, if there is a current one.
pub(crate) fn lookup(path: &Path, recipe: &str) -> Option<CachedRaw> {
    let config = config()?;
    let identity = identity(path, recipe, &config.revision)?;
    let file_path = entry_path(&config.dir, &identity);
    let mut file = File::open(&file_path).ok()?;
    let hit = read_entry(&mut file, &identity);
    if hit.is_none() {
        drop(file);
        let _ = std::fs::remove_file(&file_path);
        return None;
    }
    // Most recently used: the budget sweep drops the oldest first.
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&file_path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
    hit
}

/// Write `canvas` (a fresh full RAW decode of `path`) to the cache on a
/// background thread. No-op when the cache is off or the canvas is not a
/// single opaque RAW layer with its scene master.
pub(crate) fn store(path: &Path, canvas: &Canvas, recipe: &str) {
    let Some(config) = config() else {
        return;
    };
    let Some(scene) = canvas.develop_source.clone() else {
        return;
    };
    let layers = &canvas.layer_stack.layers;
    if layers.len() != 1
        || scene.alpha.is_some()
        || scene.width != canvas.width
        || scene.height != canvas.height
        || scene.half.len() != canvas.width as usize * canvas.height as usize * 4
    {
        return;
    }
    let Some(identity) = identity(path, recipe, &config.revision) else {
        return;
    };
    let tiles = layers[0].tiles.clone();
    let _ = std::thread::Builder::new()
        .name("iai-raw-cache".to_string())
        .spawn(move || {
            let _one_at_a_time = WRITING.lock();
            let target = entry_path(&config.dir, &identity);
            if target.exists() {
                return;
            }
            let pixels = scene.width as u64 * scene.height as u64;
            let bytes = pixels * 12 + 4096;
            if bytes > config.budget || std::fs::create_dir_all(&config.dir).is_err() {
                return;
            }
            let free = iai_platform::hw::free_disk_bytes(&config.dir).unwrap_or(0);
            if bytes.saturating_add(config.free_reserve) > free {
                return;
            }
            sweep(&config.dir, config.budget.saturating_sub(bytes));
            let tmp = target.with_extension("part");
            let written = File::create(&tmp)
                .map_err(|e| e.to_string())
                .and_then(|mut file| write_entry(&mut file, &identity, &scene, &tiles));
            if written.is_ok() {
                if std::fs::rename(&tmp, &target).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        });
}

/// Delete the least recently used entries (and any abandoned partial write)
/// until the rest fit in `keep` bytes.
fn sweep(dir: &Path, keep: u64) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        match path.extension().and_then(|e| e.to_str()) {
            Some(EXT) => entries.push((
                meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                meta.len(),
                path,
            )),
            Some("part") => {
                // Another write in progress holds WRITING, so a .part seen
                // here is left over from a crash.
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
    entries.sort_by_key(|(modified, _, _)| *modified);
    let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();
    for (_, len, path) in entries {
        if total <= keep {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

fn write_entry(
    file: &mut File,
    identity: &str,
    scene: &Arc<iai_develop::develop_scene::SceneSource>,
    tiles: &iai_raster::tile::TileMap,
) -> Result<(), String> {
    let (w, h) = (scene.width as usize, scene.height as usize);
    if !scene.half.chunks_exact(4).all(|px| px[3] == SCENE_OPAQUE) {
        return Err("scene has transparency".to_string());
    }
    let io = |e: std::io::Error| e.to_string();
    let mut head = Vec::with_capacity(4096);
    head.extend_from_slice(MAGIC);
    head.extend_from_slice(&(identity.len() as u32).to_le_bytes());
    head.extend_from_slice(identity.as_bytes());
    head.extend_from_slice(&scene.width.to_le_bytes());
    head.extend_from_slice(&scene.height.to_le_bytes());
    match &scene.camera_rgb_curve {
        Some(curve) => {
            head.push(1);
            for channel in curve.iter() {
                for v in channel {
                    head.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        None => head.push(0),
    }
    file.write_all(&head).map_err(io)?;
    let mut buf: Vec<u16> = Vec::with_capacity(ROWS_PER_CHUNK * w * 3);
    for chunk in scene.half.chunks(ROWS_PER_CHUNK * w * 4) {
        pack_rgb(chunk, &mut buf);
        file.write_all(bytemuck::cast_slice(&buf)).map_err(io)?;
    }
    let mut band = vec![0u16; ROWS_PER_CHUNK * w * 4];
    let mut y = 0;
    while y < h {
        let rows = ROWS_PER_CHUNK.min(h - y);
        let band = &mut band[..rows * w * 4];
        tiles.flatten16_region_into(0, y as u32, w as u32, rows as u32, band);
        if band.chunks_exact(4).any(|px| px[3] != u16::MAX) {
            return Err("look has transparency".to_string());
        }
        pack_rgb(band, &mut buf);
        file.write_all(bytemuck::cast_slice(&buf)).map_err(io)?;
        y += rows;
    }
    file.flush().map_err(io)
}

fn pack_rgb(rgba: &[u16], out: &mut Vec<u16>) {
    out.clear();
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
}

fn read_entry(file: &mut File, identity: &str) -> Option<CachedRaw> {
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic).ok()?;
    if &magic != MAGIC {
        return None;
    }
    let mut word = [0u8; 4];
    file.read_exact(&mut word).ok()?;
    let len = u32::from_le_bytes(word) as usize;
    if len != identity.len() {
        return None;
    }
    let mut stored = vec![0u8; len];
    file.read_exact(&mut stored).ok()?;
    if stored != identity.as_bytes() {
        return None;
    }
    file.read_exact(&mut word).ok()?;
    let width = u32::from_le_bytes(word);
    file.read_exact(&mut word).ok()?;
    let height = u32::from_le_bytes(word);
    if width == 0 || height == 0 || (width as u64) * (height as u64) > 1 << 31 {
        return None;
    }
    let mut flag = [0u8; 1];
    file.read_exact(&mut flag).ok()?;
    let camera_rgb_curve = if flag[0] == 1 {
        let mut curve = Box::new([[0f32; 256]; 3]);
        for channel in curve.iter_mut() {
            for v in channel.iter_mut() {
                file.read_exact(&mut word).ok()?;
                *v = f32::from_le_bytes(word);
            }
        }
        Some(curve)
    } else {
        None
    };
    let (w, pixels) = (width as usize, width as usize * height as usize);
    let half = read_rgb(file, pixels, w, SCENE_OPAQUE).ok()?;
    let px16 = read_rgb(file, pixels, w, u16::MAX).ok()?;
    // Nothing may follow: a longer file is not what this build wrote.
    if file.read(&mut flag).ok()? != 0 {
        return None;
    }
    Some(CachedRaw {
        width,
        height,
        half,
        px16,
        camera_rgb_curve,
    })
}

fn read_rgb(file: &mut File, pixels: usize, width: usize, alpha: u16) -> std::io::Result<Vec<u16>> {
    let mut rgba = vec![alpha; pixels * 4];
    let mut buf = vec![0u16; ROWS_PER_CHUNK * width * 3];
    for chunk in rgba.chunks_mut(ROWS_PER_CHUNK * width * 4) {
        let packed = &mut buf[..chunk.len() / 4 * 3];
        file.read_exact(bytemuck::cast_slice_mut(packed))?;
        for (dst, src) in chunk.chunks_exact_mut(4).zip(packed.chunks_exact(3)) {
            dst[..3].copy_from_slice(src);
        }
    }
    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iai_develop::develop_scene::{SceneSource, f32_to_f16_bits};

    fn raw_like(w: u32, h: u32) -> Canvas {
        let n = (w * h) as usize;
        let mut half = vec![0u16; n * 4];
        let mut px16 = vec![0u16; n * 4];
        for i in 0..n {
            for c in 0..3 {
                half[i * 4 + c] = f32_to_f16_bits((i * 3 + c) as f32 * 0.001 - 0.2);
                px16[i * 4 + c] = ((i * 7 + c * 13) % 65536) as u16;
            }
            half[i * 4 + 3] = SCENE_OPAQUE;
            px16[i * 4 + 3] = u16::MAX;
        }
        let mut canvas = Canvas::from_rgba16(px16, w, h);
        let mut scene = SceneSource::new(w, h);
        scene.half = half;
        let mut curve = Box::new([[0f32; 256]; 3]);
        curve[1][7] = 0.25;
        scene.camera_rgb_curve = Some(curve);
        canvas.develop_source = Some(Arc::new(scene));
        canvas
    }

    #[test]
    fn an_entry_reads_back_exactly_and_only_for_its_identity() {
        let dir = std::env::temp_dir().join(format!("iai-raw-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let canvas = raw_like(70, 133);
        let scene = canvas.develop_source.clone().unwrap();
        let path = dir.join("entry.rawc");
        let mut file = File::create(&path).unwrap();
        write_entry(
            &mut file,
            "id-a",
            &scene,
            &canvas.layer_stack.layers[0].tiles,
        )
        .unwrap();
        drop(file);

        let hit = read_entry(&mut File::open(&path).unwrap(), "id-a").unwrap();
        assert_eq!((hit.width, hit.height), (70, 133));
        assert_eq!(hit.half, scene.half);
        assert_eq!(hit.px16, canvas.layer_stack.layers[0].tiles.flatten16());
        assert_eq!(hit.camera_rgb_curve, scene.camera_rgb_curve);
        // Rebuilt the way the decode does, the tiles are the same tiles.
        let again = Canvas::from_rgba16(hit.px16, 70, 133);
        assert_eq!(
            again.layer_stack.layers[0].tiles.flatten16(),
            canvas.layer_stack.layers[0].tiles.flatten16()
        );
        assert_eq!(
            again.layer_stack.layers[0].tiles.flatten(),
            canvas.layer_stack.layers[0].tiles.flatten()
        );

        assert!(read_entry(&mut File::open(&path).unwrap(), "id-b").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sweep_drops_the_oldest_entries_first() {
        let dir = std::env::temp_dir().join(format!("iai-raw-sweep-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let path = dir.join(format!("{name}.{EXT}"));
            std::fs::write(&path, vec![0u8; 100]).unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(now - std::time::Duration::from_secs(100 - i as u64 * 10))
                .unwrap();
        }
        std::fs::write(dir.join("x.part"), [0u8; 10]).unwrap();
        sweep(&dir, 200);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["b.rawc", "c.rawc"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
