//! A wallpaper FACT paints itself, so it shows in any truecolor terminal:
//! Windows Terminal, the classic console, VS Code and others.
//!
//! Every blank cell shows several image pixels through a block character with
//! two colors: four (a 2x2 quadrant glyph such as `▚` or `▟`) in terminals that
//! draw block characters themselves, two (`▀`) elsewhere. A cell with text gets
//! the average of its pixels as its background. Pixels are darkened by the
//! opacity so text stays readable. Only cells still at the default background
//! are painted, so panels, the cursor, selections and application colors are
//! never covered.
//!
//! Videos play in a loop at their own colors: ffmpeg decodes them and streams
//! raw frames already scaled to the window's cell grid (two pixels per cell is
//! the finest any terminal can show behind text). Animated GIFs loop too,
//! decoded one frame at a time. Playback pauses while the window is unfocused.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use image::codecs::gif::GifDecoder;
use image::imageops::{self, FilterType};
use image::{AnimationDecoder, Frames, ImageFormat, ImageReader, Rgb, RgbImage};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

/// How the image fills the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Cover the screen, cropping the edges if needed.
    Fill,
    /// Show the whole image, letterboxed in black.
    Fit,
    /// Stretch to the screen, ignoring the aspect ratio.
    Stretch,
    /// Keep a small image at its own size, centered; shrink a large one to fit.
    Center,
}

/// What to paint behind the panes.
#[derive(Debug, Clone)]
pub struct WallpaperSpec {
    pub image: PathBuf,
    /// 0.05-1.0: how strongly the image shows through.
    pub opacity: f32,
    pub fit: Fit,
}

/// File types played as video through ffmpeg.
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v", "wmv", "flv"];
/// Video playback rate. ffmpeg resamples every video to it.
const VIDEO_FPS: u32 = 24;

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// True when this process is really hosted by Windows Terminal.
///
/// `WT_SESSION` alone is not enough: a classic console window started from a
/// Windows Terminal tab inherits it. Inside Windows Terminal the console
/// window is a hidden pseudo console; a classic console has a real window.
pub fn in_windows_terminal() -> bool {
    // Other terminals started from a Windows Terminal tab (VS Code, WezTerm)
    // also use a pseudo console and inherit WT_SESSION, but set TERM_PROGRAM.
    std::env::var_os("WT_SESSION").is_some()
        && std::env::var_os("TERM_PROGRAM").is_none()
        && console_is_pseudo()
}

#[cfg(windows)]
fn console_is_pseudo() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let window = unsafe { GetConsoleWindow() };
    if window.is_null() {
        return false;
    }
    let mut class = [0u16; 64];
    let length = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    let length = usize::try_from(length).unwrap_or(0).min(class.len());
    String::from_utf16_lossy(&class[..length]) == "PseudoConsoleWindow"
}

#[cfg(not(windows))]
fn console_is_pseudo() -> bool {
    false
}

/// `FACT_FFMPEG`, then `ffmpeg.exe` on PATH, then where winget, Chocolatey,
/// Scoop and manual installs put it. A terminal opened before ffmpeg was
/// installed has an old PATH, so the install folders matter.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FACT_FFMPEG").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join("ffmpeg.exe"));
    let env_dir = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let local = env_dir("LOCALAPPDATA");
    let mut known: Vec<PathBuf> = Vec::new();
    if let Some(local) = &local {
        known.push(local.join(r"Microsoft\WinGet\Links\ffmpeg.exe"));
    }
    if let Some(data) = env_dir("ProgramData") {
        known.push(data.join(r"chocolatey\bin\ffmpeg.exe"));
    }
    if let Some(home) = env_dir("USERPROFILE") {
        known.push(home.join(r"scoop\shims\ffmpeg.exe"));
    }
    known.push(PathBuf::from(r"C:\ffmpeg\bin\ffmpeg.exe"));
    known.push(PathBuf::from(r"C:\Program Files\ffmpeg\bin\ffmpeg.exe"));
    on_path
        .chain(known)
        .find(|path| path.is_file())
        .or_else(|| winget_package_ffmpeg(&local?.join(r"Microsoft\WinGet\Packages")))
}

