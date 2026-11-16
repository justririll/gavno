use gtk::{gdk, prelude::*};
use lru::LruCache;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
};

pub fn cache_path(base: &Path, uri: &str, large: bool) -> PathBuf {
    base.join("thumbnails")
        .join(if large { "large" } else { "normal" })
        .join(format!("{:x}.png", md5::compute(uri.as_bytes())))
}
pub fn supported(info: &gio::FileInfo) -> bool {
    let name = info.name().to_string_lossy().to_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif", ".svg"]
        .iter()
        .any(|ext| name.ends_with(ext))
}
struct Pixels {
    data: Vec<u8>,
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
}
struct Request {
    uri: String,
    path: PathBuf,
    modified: u64,
    reply: futures_channel::oneshot::Sender<Result<Pixels, String>>,
}
#[derive(Clone)]
pub struct Thumbnails {
    cache: Rc<RefCell<LruCache<String, gdk::Texture>>>,
    sender: mpsc::Sender<Request>,
}
impl Thumbnails {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<Request>();
        // One decoder bounds memory, disk concurrency, and SVG rasterization work.
        std::thread::spawn(move || {
            for req in receiver {
                let result = decode(&req);
                let _ = req.reply.send(result);
            }
        });
        Self {
            cache: Rc::new(RefCell::new(LruCache::unbounded())),
            sender,
        }
    }
    pub async fn get(&self, info: &gio::FileInfo) -> Result<Option<gdk::Texture>, String> {
        let file = crate::model::file(info);
        let Some(path) = file.path() else {
            return Ok(None);
        };
        let modified = info.attribute_uint64("time::modified");
        let key = format!("{}:{modified}", file.uri());
        if let Some(texture) = self.cache.borrow_mut().get(&key).cloned() {
            return Ok(Some(texture));
        }
        let (reply, result) = futures_channel::oneshot::channel();
        self.sender
            .send(Request {
                uri: file.uri().to_string(),
                path,
                modified,
                reply,
            })
            .map_err(|e| e.to_string())?;
        let p = result.await.map_err(|e| e.to_string())??;
        let texture: gdk::Texture = gdk::MemoryTexture::new(
            p.width,
            p.height,
            if p.alpha {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            },
            &glib::Bytes::from_owned(p.data),
            p.stride,
        )
        .upcast();
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= 256 {
            cache.pop_lru();
        }
        cache.put(key, texture.clone());
        Ok(Some(texture))
    }
}
fn decode(req: &Request) -> Result<Pixels, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let base = glib::user_cache_dir();
    let mut found = None;
    for large in [true, false] {
        let path = cache_path(&base, &req.uri, large);
        if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(&path) {
            let valid = pixbuf.option("tEXt::Thumb::URI").as_deref() == Some(req.uri.as_str())
                && pixbuf.option("tEXt::Thumb::MTime").as_deref()
                    == Some(req.modified.to_string().as_str());
            if valid {
                found = Some(pixbuf);
                break;
            }
        }
    }
    let pixbuf = match found {
        Some(p) => p,
        None => {
            let original = gdk_pixbuf::Pixbuf::from_file_at_scale(&req.path, 256, 256, true)
                .map_err(|e| e.to_string())?;
            let p = original.apply_embedded_orientation().unwrap_or(original);
            // Never recursively thumbnail the thumbnail cache itself.
            if !req.path.starts_with(base.join("thumbnails")) {
                for large in [false, true] {
                    let target = cache_path(&base, &req.uri, large);
                    if let Some(parent) = target.parent() {
                        std::fs::DirBuilder::new()
                            .recursive(true)
                            .mode(0o700)
                            .create(parent)
                            .map_err(|e| e.to_string())?;
                    }
                    let side = if large { 256 } else { 128 };
                    let scaled = if p.width() > side || p.height() > side {
                        let ratio =
                            (side as f64 / p.width() as f64).min(side as f64 / p.height() as f64);
                        p.scale_simple(
                            (p.width() as f64 * ratio).max(1.) as i32,
                            (p.height() as f64 * ratio).max(1.) as i32,
                            gdk_pixbuf::InterpType::Bilinear,
                        )
                        .ok_or("Cannot scale thumbnail")?
                    } else {
                        p.clone()
                    };
                    let temp = target.with_extension(format!("{}.tmp", std::process::id()));
                    let handle = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .mode(0o600)
                        .open(&temp)
                        .map_err(|e| e.to_string())?;
                    handle
                        .set_permissions(std::fs::Permissions::from_mode(0o600))
                        .map_err(|e| e.to_string())?;
                    scaled
                        .savev(
                            &temp,
                            "png",
                            &[
                                ("tEXt::Thumb::URI", &req.uri),
                                ("tEXt::Thumb::MTime", &req.modified.to_string()),
                                ("tEXt::Software", "gavno-files"),
                            ],
                        )
                        .map_err(|e| e.to_string())?;
                    std::fs::rename(&temp, &target).map_err(|e| e.to_string())?;
                }
            }
            p
        }
    };
    Ok(Pixels {
        data: pixbuf.read_pixel_bytes().to_vec(),
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride() as usize,
        alpha: pixbuf.has_alpha(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn freedesktop_md5() {
        assert_eq!(
            cache_path(Path::new("/cache"), "file:///home/user/image.png", false),
            PathBuf::from("/cache/thumbnails/normal/e47e29c02f69c7dd185f87f51d43a326.png")
        );
        assert_eq!(
            cache_path(Path::new("/cache"), "file:///tmp/a%20b.png", true)
                .file_name()
                .and_then(|s| s.to_str()),
            Some("f2584ab78dd95a88bd0d3f0ecaee7a8c.png")
        );
    }
}
