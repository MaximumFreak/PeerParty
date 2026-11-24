//! Loads the library's pictures (covers and art, `file://` paths `Image::from_uri` is
//! given) on two worker threads with the `image` crate: what egui's extras did for this,
//! without their loaders for every scheme and format. A picture larger than a card ever
//! shows is scaled down before it's uploaded.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui;
use egui::load::{ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::ColorImage;

/// The longest side a picture is kept at (a tall cover at twice the display scale).
const MAX_SIDE: u32 = 1024;
const WORKERS: usize = 2;

enum Entry {
    Pending,
    Ready(Arc<ColorImage>),
    Failed(String),
}

#[derive(Default)]
struct Shared {
    cache: Mutex<HashMap<String, Entry>>,
    queue: Mutex<VecDeque<(String, egui::Context)>>,
    wake: Condvar,
}

#[derive(Default)]
pub struct FileImages {
    shared: Arc<Shared>,
    started: std::sync::Once,
}

/// Lets `ctx` show `file://` pictures.
pub fn install(ctx: &egui::Context) {
    ctx.add_image_loader(Arc::new(FileImages::default()));
}

/// The file a `file://` URI names (`file:///C:/...` on Windows).
fn path_of(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let b = rest.as_bytes();
    let rest = if b.len() > 2 && b[0] == b'/' && b[2] == b':' { &rest[1..] } else { rest };
    Some(PathBuf::from(rest))
}

/// The picture at `uri`, decoded and no larger than `MAX_SIDE`.
fn decode(uri: &str) -> Result<ColorImage, String> {
    let path = path_of(uri).ok_or("not a file")?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mut img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    if img.width().max(img.height()) > MAX_SIDE {
        img = img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle);
    }
    let rgba = img.to_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

impl FileImages {
    fn start(&self) {
        self.started.call_once(|| {
            for _ in 0..WORKERS {
                let shared = self.shared.clone();
                std::thread::spawn(move || loop {
                    let (uri, ctx) = {
                        let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(job) = q.pop_front() {
                                break job;
                            }
                            q = shared.wake.wait(q).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let entry = match decode(&uri) {
                        Ok(img) => Entry::Ready(Arc::new(img)),
                        Err(e) => Entry::Failed(e),
                    };
                    let mut cache = shared.cache.lock().unwrap_or_else(|e| e.into_inner());
                    // Forgotten while it loaded: not put back.
                    if let Some(slot) = cache.get_mut(&uri) {
                        *slot = entry;
                    }
                    drop(cache);
                    ctx.request_repaint();
                });
            }
        });
    }
}

impl ImageLoader for FileImages {
    fn id(&self) -> &str {
        concat!(module_path!(), "::FileImages")
    }

    fn load(&self, ctx: &egui::Context, uri: &str, _: SizeHint) -> ImageLoadResult {
        if !uri.starts_with("file://") {
            return Err(LoadError::NotSupported);
        }
        let mut cache = self.shared.cache.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(uri) {
            Some(Entry::Ready(img)) => return Ok(ImagePoll::Ready { image: img.clone() }),
            Some(Entry::Failed(e)) => return Err(LoadError::Loading(e.clone())),
            Some(Entry::Pending) => return Ok(ImagePoll::Pending { size: None }),
            None => {}
        }
        cache.insert(uri.to_string(), Entry::Pending);
        drop(cache);
        self.start();
        self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back((uri.to_string(), ctx.clone()));
        self.shared.wake.notify_one();
        Ok(ImagePoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        self.shared.cache.lock().unwrap_or_else(|e| e.into_inner()).remove(uri);
    }

    fn forget_all(&self) {
        self.shared.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn byte_size(&self) -> usize {
        let cache = self.shared.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.values().map(|e| if let Entry::Ready(img) = e { img.pixels.len() * 4 } else { 0 }).sum()
    }

    fn has_pending(&self) -> bool {
        self.shared.cache.lock().unwrap_or_else(|e| e.into_inner()).values().any(|e| matches!(e, Entry::Pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_name_their_files() {
        #[cfg(windows)]
        assert_eq!(path_of("file:///C:/Games/x/cover.jpg"), Some(PathBuf::from("C:/Games/x/cover.jpg")));
        assert_eq!(path_of("file:///home/kim/cover.png"), Some(PathBuf::from("/home/kim/cover.png")));
        assert_eq!(path_of("https://example.com/a.png"), None);
    }

    /// A big picture comes back scaled to `MAX_SIDE`, its shape kept.
    #[test]
    fn large_pictures_are_scaled_down() {
        let dir = std::env::temp_dir().join(format!("peerparty-covers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.png");
        image::RgbaImage::from_pixel(1200, 1800, image::Rgba([10, 20, 30, 255])).save(&file).unwrap();
        let uri = format!("file:///{}", file.display().to_string().replace('\\', "/"));
        let img = decode(&uri).unwrap();
        assert_eq!(img.size[1], 1024);
        assert!((682..=683).contains(&img.size[0]), "{:?}", img.size);
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(10, 20, 30));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