/// `<packages>\Gyan.FFmpeg_...\ffmpeg-7.1-full_build\bin\ffmpeg.exe`.
fn winget_package_ffmpeg(packages: &Path) -> Option<PathBuf> {
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    };
    children(packages)
        .into_iter()
        .filter(|package| {
            package
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.to_ascii_lowercase().contains("ffmpeg"))
        })
        .flat_map(|package| children(&package))
        .map(|build| build.join(r"bin\ffmpeg.exe"))
        .find(|path| path.is_file())
}

/// Check that `path` is a picture FACT can show, judging by its content
/// rather than its name. Returns its size in pixels.
pub fn image_dimensions(path: &Path) -> Result<(u32, u32), String> {
    ImageReader::open(path)
        .and_then(|reader| reader.with_guessed_format())
        .map_err(|error| error.to_string())?
        .into_dimensions()
        .map_err(|error| error.to_string())
}

fn is_gif(path: &Path) -> bool {
    ImageReader::open(path)
        .and_then(|reader| reader.with_guessed_format())
        .is_ok_and(|reader| reader.format() == Some(ImageFormat::Gif))
}

/// Larger images are scaled down once at load; this is plenty for a window
/// of a few hundred cells and keeps memory small.
const MAX_SOURCE: u32 = 800;
/// Animation frames are scaled down further: they are decoded continuously.
const MAX_FRAME: u32 = 320;
/// Fastest and slowest frame times; GIFs often declare 0 ms.
const MIN_FRAME_TIME: Duration = Duration::from_millis(66);
const MAX_FRAME_TIME: Duration = Duration::from_secs(2);
const DEFAULT_FRAME_TIME: Duration = Duration::from_millis(100);
/// How soon to look again when ffmpeg has not delivered the next frame yet.
const VIDEO_RETRY: Duration = Duration::from_millis(10);
/// A window being resized keeps its last frame (scaled) until the size has
/// held this long, instead of restarting ffmpeg for every step.
const RESIZE_SETTLE: Duration = Duration::from_millis(200);
/// ffmpeg is restarted this many times after it stops before FACT gives up.
const VIDEO_ATTEMPTS: u32 = 3;

pub struct Wallpaper {
    source: RgbImage,
    /// Image pixels per cell across: 2 with quadrant glyphs, 1 with `▀`.
    across: u16,
    /// The theme background the image fades toward (black for dark themes,
    /// near white for light ones), so text stays readable either way.
    base: [u8; 3],
    animation: Option<Animation>,
    video: Option<Video>,
    opacity: f32,
    fit: Fit,
    /// Darkened pixels for the current screen size: `cols` wide and
    /// `rows * 2` tall (two pixels per cell), row-major.
    cache: Option<(u16, u16, Vec<[u8; 3]>)>,
}

/// A looping video streamed from ffmpeg at the current cell grid size.
struct Video {
    across: u16,
    path: PathBuf,
    ffmpeg: PathBuf,
    fit: Fit,
    /// Cell grid the running stream was scaled for.
    size: (u16, u16),
    stream: Option<(Child, Receiver<Vec<u8>>)>,
    /// When the next frame should appear. Frames keep this schedule, so a
    /// late frame does not slow down the whole video.
    due: Instant,
    /// When to look for the next frame (at or after `due`).
    check_at: Instant,
    /// A new window size and when it was first seen.
    pending_size: Option<((u16, u16), Instant)>,
    /// Restarts after ffmpeg stopped, and when to try the next one.
    attempts: u32,
    retry_at: Option<Instant>,
    paused: bool,
    failed: bool,
    /// Why playback stopped, for the status line.
    message: Option<String>,
}

impl Video {
    fn frame_time() -> Duration {
        Duration::from_secs(1) / VIDEO_FPS
    }

