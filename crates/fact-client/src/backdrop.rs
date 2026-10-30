//! Full-resolution video behind Windows Terminal.
//!
//! In Windows Terminal's FACT profile the terminal background is transparent.
//! FACT keeps a borderless window exactly behind the terminal window and plays
//! the video in it at the window's own pixel size, so the video shows behind
//! the text at its real quality instead of as character cells. The window
//! never takes focus and stays out of the taskbar and Alt+Tab. It follows the
//! terminal window as it moves, resizes, minimizes or changes virtual desktop,
//! and it disappears with FACT.

use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, PatBlt, ReleaseDC, SetStretchBltMode, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLACKNESS, COLORONCOLOR, DIB_RGB_COLORS, HDC, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetAncestor, GetClassNameW,
    GetClientRect, GetForegroundWindow, GetWindow, GetWindowRect, IsIconic, IsWindow,
    IsWindowVisible, MsgWaitForMultipleObjects, PeekMessageW, PostThreadMessageW, RegisterClassW,
    SetWindowPos, ShowWindow, TranslateMessage, GA_ROOTOWNER, GW_HWNDPREV, GW_OWNER, HTTRANSPARENT,
    MA_NOACTIVATE, MSG, PM_REMOVE, QS_ALLINPUT, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW,
    SW_HIDE, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_NULL, WM_PAINT, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::wallpaper::{Fit, WallpaperSpec};

/// Fastest playback; most videos are 24-60 fps.
const MAX_FPS: f64 = 60.0;
/// Frame size cap (4K), so a huge window cannot exhaust memory.
const MAX_WIDTH: i32 = 3840;
const MAX_HEIGHT: i32 = 2160;
/// A resize restarts ffmpeg once the size has held this long.
const RESIZE_SETTLE: Duration = Duration::from_millis(250);
/// How often the window position is checked while the video is paused.
const IDLE_CHECK: Duration = Duration::from_millis(250);
/// ffmpeg restarts after it stops before playback gives up.
const ATTEMPTS: u32 = 3;
const TERMINAL_CLASS: &str = "CASCADIA_HOSTING_WINDOW_CLASS";

