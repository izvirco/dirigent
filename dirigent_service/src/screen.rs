//! A thread's screen: the latest picture of the window its agent is using the computer in
//! (the runtime's `computer.ts` sends one whenever it changed), for clients to watch. Only
//! the latest is kept, in memory, and only for the run it belongs to: a screen is something
//! to look at while it happens, not history (what the model itself looked at is in the
//! transcript).
//!
//! Clients are told of every step with a small `screen` event and ask for the picture
//! (`GetScreen`) while they show the thread. Watching must never cost a client its
//! connection's responsiveness, however slow that is, so a picture is made small (a JPEG
//! scaled and compressed towards `MAX_BYTES`, made the first time someone asks: a screen
//! nobody watches costs nothing) and handed out in pieces of `PIECE`, each asked for once
//! the one before arrived: no more than one piece is ever on its way, ahead of whatever
//! else the connection has to carry.

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use image::ImageFormat;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::warn;

/// What a picture should weigh at most: what a slow connection takes a few seconds to carry.
const MAX_BYTES: usize = 96 << 10;
/// The longest edge and JPEG quality to try, in order, until the picture fits `MAX_BYTES`:
/// a window of text does at the first, a photo may need the last (sent as it is then).
const STEPS: [(u32, u8); 4] = [(1280, 70), (1280, 45), (960, 45), (640, 40)];
/// How much of a picture's base64 text one `Screen` reply carries: the most a client's
/// other messages ever wait behind.
const PIECE: usize = 16 << 10;

#[derive(Default)]
pub struct Screens {
    threads: Mutex<HashMap<String, Screen>>,
    /// The last picture's id. One count for all threads, so a thread's next run never
    /// reuses an id a client still holds a picture for.
    frames: AtomicU64,
}

/// The picture a connection is in the middle of reading (`Screens::piece`): its thread, its
/// id and its base64 text, kept whole while the thread's screen moves on. The connection
/// holds it (`server::Conn`) and it goes with it.
pub struct Reading {
    thread_id: String,
    frame: u64,
    text: Arc<str>,
}

#[derive(Default)]
struct Screen {
    /// The picture's id, as `screen` events name it; 0 while there is none.
    frame: u64,
    picture: Option<Picture>,
}

enum Picture {
    /// As the driver took it.
    Png(Arc<Vec<u8>>),
    /// As clients get it (`encode`), base64: made once, in the PNG's place.
    Wire(Arc<str>),
}

/// A piece of a picture, as a `Screen` reply carries it. Empty (`total` 0) when there is
/// no picture to give.
#[derive(Debug, Default, PartialEq)]
pub struct Piece {
    pub frame: u64,
    /// The length of the picture's whole base64 text.
    pub total: usize,
    pub data: String,
}

impl Screens {
    /// A step of the thread's computer use, with the window's picture if it changed.
    /// Returns the id of the picture the screen shows now, and whether the thread just got
    /// its screen.
    pub fn update(&self, thread_id: &str, png: Option<Vec<u8>>) -> (u64, bool) {
        let mut threads = self.threads.lock().unwrap();
        let new = !threads.contains_key(thread_id);
        let screen = threads.entry(thread_id.to_owned()).or_default();
        if let Some(png) = png {
            screen.frame = self.frames.fetch_add(1, Ordering::Relaxed) + 1;
            screen.picture = Some(Picture::Png(Arc::new(png)));
        }
        (screen.frame, new)
    }

    pub fn has(&self, thread_id: &str) -> bool {
        self.threads.lock().unwrap().contains_key(thread_id)
    }

    /// The thread's run is over, or its session is new or gone: it has no screen. A picture
    /// a client is in the middle of reading stays until it has it.
    pub fn clear(&self, thread_id: &str) {
        self.threads.lock().unwrap().remove(thread_id);
    }

    /// The runtime is gone, and every screen with it.
    pub fn clear_all(&self) {
        self.threads.lock().unwrap().clear();
    }

    /// The piece at `offset` of a picture of the thread's, for the connection that holds
    /// `reading` (`ClientMessage::GetScreen`): at 0 the thread's latest, which the
    /// connection then reads to its end (`frame` names it from there on) whatever the screen
    /// does meanwhile. An empty piece when there is none: the thread has no screen (its run
    /// just ended), or the connection isn't reading that picture.
    pub async fn piece(&self, reading: &Mutex<Option<Reading>>, thread_id: &str, frame: u64, offset: usize) -> Piece {
        if offset == 0 {
            let picture = self.picture(thread_id).await;
            *reading.lock().unwrap() = picture.map(|(frame, text)| Reading { thread_id: thread_id.to_owned(), frame, text });
        }
        let mut reading = reading.lock().unwrap();
        let Some(picture) =
            reading.as_ref().filter(|r| r.thread_id == thread_id && (offset == 0 || r.frame == frame) && offset < r.text.len())
        else {
            return Piece::default();
        };
        let end = (offset + PIECE).min(picture.text.len());
        let piece = Piece { frame: picture.frame, total: picture.text.len(), data: picture.text[offset..end].to_owned() };
        if end == piece.total {
            *reading = None;
        }
        piece
    }