    /// (Re)start ffmpeg for a `cols` x `rows` cell grid.
    fn start(&mut self, cols: u16, rows: u16) {
        self.stop();
        self.size = (cols, rows);
        let (width, height) = (
            u32::from(cols) * u32::from(self.across),
            u32::from(rows) * 2,
        );
        let scale = match self.fit {
            Fit::Fill => format!(
                "scale={width}:{height}:force_original_aspect_ratio=increase:flags=area,crop={width}:{height}"
            ),
            Fit::Fit => format!(
                "scale={width}:{height}:force_original_aspect_ratio=decrease:flags=area,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
            ),
            Fit::Stretch => format!("scale={width}:{height}:flags=area"),
            Fit::Center => format!(
                "scale='min(iw,{width})':'min(ih,{height})':force_original_aspect_ratio=decrease:flags=area,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
            ),
        };
        let mut command = Command::new(&self.ffmpeg);
        command
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-stream_loop",
                "-1",
                "-i",
            ])
            .arg(&self.path)
            .args(["-an", "-vf"])
            .arg(format!("fps={VIDEO_FPS},{scale}"))
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let Ok(mut child) = command.spawn() else {
            self.stopped(Instant::now());
            return;
        };
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            self.stopped(Instant::now());
            return;
        };
        let frame_bytes = width as usize * height as usize * 3;
        // One frame in flight: ffmpeg blocks until FACT takes the next one, so
        // playback follows FACT's clock and pausing simply stops reading.
        let (tx, rx) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("fact-wallpaper-video".into())
            .spawn(move || loop {
                let mut frame = vec![0u8; frame_bytes];
                if stdout.read_exact(&mut frame).is_err() || tx.send(frame).is_err() {
                    break;
                }
            });
        if spawned.is_err() {
            let _ = child.kill();
            self.stopped(Instant::now());
            return;
        }
        self.stream = Some((child, rx));
        let now = Instant::now();
        self.due = now;
        self.check_at = now;
    }

    /// ffmpeg stopped or could not start: try again a little later, a few
    /// times, then keep the last frame and say why.
    fn stopped(&mut self, now: Instant) {
        self.stop();
        self.attempts += 1;
        if self.attempts < VIDEO_ATTEMPTS {
            self.retry_at = Some(now + Duration::from_secs(u64::from(self.attempts)));
        } else {
            self.failed = true;
            self.message = Some(format!(
                "Video wallpaper stopped: ffmpeg could not play {}.",
                self.path.display()
            ));
        }
    }

    fn stop(&mut self) {
        if let Some((mut child, _)) = self.stream.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Animation {
    path: PathBuf,
    frames: Frames<'static>,
    delay: Duration,
    shown_at: Instant,
    paused: bool,
}

impl Wallpaper {
    pub fn load(spec: &WallpaperSpec) -> Result<Self, String> {
        let failed = |error: String| format!("could not load {}: {error}", spec.image.display());
        if is_video(&spec.image) {
            if !spec.image.is_file() {
                return Err(failed("the video file is missing".into()));
            }
            let ffmpeg = find_ffmpeg().ok_or_else(|| {
                failed("video wallpapers need ffmpeg (winget install ffmpeg)".into())
            })?;
            let mut wallpaper = Self::from_image(RgbImage::new(1, 1), spec.opacity, spec.fit);
            let now = Instant::now();
            wallpaper.video = Some(Video {
                across: wallpaper.across,
                path: spec.image.clone(),
                ffmpeg,
                fit: spec.fit,
                size: (0, 0),
                stream: None,
                due: now,
                check_at: now,
                pending_size: None,
                attempts: 0,
                retry_at: None,
                paused: false,
                failed: false,
                message: None,
            });
            return Ok(wallpaper);
        }
        if is_gif(&spec.image) && open_frames(&spec.image).map_err(failed)?.take(2).count() > 1 {
            let mut frames = open_frames(&spec.image).map_err(failed)?;
            let first = frames
                .next()
                .ok_or_else(|| failed("no frames".into()))?
                .map_err(|error| failed(error.to_string()))?;
            let delay = frame_time(&first);
            let mut wallpaper = Self::from_image(frame_image(first), spec.opacity, spec.fit);
            wallpaper.animation = Some(Animation {
                path: spec.image.clone(),
                frames,
                delay,
                shown_at: Instant::now(),
                paused: false,
            });
            return Ok(wallpaper);
        }
        // Judge the format by content: a renamed file still loads.
        let image = ImageReader::open(&spec.image)
            .and_then(|reader| reader.with_guessed_format())
            .map_err(|error| failed(error.to_string()))?
            .decode()
            .map_err(|error| failed(error.to_string()))?;
        let image = if image.width() > MAX_SOURCE || image.height() > MAX_SOURCE {
            image.thumbnail(MAX_SOURCE, MAX_SOURCE)
        } else {
            image
        };
        Ok(Self::from_image(
            over_black(image.to_rgba8()),
            spec.opacity,
            spec.fit,
        ))
    }

    pub fn from_image(source: RgbImage, opacity: f32, fit: Fit) -> Self {
        Self {
            source,
            across: if quadrant_glyphs() { 2 } else { 1 },
            base: [0, 0, 0],
            animation: None,
            video: None,
            opacity: opacity.clamp(0.05, 1.0),
            fit,
            cache: None,
        }
    }

    /// Fade toward this background color. Called when the theme changes.
    pub fn set_base(&mut self, base: [u8; 3]) {
        if self.base != base {
            self.base = base;
            // Stills are recomputed from the source; video picks it up on
            // its next frame.
            if self.video.is_none() {
                self.cache = None;
            }
        }
    }

    /// Why the wallpaper stopped playing, once, for the status line.
    pub fn take_message(&mut self) -> Option<String> {
        self.video.as_mut().and_then(|video| video.message.take())
    }

    /// When the next animation frame is due, if the wallpaper is playing.
    pub fn next_frame_at(&self) -> Option<Instant> {
        if let Some(video) = &self.video {
            if video.paused || video.failed {
                return None;
            }
            let settle = video.pending_size.map(|(_, at)| at + RESIZE_SETTLE);
            let frame = video.stream.as_ref().map(|_| video.check_at);
            return [settle, frame, video.retry_at].into_iter().flatten().min();
        }
        self.animation
            .as_ref()
            .filter(|animation| !animation.paused)
            .map(|animation| animation.shown_at + animation.delay)
    }

    /// Stop or resume playback (FACT pauses while its window is unfocused).
    pub fn set_paused(&mut self, paused: bool) {
        if let Some(video) = &mut self.video {
            if video.paused && !paused {
                let now = Instant::now();
                video.due = now;
                video.check_at = now;
            }
            video.paused = paused;
        }
        if let Some(animation) = &mut self.animation {
            if animation.paused && !paused {
                animation.shown_at = Instant::now();
            }
            animation.paused = paused;
        }
    }

    /// Advance to the next frame when it is due. Returns `true` when the
    /// picture changed and the screen should be redrawn.
    pub fn tick(&mut self, now: Instant) -> bool {
        if let Some(video) = &mut self.video {
            if video.paused || video.failed {
                return false;
            }
            if let Some((size, since)) = video.pending_size {
                if now >= since + RESIZE_SETTLE {
                    video.pending_size = None;
                    video.start(size.0, size.1);
                    return false;
                }
            }
            if video.retry_at.is_some_and(|at| now >= at) {
                video.retry_at = None;
                video.start(video.size.0, video.size.1);
                return false;
            }
            if now < video.check_at {
                return false;
            }
            let Some((_, frames)) = &video.stream else {
                return false;
            };
            return match frames.try_recv() {
                Ok(frame) => {
                    let frame_time = Video::frame_time();
                    // Keep the schedule unless far behind (after a stall).
                    video.due = if now.saturating_duration_since(video.due) > frame_time * 2 {
                        now + frame_time
                    } else {
                        video.due + frame_time
                    };
                    video.check_at = video.due;
                    video.attempts = 0;
                    let (scale, base) = (self.opacity, self.base);
                    let (pixels, _) = frame.as_chunks::<3>();
                    let pixels = pixels.iter().map(|p| blend(*p, base, scale)).collect();
                    let (cols, rows) = video.size;
                    self.cache = Some((cols, rows, pixels));
                    // Kept to show, scaled, while the window is resized.
                    let (width, height) = (
                        u32::from(cols) * u32::from(video.across),
                        u32::from(rows) * 2,
                    );
                    if let Some(image) = RgbImage::from_raw(width, height, frame) {
                        self.source = image;
                    }
                    true
                }
                // Not decoded yet: look again shortly instead of spinning on
                // a deadline that has already passed.
                Err(TryRecvError::Empty) => {
                    video.check_at = now + VIDEO_RETRY;
                    false
                }
                Err(TryRecvError::Disconnected) => {
                    // ffmpeg stopped; keep the last frame and try again.
                    video.stopped(now);
                    false
                }
            };
        }
        let Some(animation) = &mut self.animation else {
            return false;
        };
        if animation.paused || now < animation.shown_at + animation.delay {
            return false;
        }
        let next = match animation.frames.next() {
            Some(Ok(frame)) => Some(frame),
            // End of the file (or a damaged frame): start the loop again.
            _ => open_frames(&animation.path).ok().and_then(|mut frames| {
                let first = frames.next()?.ok()?;
                animation.frames = frames;
                Some(first)
            }),
        };
        match next {
            Some(frame) => {
                // Keep the GIF's own timing unless far behind.
                let due = animation.shown_at + animation.delay;
                animation.shown_at = if now.saturating_duration_since(due) > animation.delay * 2 {
                    now
                } else {
                    due
                };
                animation.delay = frame_time(&frame);
                self.source = frame_image(frame);
                self.cache = None;
                true
            }
            None => {
                // The file went away; keep showing the last frame.
                self.animation = None;
                false
            }
        }
    }

    /// Paint the wallpaper behind every cell of `area` that still has the
    /// default (`Reset`) background.
    pub fn paint(&mut self, buffer: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if let Some(video) = &mut self.video {
            let size = (area.width, area.height);
            if video.size != size && !video.failed {
                if video.stream.is_none() && video.retry_at.is_none() {
                    video.start(size.0, size.1);
                } else if video
                    .pending_size
                    .is_none_or(|(pending, _)| pending != size)
                {
                    video.pending_size = Some((size, Instant::now()));
                }
            }
            // Before the first frame arrives, leave the background alone.
            // Later, a frame of another size is shown scaled.
            if self.cache.is_none() {
                return;
            }
        }
        let across = usize::from(self.across);
        let pixels = self.pixels(area.width, area.height);
        let row_len = usize::from(area.width) * across;
        for y in 0..area.height {
            let upper = usize::from(y) * 2 * row_len;
            let lower = upper + row_len;
            // The cell to the right of a double-width character is covered by
            // it; only its background may change.
            let mut covered = false;
            for x in 0..area.width {
                let left = usize::from(x) * across;
                let right = left + across - 1;
                let quad = [
                    pixels[upper + left],
                    pixels[upper + right],
                    pixels[lower + left],
                    pixels[lower + right],
                ];
                let cell = &mut buffer[(area.x + x, area.y + y)];
                let was_covered = covered;
                covered = cell.symbol().width() > 1;
                if cell.bg != Color::Reset {
                    continue;
                }
                let blank = cell.symbol() == " " && cell.modifier == Modifier::empty();
                if blank && !was_covered {
                    let (glyph, fg, bg) = if across == 2 {
                        quadrant(quad)
                    } else {
                        ('▀', quad[0], quad[2])
                    };
                    cell.set_char(glyph);
                    cell.fg = rgb(fg);
                    cell.bg = rgb(bg);
                } else {
                    cell.bg = rgb(mean(&quad));
                }
            }
        }
    }

    fn pixels(&mut self, cols: u16, rows: u16) -> &[[u8; 3]] {
        let stale = self
            .cache
            .as_ref()
            .is_none_or(|(c, r, _)| *c != cols || *r != rows);
        if stale {
            let fitted = fit_image(
                &self.source,
                u32::from(cols) * u32::from(self.across),
                u32::from(rows) * 2,
                self.fit,
            );
            let (scale, base) = (self.opacity, self.base);
            let pixels = fitted
                .pixels()
                .map(|pixel| blend(pixel.0, base, scale))
                .collect();
            self.cache = Some((cols, rows, pixels));
        }
        &self.cache.as_ref().expect("cache was just filled").2
    }
}

fn open_frames(path: &Path) -> Result<Frames<'static>, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let decoder = GifDecoder::new(BufReader::new(file)).map_err(|error| error.to_string())?;
    Ok(decoder.into_frames())
}