/// A full-resolution video window behind the Windows Terminal window.
pub struct Backdrop {
    control: Sender<Control>,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

enum Control {
    Pause(bool),
    Hide(bool),
}

impl Backdrop {
    /// Start playing `spec` behind the terminal window hosting FACT. Fails when
    /// that window cannot be found or the video window cannot be made.
    pub fn start(spec: &WallpaperSpec, ffmpeg: PathBuf) -> Result<Self, String> {
        let host = host_window().ok_or("Windows Terminal's window was not found")?;
        let host = host as isize;
        let spec = spec.clone();
        let (control, commands) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("fact-backdrop".into())
            .spawn(move || {
                let mut player = match Player::new(spec, ffmpeg, host as HWND) {
                    Ok(player) => player,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                player.run(&commands);
            })
            .map_err(|error| error.to_string())?;
        let thread_id = ready_rx
            .recv()
            .map_err(|_| "the video window stopped".to_string())??;
        Ok(Self {
            control,
            thread_id,
            thread: Some(thread),
        })
    }

    pub fn set_paused(&self, paused: bool) {
        self.send(Control::Pause(paused));
    }

    pub fn set_hidden(&self, hidden: bool) {
        self.send(Control::Hide(hidden));
    }

    fn send(&self, control: Control) {
        if self.control.send(control).is_ok() {
            unsafe { PostThreadMessageW(self.thread_id, WM_NULL, 0, 0) };
        }
    }
}

impl Drop for Backdrop {
    fn drop(&mut self) {
        // A closed channel tells the thread to stop; the message wakes it.
        drop(mem::replace(&mut self.control, mpsc::channel().0));
        unsafe { PostThreadMessageW(self.thread_id, WM_NULL, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

thread_local! {
    /// The frame on screen (width, height, BGRA rows), for repainting.
    static FRAME: RefCell<Option<(i32, i32, Vec<u8>)>> = const { RefCell::new(None) };
}

struct Stream {
    child: Child,
    frames: Receiver<Vec<u8>>,
    size: (i32, i32),
}

struct Player {
    window: HWND,
    /// The terminal window found at start, used if the console's owner
    /// cannot be read later.
    host: HWND,
    spec: WallpaperSpec,
    ffmpeg: PathBuf,
    fps: f64,
    duration: Option<f64>,
    stream: Option<Stream>,
    /// Window size in pixels.
    size: (i32, i32),
    rect: (i32, i32, i32, i32),
    visible: bool,
    paused: bool,
    hidden: bool,
    /// When the next frame is due.
    due: Instant,
    /// Seconds into the video, so a restart continues where it was.
    position: f64,
    restart_at: Option<Instant>,
    attempts: u32,
    hardware: bool,
}

impl Player {
    fn new(spec: WallpaperSpec, ffmpeg: PathBuf, host: HWND) -> Result<Self, String> {
        let window = unsafe { create_window()? };
        let (fps, duration) = probe(&ffmpeg, &spec.image);
        Ok(Self {
            window,
            host,
            spec,
            ffmpeg,
            fps: fps.unwrap_or(30.0).clamp(1.0, MAX_FPS),
            duration,
            stream: None,
            size: (0, 0),
            rect: (0, 0, 0, 0),
            visible: false,
            paused: false,
            hidden: false,
            due: Instant::now(),
            position: 0.0,
            restart_at: None,
            attempts: 0,
            hardware: true,
        })
    }

    fn run(&mut self, commands: &Receiver<Control>) {
        loop {
            let wait = self.wait_time(Instant::now());
            let millis = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX).max(1);
            unsafe { MsgWaitForMultipleObjects(0, ptr::null(), 0, millis, QS_ALLINPUT) };
            let mut msg: MSG = unsafe { mem::zeroed() };
            while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            loop {
                match commands.try_recv() {
                    Ok(Control::Pause(paused)) => {
                        if self.paused && !paused {
                            self.due = Instant::now();
                        }
                        self.paused = paused;
                    }
                    Ok(Control::Hide(hidden)) => {
                        if self.hidden && !hidden {
                            self.due = Instant::now();
                        }
                        self.hidden = hidden;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.stop();
                        unsafe { DestroyWindow(self.window) };
                        return;
                    }
                }
            }
            let now = Instant::now();
            self.track(now);
            self.play(now);
        }
    }

    fn playing(&self) -> bool {
        self.visible && !self.paused && !self.hidden && self.stream.is_some()
    }

    fn wait_time(&self, now: Instant) -> Duration {
        let mut wait = if self.playing() {
            self.due.saturating_duration_since(now)
        } else {
            IDLE_CHECK
        };
        if let Some(at) = self.restart_at {
            wait = wait.min(at.saturating_duration_since(now));
        }
        wait
    }

    /// Keep the window exactly behind the terminal window, or hidden when
    /// the terminal is minimized, on another virtual desktop, or gone.
    fn track(&mut self, now: Instant) {
        let host = console_owner().unwrap_or(self.host);
        self.host = host;
        let gone =
            unsafe { IsWindow(host) == 0 || IsIconic(host) != 0 || IsWindowVisible(host) == 0 };
        if self.hidden || gone || cloaked(host) {
            if self.visible {
                unsafe { ShowWindow(self.window, SW_HIDE) };
                self.visible = false;
            }
            return;
        }
        let bounds = frame_bounds(host);
        let rect = (bounds.left, bounds.top, bounds.right, bounds.bottom);
        let size = (
            (rect.2 - rect.0).clamp(1, MAX_WIDTH),
            (rect.3 - rect.1).clamp(1, MAX_HEIGHT),
        );
        let above = unsafe { GetWindow(self.window, GW_HWNDPREV) };
        if !self.visible || rect != self.rect || above != host {
            unsafe {
                SetWindowPos(
                    self.window,
                    host,
                    rect.0,
                    rect.1,
                    rect.2 - rect.0,
                    rect.3 - rect.1,
                    SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                );
            }
            self.visible = true;
            self.rect = rect;
        }
        if size != self.size {
            self.size = size;
            if self.stream.is_none() && self.restart_at.is_none() {
                self.start();
            } else {
                // Show the last frame scaled until the size settles.
                self.restart_at = Some(now + RESIZE_SETTLE);
                self.present();
            }
        }
    }

    fn play(&mut self, now: Instant) {
        if self.restart_at.is_some_and(|at| now >= at) {
            self.restart_at = None;
            self.start();
        }
        if !self.playing() || now < self.due {
            return;
        }
        let Some(stream) = &self.stream else {
            return;
        };
        let frame_time = Duration::from_secs_f64(1.0 / self.fps);
        match stream.frames.try_recv() {
            Ok(frame) => {
                let (width, height) = stream.size;
                FRAME.with(|current| *current.borrow_mut() = Some((width, height, frame)));
                self.present();
                self.position += 1.0 / self.fps;
                if let Some(duration) = self.duration.filter(|d| *d > 0.0) {
                    self.position %= duration;
                }
                // Keep the schedule unless far behind (after a stall).
                self.due = if now.saturating_duration_since(self.due) > frame_time * 2 {
                    now + frame_time
                } else {
                    self.due + frame_time
                };
                if self.attempts > 0 && self.position > 10.0 {
                    self.attempts = 0;
                }
            }
            Err(TryRecvError::Empty) => self.due = now + Duration::from_millis(4),
            Err(TryRecvError::Disconnected) => {
                self.stop();
                self.attempts += 1;
                // Hardware decoding is the usual suspect; retry without it.
                self.hardware = false;
                if self.attempts < ATTEMPTS {
                    self.restart_at = Some(now + Duration::from_secs(u64::from(self.attempts)));
                }
            }
        }
    }

    /// (Re)start ffmpeg for the current window size, where playback was.
    fn start(&mut self) {
        self.stop();
        let (width, height) = self.size;
        if width < 2 || height < 2 {
            return;
        }
        let scale = match self.spec.fit {
            Fit::Fill => format!(
                "scale={width}:{height}:force_original_aspect_ratio=increase:flags=lanczos,crop={width}:{height}"
            ),
            Fit::Fit => format!(
                "scale={width}:{height}:force_original_aspect_ratio=decrease:flags=lanczos,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
            ),
            Fit::Stretch => format!("scale={width}:{height}:flags=lanczos"),
            Fit::Center => format!(
                "scale='min(iw,{width})':'min(ih,{height})':force_original_aspect_ratio=decrease:flags=lanczos,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
            ),
        };
        let strength = self.spec.opacity.clamp(0.05, 1.0);
        let dim = if strength < 0.999 {
            format!(",colorchannelmixer=rr={strength:.3}:gg={strength:.3}:bb={strength:.3}")
        } else {
            String::new()
        };
        let mut command = Command::new(&self.ffmpeg);
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if self.hardware {
            command.args(["-hwaccel", "auto"]);
        }
        command.args(["-stream_loop", "-1"]);
        if self.position > 0.05 {
            command.args(["-ss", &format!("{:.3}", self.position)]);
        }
        command
            .arg("-i")
            .arg(&self.spec.image)
            .args(["-an", "-vf"])
            .arg(format!("fps={:.3},{scale}{dim},format=bgra", self.fps))
            .args(["-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        hide_console(&mut command);
        let Ok(mut child) = command.spawn() else {
            self.attempts = ATTEMPTS;
            return;
        };
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return;
        };
        let frame_bytes = width as usize * height as usize * 4;
        // Two frames in flight: ffmpeg waits for FACT, so pausing simply
        // stops reading.
        let (tx, rx) = mpsc::sync_channel(2);
        let spawned = thread::Builder::new()
            .name("fact-backdrop-video".into())
            .spawn(move || loop {
                let mut frame = vec![0u8; frame_bytes];
                if stdout.read_exact(&mut frame).is_err() || tx.send(frame).is_err() {
                    break;
                }
            });
        if spawned.is_err() {
            let _ = child.kill();
            return;
        }
        self.stream = Some(Stream {
            child,
            frames: rx,
            size: (width, height),
        });
        self.due = Instant::now();
    }

    fn stop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.child.kill();
            let _ = stream.child.wait();
        }
    }

    fn present(&self) {
        unsafe {
            let hdc = GetDC(self.window);
            if !hdc.is_null() {
                draw(hdc, self.size.0, self.size.1);
                ReleaseDC(self.window, hdc);
            }
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Draw the current frame over `width` x `height` (black before the first).
unsafe fn draw(hdc: HDC, width: i32, height: i32) {
    FRAME.with(|current| {
        let current = current.borrow();
        let Some((frame_width, frame_height, bits)) = current.as_ref() else {
            unsafe { PatBlt(hdc, 0, 0, width, height, BLACKNESS) };
            return;
        };
        let mut info: BITMAPINFO = unsafe { mem::zeroed() };
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: *frame_width,
            // Negative: rows run top to bottom, as ffmpeg writes them.
            biHeight: -*frame_height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..unsafe { mem::zeroed() }
        };
        unsafe {
            SetStretchBltMode(hdc, COLORONCOLOR);
            StretchDIBits(
                hdc,
                0,
                0,
                width,
                height,
                0,
                0,
                *frame_width,
                *frame_height,
                bits.as_ptr() as *const c_void,
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    });
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = unsafe { mem::zeroed() };
            let hdc = unsafe { BeginPaint(window, &mut paint) };
            let mut client: RECT = unsafe { mem::zeroed() };
            unsafe {
                GetClientRect(window, &mut client);
                draw(hdc, client.right - client.left, client.bottom - client.top);
                EndPaint(window, &paint);
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// A borderless window that never activates and stays out of the taskbar.
unsafe fn create_window() -> Result<HWND, String> {
    // Physical pixels on every monitor, to match the terminal window exactly.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let class = wide("FactVideoBackdrop");
    let mut class_info: WNDCLASSW = unsafe { mem::zeroed() };
    class_info.lpfnWndProc = Some(window_proc);
    class_info.hInstance = instance;
    class_info.lpszClassName = class.as_ptr();
    // Fails harmlessly when a previous backdrop already registered it.
    unsafe { RegisterClassW(&class_info) };
    let title = wide("FACT video wallpaper");
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if window.is_null() {
        return Err("could not create the video window".into());
    }
    Ok(window)
}

/// The Windows Terminal window hosting this console: the owner of its hidden
/// pseudo console window, else the terminal in the foreground (FACT was just
/// started there).
fn host_window() -> Option<HWND> {
    console_owner().or_else(|| {
        let foreground = unsafe { GetForegroundWindow() };
        is_terminal(foreground).then_some(foreground)
    })
}

fn console_owner() -> Option<HWND> {
    let console = unsafe { GetConsoleWindow() };
    if console.is_null() {
        return None;
    }
    let owner = unsafe { GetWindow(console, GW_OWNER) };
    if is_terminal(owner) {
        return Some(owner);
    }
    let root = unsafe { GetAncestor(console, GA_ROOTOWNER) };
    is_terminal(root).then_some(root)
}

fn is_terminal(window: HWND) -> bool {
    if window.is_null() {
        return false;
    }
    let mut class = [0u16; 64];
    let length = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    let length = usize::try_from(length).unwrap_or(0).min(class.len());
    String::from_utf16_lossy(&class[..length]) == TERMINAL_CLASS
}

/// The window's visible bounds (without the invisible resize borders).
fn frame_bounds(window: HWND) -> RECT {
    let mut rect: RECT = unsafe { mem::zeroed() };
    let result = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut rect as *mut RECT as *mut c_void,
            mem::size_of::<RECT>() as u32,
        )
    };
    if result < 0 {
        unsafe { GetWindowRect(window, &mut rect) };
    }
    rect
}

/// True when the window is on another virtual desktop (or otherwise hidden
/// by the compositor).
fn cloaked(window: HWND) -> bool {
    let mut value = 0u32;
    let result = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED as u32,
            &mut value as *mut u32 as *mut c_void,
            mem::size_of::<u32>() as u32,
        )
    };
    result >= 0 && value != 0
}

/// The video's frame rate and length in seconds, read from ffmpeg's report.
fn probe(ffmpeg: &Path, video: &Path) -> (Option<f64>, Option<f64>) {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(video)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    let Ok(output) = command.output() else {
        return (None, None);
    };
    parse_probe(&String::from_utf8_lossy(&output.stderr))
}

fn parse_probe(report: &str) -> (Option<f64>, Option<f64>) {
    let duration = report.split("Duration: ").nth(1).and_then(|rest| {
        let stamp = rest.split(',').next()?.trim();
        let mut parts = stamp.split(':');
        let hours: f64 = parts.next()?.parse().ok()?;
        let minutes: f64 = parts.next()?.parse().ok()?;
        let seconds: f64 = parts.next()?.parse().ok()?;
        Some(hours * 3600.0 + minutes * 60.0 + seconds)
    });
    let fps = report
        .lines()
        .find(|line| line.contains("Video:"))
        .and_then(|line| {
            line.split(',')
                .find_map(|part| part.trim().strip_suffix(" fps")?.trim().parse::<f64>().ok())
        })
        .filter(|fps| fps.is_finite() && *fps > 0.0);
    (fps, duration)
}

fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_frame_rate_and_length_from_the_ffmpeg_report() {
        let report = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'clip.mp4':\n  Duration: 00:01:02.50, start: 0.000000, bitrate: 8000 kb/s\n  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709, progressive), 1920x1080 [SAR 1:1 DAR 16:9], 7800 kb/s, 59.94 fps, 59.94 tbr, 60k tbn (default)\n";
        let (fps, duration) = parse_probe(report);
        assert_eq!(fps, Some(59.94));
        assert_eq!(duration, Some(62.5));
        assert_eq!(parse_probe("garbage"), (None, None));
    }
}