    /// The thread's picture for the wire, if it has one: its id and a JPEG as base64 text.
    async fn picture(&self, thread_id: &str) -> Option<(u64, Arc<str>)> {
        let (frame, png) = {
            let threads = self.threads.lock().unwrap();
            let screen = threads.get(thread_id)?;
            match screen.picture.as_ref()? {
                Picture::Wire(text) => return Some((screen.frame, text.clone())),
                Picture::Png(png) => (screen.frame, png.clone()),
            }
        };
        // Off the async workers: a big window takes some tens of milliseconds.
        let jpeg = match tokio::task::spawn_blocking(move || encode(&png)).await {
            Ok(Ok(jpeg)) => jpeg,
            Ok(Err(e)) => {
                warn!(thread = %thread_id, "a screen's picture can't be read: {e:#}");
                return None;
            }
            Err(_) => return None,
        };
        let text: Arc<str> = B64.encode(jpeg).into();
        // Kept unless the screen moved on meanwhile.
        if let Some(screen) = self.threads.lock().unwrap().get_mut(thread_id).filter(|s| s.frame == frame) {
            screen.picture = Some(Picture::Wire(text.clone()));
        }
        Some((frame, text))
    }
}

/// `png` as a JPEG that fits `MAX_BYTES`: scaled down and compressed as far as `STEPS` goes.
fn encode(png: &[u8]) -> Result<Vec<u8>> {
    let picture = image::load_from_memory_with_format(png, ImageFormat::Png)?;
    let mut jpeg = Vec::new();
    for (edge, quality) in STEPS {
        // `resize` keeps the shape; a picture already within the edge stays as it is.
        let sized = if picture.width().max(picture.height()) > edge { &picture.resize(edge, edge, FilterType::Triangle) } else { &picture };
        jpeg.clear();
        JpegEncoder::new_with_quality(&mut jpeg, quality).encode_image(&sized.to_rgb8())?;
        if jpeg.len() <= MAX_BYTES {
            break;
        }
    }
    Ok(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let picture = image::RgbImage::from_fn(width, height, |x, y| image::Rgb(pixel(x, y)));
        let mut png = Vec::new();
        picture.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).unwrap();
        png
    }

    /// A cheap hash per pixel, no two neighbours alike: the worst a screen can be.
    fn noise(x: u32, y: u32) -> [u8; 3] {
        (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)).to_le_bytes()[..3].try_into().unwrap()
    }

    /// Whatever the window, the wire gets a JPEG within the budget: a plain one at its own
    /// size, noise scaled down until it fits.
    #[test]
    fn pictures_fit_the_wire() {
        let size = |jpeg: &[u8]| image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg).map(|p| (p.width(), p.height())).unwrap();
        let plain = encode(&png(800, 600, |x, _| [(x / 4) as u8, 40, 90])).unwrap();
        assert!(plain.len() <= MAX_BYTES);
        assert_eq!(size(&plain), (800, 600));
        let noisy = encode(&png(2560, 1440, noise)).unwrap();
        assert!(noisy.len() <= MAX_BYTES, "{} bytes", noisy.len());
        assert!(size(&noisy).0 <= 1280);
    }

    /// A step without a picture keeps the one on show, a new picture has a new id, and a
    /// cleared thread has no screen: asking then gets an empty piece.
    #[tokio::test]
    async fn the_latest_picture_is_the_screen() {
        let (screens, reading) = (Screens::default(), Mutex::new(None));
        assert_eq!(screens.piece(&reading, "t", 0, 0).await, Piece::default());
        assert_eq!(screens.update("t", None), (0, true));
        assert_eq!(screens.piece(&reading, "t", 0, 0).await, Piece::default());
        let (first, new) = screens.update("t", Some(png(4, 4, |_, _| [9, 9, 9])));
        assert!(first > 0 && !new);
        assert_eq!(screens.update("t", None), (first, false));
        let piece = screens.piece(&reading, "t", 0, 0).await;
        assert_eq!((piece.frame, piece.total), (first, piece.data.len()));
        assert_eq!(B64.decode(&piece.data).unwrap()[..2], [0xff, 0xd8]);
        assert!(screens.update("t", Some(png(4, 4, |_, _| [1, 2, 3]))).0 > first);
        screens.clear("t");
        assert!(!screens.has("t"));
        assert_eq!(screens.piece(&reading, "t", 0, 0).await, Piece::default());
    }

    /// A picture is read a piece at a time, to its end even when the screen has moved on;
    /// the pieces put together are the picture. Nobody else's pieces, and none after the last.
    #[tokio::test]
    async fn a_picture_comes_in_pieces() {
        let (screens, reading, other) = (Screens::default(), Mutex::new(None), Mutex::new(None));
        screens.update("t", Some(png(640, 480, noise)));
        let first = screens.piece(&reading, "t", 0, 0).await;
        assert_eq!(first.data.len(), PIECE);
        assert!(first.total > 2 * PIECE);
        // The screen moves on, and its run ends: the reader still gets the picture it began.
        screens.update("t", Some(png(4, 4, |_, _| [0, 0, 0])));
        screens.clear("t");
        assert_eq!(screens.piece(&other, "t", first.frame, PIECE).await, Piece::default());
        let mut text = first.data;
        while text.len() < first.total {
            let piece = screens.piece(&reading, "t", first.frame, text.len()).await;
            assert_eq!((piece.frame, piece.total), (first.frame, first.total));
            assert!(!piece.data.is_empty() && piece.data.len() <= PIECE);
            text += &piece.data;
        }
        let jpeg = B64.decode(&text).unwrap();
        assert!(image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg).is_ok());
        assert_eq!(screens.piece(&reading, "t", first.frame, PIECE).await, Piece::default());
    }
}