fn frame_time(frame: &image::Frame) -> Duration {
    let (numerator, denominator) = frame.delay().numer_denom_ms();
    let millis = numerator.checked_div(denominator).unwrap_or(0);
    if millis == 0 {
        DEFAULT_FRAME_TIME
    } else {
        Duration::from_millis(u64::from(millis)).clamp(MIN_FRAME_TIME, MAX_FRAME_TIME)
    }
}

/// A frame scaled down for playback, composited over black.
fn frame_image(frame: image::Frame) -> RgbImage {
    let mut rgba = frame.into_buffer();
    if rgba.width() > MAX_FRAME || rgba.height() > MAX_FRAME {
        let scale = f64::from(MAX_FRAME) / f64::from(rgba.width().max(rgba.height()));
        let width = ((f64::from(rgba.width()) * scale).round() as u32).max(1);
        let height = ((f64::from(rgba.height()) * scale).round() as u32).max(1);
        rgba = imageops::thumbnail(&rgba, width, height);
    }
    over_black(rgba)
}

/// Composite transparent pixels over black, the color behind the wallpaper.
fn over_black(rgba: image::RgbaImage) -> RgbImage {
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let alpha = u16::from(a);
        Rgb([r, g, b].map(|c| (u16::from(c) * alpha / 255) as u8))
    })
}

