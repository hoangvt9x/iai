//! The RAW decode alone, timed stage by stage, with the same fingerprint as the
//! app-level `tests/raw_open_speed_probe.rs`: a quick loop for decode work that
//! builds only this crate and what it uses, not the whole app.
//!
//! ```text
//! IAI_RAW_CORPUS="C:\\path\\to\\raws" IAI_RAW_TIMING=1 \
//!   cargo test --release -p iai-formats --test raw_decode_speed_probe -- --ignored --nocapture
//! ```
//!
//! Optional: `IAI_RAW_SPEED_FILTER` (substring), `IAI_RAW_SPEED_THREADS`,
//! `IAI_RAW_SPEED_REPEAT`.

use std::path::PathBuf;
use std::time::Instant;

use iai_formats::Importer;
use iai_formats::raw::{RawDecodeControl, RawImporter};

fn fnv1a(hash: &mut u64, words: &[u16]) {
    for &word in words {
        *hash ^= word as u64;
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn fingerprint(canvas: &iai_canvas::canvas::Canvas) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    if let Some(scene) = &canvas.develop_source {
        fnv1a(&mut hash, &scene.half);
    }
    fnv1a(&mut hash, &canvas.layer_stack.layers[0].tiles.flatten16());
    hash
}

#[test]
#[ignore = "requires a local RAW corpus via IAI_RAW_CORPUS; slow"]
fn raw_decode_speed() {
    let Some(dir) = std::env::var("IAI_RAW_CORPUS").ok().map(PathBuf::from) else {
        eprintln!("IAI_RAW_CORPUS not set; skipping");
        return;
    };
    // Windows otherwise runs a background console at its power-saving pace.
    iai_platform::hw::opt_out_of_power_throttling();
    let filter = std::env::var("IAI_RAW_SPEED_FILTER").unwrap_or_default();
    let threads: usize = std::env::var("IAI_RAW_SPEED_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .div_ceil(2)
        });
    let repeat: usize = std::env::var("IAI_RAW_SPEED_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    let importer = RawImporter;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && importer.can_import(p))
        .filter(|p| filter.is_empty() || p.to_string_lossy().contains(&filter))
        .collect();
    entries.sort();
    println!("threads={threads}");
    for path in entries
        .iter()
        .flat_map(|p| std::iter::repeat(p).take(repeat))
    {
        let name: String = path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .chars()
            .take(40)
            .collect();
        drop(iai_formats::raw_preview::extract(path));
        let t = Instant::now();
        let draft_at = std::sync::Mutex::new(None);
        let sink = |_draft: iai_canvas::canvas::Canvas| {
            *draft_at.lock().unwrap() = Some(t.elapsed().as_millis());
        };
        let control = RawDecodeControl {
            cancel: None,
            draft: Some(&sink),
        };
        let canvas = pool
            .install(|| iai_formats::raw::decode_raw_controlled(path, control))
            .unwrap();
        let decode_ms = t.elapsed().as_millis();
        let draft = draft_at
            .into_inner()
            .unwrap()
            .map_or("none".to_string(), |ms| format!("{ms} ms"));
        let mp = canvas.width as f64 * canvas.height as f64 / 1e6;
        println!(
            "{name:<40} {mp:>5.1} MP  decode {decode_ms:>6} ms  draft {draft}  hash {:016x}",
            fingerprint(&canvas)
        );
    }
}

/// The cache: a second decode of the same file reads it back from disk and
/// must land on exactly the same pixels and scene master.
#[test]
#[ignore = "requires a local RAW corpus via IAI_RAW_CORPUS; slow"]
fn raw_cache_hit_matches_the_decode() {
    let Some(dir) = std::env::var("IAI_RAW_CORPUS").ok().map(PathBuf::from) else {
        eprintln!("IAI_RAW_CORPUS not set; skipping");
        return;
    };
    iai_platform::hw::opt_out_of_power_throttling();
    let filter = std::env::var("IAI_RAW_SPEED_FILTER").unwrap_or_default();
    let cache = std::env::temp_dir().join(format!("iai-raw-cache-probe-{}", std::process::id()));
    iai_formats::raw_cache::configure(Some(iai_formats::raw_cache::RawCacheConfig {
        dir: cache.clone(),
        budget: 8 << 30,
        free_reserve: 1 << 30,
        revision: "probe".to_string(),
    }));
    let importer = RawImporter;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && importer.can_import(p))
        .filter(|p| filter.is_empty() || p.to_string_lossy().contains(&filter))
        .collect();
    entries.sort();
    for path in &entries {
        let name: String = path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .chars()
            .take(40)
            .collect();
        let t = Instant::now();
        let first =
            iai_formats::raw::decode_raw_controlled(path, RawDecodeControl::default()).unwrap();
        let decode_ms = t.elapsed().as_millis();
        let expected = fingerprint(&first);
        drop(first);
        // The write runs on its own thread; wait for it to land.
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let done = std::fs::read_dir(&cache)
                .map(|d| {
                    d.flatten()
                        .filter(|e| e.path().extension().is_some_and(|x| x == "rawc"))
                        .count()
                })
                .unwrap_or(0);
            let partial = std::fs::read_dir(&cache)
                .map(|d| {
                    d.flatten()
                        .any(|e| e.path().extension().is_some_and(|x| x == "part"))
                })
                .unwrap_or(false);
            if done > 0 && !partial {
                break;
            }
            assert!(Instant::now() < deadline, "cache write landed");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let t = Instant::now();
        let second =
            iai_formats::raw::decode_raw_controlled(path, RawDecodeControl::default()).unwrap();
        let hit_ms = t.elapsed().as_millis();
        let got = fingerprint(&second);
        println!(
            "{name:<40} decode {decode_ms:>6} ms  from cache {hit_ms:>5} ms  same {}",
            got == expected
        );
        assert_eq!(got, expected, "{name}: cache hit differs from the decode");
        let _ = std::fs::remove_dir_all(&cache);
    }
    iai_formats::raw_cache::configure(None);
}
