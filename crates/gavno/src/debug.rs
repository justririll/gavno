//! Debug helpers: `kill -USR1 <gavno>` saves the next frame of the first output
//! to `$XDG_RUNTIME_DIR/gavno/shot.png` (half resolution), so the screen can be
//! checked over SSH. The same offscreen path will back gavno-agent screenshots.

use std::{
    io::Write,
    os::fd::{FromRawFd, OwnedFd},
    path::PathBuf,
    sync::atomic::{AtomicI32, Ordering},
};

use smithay::reexports::calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction};

use crate::Gavno;

static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_sigusr1(_: libc::c_int) {
    let fd = PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = 1u8;
        // Async-signal-safe: a single write to a non-blocking pipe.
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

pub fn shot_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("gavno").join("shot.png")
}

pub fn install_screenshot_signal(handle: &LoopHandle<'static, Gavno>) {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return;
    }
    PIPE_WRITE.store(fds[1], Ordering::Relaxed);
    unsafe { libc::signal(libc::SIGUSR1, on_sigusr1 as *const () as libc::sighandler_t) };
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let _ = handle.insert_source(Generic::new(read, Interest::READ, Mode::Level), |_, fd, gv| {
        let mut buf = [0u8; 16];
        let raw = std::os::fd::AsRawFd::as_raw_fd(unsafe { fd.get_mut() });
        unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        let path = shot_path();
        match crate::backend::screenshot(gv) {
            Some((w, h, rgba)) => match write_png_half(&path, w, h, &rgba) {
                Ok(()) => tracing::info!(?path, "screenshot saved"),
                Err(err) => tracing::warn!(?err, "screenshot write failed"),
            },
            None => tracing::warn!("screenshot failed"),
        }
        Ok(PostAction::Continue)
    });
}

/// Writes RGBA pixels as an RGB PNG at half resolution (2x2 box filter),
/// using stored (uncompressed) deflate blocks: no compression dependency.
pub fn write_png_half(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let (ow, oh) = (w / 2, h / 2);
    let mut raw = Vec::with_capacity(((ow * 3 + 1) * oh) as usize);
    for y in 0..oh {
        raw.push(0u8); // filter: none
        for x in 0..ow {
            for c in 0..3 {
                let mut sum = 0u32;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let i = (((y * 2 + dy) * w + (x * 2 + dx)) * 4 + c) as usize;
                    sum += rgba[i] as u32;
                }
                raw.push((sum / 4) as u8);
            }
        }
    }

    let mut zlib = vec![0x78, 0x01];
    for (i, chunk) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        zlib.push(last as u8);
        let len = chunk.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(chunk);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&ow.to_be_bytes());
    ihdr.extend_from_slice(&oh.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::File::create(path)?.write_all(&png)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc_of_iend() {
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
    }
}