/// Scale `source` to exactly `width` x `height` pixels using `fit`.
fn fit_image(source: &RgbImage, width: u32, height: u32, fit: Fit) -> RgbImage {
    let (sw, sh) = source.dimensions();
    if sw == 0 || sh == 0 {
        return RgbImage::new(width, height);
    }
    match fit {
        Fit::Stretch => imageops::resize(source, width, height, FilterType::Triangle),
        Fit::Fill => {
            // Crop the source to the target's aspect ratio, then scale.
            let scale = (f64::from(width) / f64::from(sw)).max(f64::from(height) / f64::from(sh));
            let crop_w = ((f64::from(width) / scale).round() as u32).clamp(1, sw);
            let crop_h = ((f64::from(height) / scale).round() as u32).clamp(1, sh);
            let cropped =
                imageops::crop_imm(source, (sw - crop_w) / 2, (sh - crop_h) / 2, crop_w, crop_h)
                    .to_image();
            imageops::resize(&cropped, width, height, FilterType::Triangle)
        }
        Fit::Fit | Fit::Center => {
            let mut scale =
                (f64::from(width) / f64::from(sw)).min(f64::from(height) / f64::from(sh));
            if fit == Fit::Center {
                scale = scale.min(1.0);
            }
            let fit_w = ((f64::from(sw) * scale).round() as u32).clamp(1, width);
            let fit_h = ((f64::from(sh) * scale).round() as u32).clamp(1, height);
            let resized = imageops::resize(source, fit_w, fit_h, FilterType::Triangle);
            let mut canvas = RgbImage::new(width, height);
            imageops::replace(
                &mut canvas,
                &resized,
                i64::from((width - fit_w) / 2),
                i64::from((height - fit_h) / 2),
            );
            canvas
        }
    }
}

/// `pixel` shown at `scale` strength over `base`.
fn blend(pixel: [u8; 3], base: [u8; 3], scale: f32) -> [u8; 3] {
    [0, 1, 2].map(|i| {
        let (p, b) = (f32::from(pixel[i]), f32::from(base[i]));
        (b + (p - b) * scale).round().clamp(0.0, 255.0) as u8
    })
}

fn mean(pixels: &[[u8; 3]]) -> [u8; 3] {
    let count = pixels.len().max(1) as u32;
    [0, 1, 2].map(|i| (pixels.iter().map(|p| u32::from(p[i])).sum::<u32>() / count) as u8)
}

fn distance(a: [u8; 3], b: [u8; 3]) -> u32 {
    (0..3)
        .map(|i| {
            let d = i32::from(a[i]) - i32::from(b[i]);
            (d * d) as u32
        })
        .sum()
}

/// Terminals that draw block elements themselves render quadrant glyphs
/// without gaps; console fonts may lack them.
fn quadrant_glyphs() -> bool {
    in_windows_terminal()
        || std::env::var("TERM_PROGRAM").is_ok_and(|program| {
            ["vscode", "WezTerm"]
                .iter()
                .any(|known| program.eq_ignore_ascii_case(known))
        })
}

/// Best two-color approximation of a 2x2 block (top-left, top-right,
/// bottom-left, bottom-right): split the pixels around the two most different
/// ones and pick the quadrant glyph whose filled part is the first group.
fn quadrant(pixels: [[u8; 3]; 4]) -> (char, [u8; 3], [u8; 3]) {
    let (mut a, mut b, mut spread) = (0, 0, 0);
    for i in 0..4 {
        for j in i + 1..4 {
            let d = distance(pixels[i], pixels[j]);
            if d > spread {
                (a, b, spread) = (i, j, d);
            }
        }
    }
    if spread < 64 {
        let color = mean(&pixels);
        return ('█', color, color);
    }
    let mut mask = 0u8;
    let (mut fg, mut bg) = (Vec::with_capacity(4), Vec::with_capacity(4));
    for (index, pixel) in pixels.iter().enumerate() {
        if distance(*pixel, pixels[a]) <= distance(*pixel, pixels[b]) {
            mask |= 0b1000 >> index;
            fg.push(*pixel);
        } else {
            bg.push(*pixel);
        }
    }
    const GLYPHS: [char; 16] = [
        ' ', '▗', '▖', '▄', '▝', '▐', '▞', '▟', '▘', '▚', '▌', '▙', '▀', '▜', '▛', '█',
    ];
    (GLYPHS[usize::from(mask)], mean(&fg), mean(&bg))
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    /// Left half red, right half blue.
    fn split_image() -> RgbImage {
        RgbImage::from_fn(40, 20, |x, _| {
            if x < 20 {
                Rgb([200, 0, 0])
            } else {
                Rgb([0, 0, 200])
            }
        })
    }

    #[test]
    fn fit_modes_produce_the_requested_size() {
        let source = split_image();
        for fit in [Fit::Fill, Fit::Fit, Fit::Stretch, Fit::Center] {
            let image = fit_image(&source, 30, 24, fit);
            assert_eq!(image.dimensions(), (30, 24), "{fit:?}");
        }
        // Letterboxing leaves black bars above and below a wide image.
        let boxed = fit_image(&source, 30, 30, Fit::Fit);
        assert_eq!(boxed.get_pixel(15, 0), &Rgb([0, 0, 0]));
        // Center never enlarges: a 40x20 image keeps its size on a big screen.
        let centered = fit_image(&source, 100, 60, Fit::Center);
        assert_eq!(centered.get_pixel(29, 30), &Rgb([0, 0, 0]));
        assert_eq!(centered.get_pixel(31, 30), &Rgb([200, 0, 0]));
        assert_eq!(centered.get_pixel(30, 19), &Rgb([0, 0, 0]));
    }

    #[test]
    fn paints_only_default_background_cells() {
        let mut wallpaper = Wallpaper::from_image(split_image(), 0.5, Fit::Stretch);
        wallpaper.across = 1;
        let area = Rect::new(0, 0, 10, 4);
        let mut buffer = Buffer::empty(area);
        buffer.set_style(area, Style::default().bg(Color::Reset));
        buffer.set_string(0, 0, "ab", Style::default().fg(Color::White));
        buffer[(9, 3)].set_bg(Color::Rgb(1, 2, 3));
        wallpaper.paint(&mut buffer, area);

        // Blank cells get two pixels as a half block, darkened to 50%.
        assert_eq!(buffer[(0, 1)].symbol(), "▀");
        assert_eq!(buffer[(0, 1)].fg, Color::Rgb(100, 0, 0));
        assert_eq!(buffer[(9, 1)].bg, Color::Rgb(0, 0, 100));
        // Text keeps its glyph and color and gets the image behind it.
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(0, 0)].fg, Color::White);
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(100, 0, 0));
        // Cells with their own background are left alone.
        assert_eq!(buffer[(9, 3)].bg, Color::Rgb(1, 2, 3));
        assert_eq!(buffer[(9, 3)].symbol(), " ");
    }

    #[test]
    fn light_themes_fade_the_image_toward_white() {
        assert_eq!(blend([200, 0, 0], [0, 0, 0], 0.5), [100, 0, 0]);
        assert_eq!(blend([200, 0, 0], [255, 255, 255], 0.5), [228, 128, 128]);
        let mut wallpaper = Wallpaper::from_image(split_image(), 0.5, Fit::Stretch);
        wallpaper.across = 1;
        wallpaper.set_base([255, 255, 255]);
        let area = Rect::new(0, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        wallpaper.paint(&mut buffer, area);
        // Faded toward white (every channel bright) and still reddish.
        let Color::Rgb(r, g, b) = buffer[(0, 0)].fg else {
            panic!("expected an RGB foreground")
        };
        assert!(r > g && g >= 120 && b >= 120, "got {r} {g} {b}");
    }

    #[test]
    fn quadrants_split_a_cell_between_two_colors() {
        let (r, b) = ([200, 0, 0], [0, 0, 200]);
        // Left column red, right column blue: a left half block.
        assert_eq!(quadrant([r, b, r, b]), ('▌', r, b));
        // Only the bottom-right pixel differs.
        let (glyph, fg, bg) = quadrant([r, r, r, b]);
        assert_eq!((glyph, fg, bg), ('▛', r, b));
        // A diagonal.
        assert_eq!(quadrant([r, b, b, r]).0, '▚');
        // Nearly uniform cells are one solid color.
        assert_eq!(quadrant([r, r, [201, 1, 0], r]).0, '█');
    }

    #[test]
    fn quadrant_mode_doubles_the_horizontal_detail() {
        let mut wallpaper = Wallpaper::from_image(split_image(), 1.0, Fit::Stretch);
        wallpaper.across = 2;
        // One cell wide over a red/blue split: half of each color.
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        wallpaper.paint(&mut buffer, area);
        assert_eq!(buffer[(0, 0)].symbol(), "▌");
        // Downscaling blends a little across the boundary; each half keeps
        // its color.
        let Color::Rgb(r, _, b) = buffer[(0, 0)].fg else {
            panic!("expected an RGB foreground")
        };
        assert!(r > 150 && b < 50, "left half should be red: {r} {b}");
        let Color::Rgb(r, _, b) = buffer[(0, 0)].bg else {
            panic!("expected an RGB background")
        };
        assert!(b > 150 && r < 50, "right half should be blue: {r} {b}");
    }

    #[test]
    fn animated_gifs_loop_through_their_frames() {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, RgbaImage};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loop.gif");
        {
            let file = File::create(&path).unwrap();
            let mut encoder = GifEncoder::new(file);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                let image = RgbaImage::from_pixel(8, 8, image::Rgba(color));
                let frame = Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(100, 1));
                encoder.encode_frame(frame).unwrap();
            }
        }
        let spec = WallpaperSpec {
            image: path,
            opacity: 1.0,
            fit: Fit::Stretch,
        };
        let mut wallpaper = Wallpaper::load(&spec).unwrap();
        let start = wallpaper.next_frame_at().expect("an animated GIF plays");
        let red = wallpaper.source.get_pixel(0, 0).0;
        assert!(red[0] > 200 && red[2] < 50, "{red:?}");
        assert!(
            !wallpaper.tick(start - Duration::from_millis(1)),
            "not due yet"
        );
        assert!(wallpaper.tick(start));
        let blue = wallpaper.source.get_pixel(0, 0).0;
        assert!(blue[2] > 200 && blue[0] < 50, "{blue:?}");
        // After the last frame playback starts over.
        let again = wallpaper.next_frame_at().unwrap();
        assert!(wallpaper.tick(again));
        assert!(wallpaper.source.get_pixel(0, 0).0[0] > 200);
        wallpaper.set_paused(true);
        assert!(wallpaper.next_frame_at().is_none());
        wallpaper.set_paused(false);
        assert!(wallpaper.next_frame_at().is_some());
    }

    #[test]
    fn the_cell_behind_a_wide_character_is_not_overwritten() {
        let mut wallpaper = Wallpaper::from_image(split_image(), 1.0, Fit::Stretch);
        wallpaper.across = 1;
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "界", Style::default());
        wallpaper.paint(&mut buffer, area);
        assert_eq!(buffer[(0, 0)].symbol(), "界");
        assert_ne!(buffer[(1, 0)].symbol(), "▀");
        assert_eq!(buffer[(2, 0)].symbol(), "▀");
    }
}
