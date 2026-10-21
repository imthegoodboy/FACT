//! Windows ConPTY process engine.
//!
//! Every FACT pane owns one pseudo console, one OS process (normally a shell),
//! and a kill-on-close job object containing that process tree. Output is read
//! on a dedicated thread and delivered over a bounded channel so a noisy pane
//! cannot grow the daemon's memory without bound.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

/// Maximum bytes accepted by a single [`PtyProcess::write`] call.
pub use fact_protocol::MAX_INPUT_CHUNK;
const OUTPUT_CHUNK: usize = 32 * 1024;
const INPUT_QUEUE: usize = 64;
const MAX_QUEUED_INPUT_BYTES: usize = INPUT_QUEUE * MAX_INPUT_CHUNK;
const OUTPUT_QUEUE: usize = 512;

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("could not {context}: {source}")]
    Windows {
        context: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("invalid process options: {0}")]
    Invalid(String),
    #[error("the pane process is no longer running")]
    Exited,
    #[error("the pane input queue is full; retry after the shell has consumed input")]
    InputBackpressure,
    #[error("input chunk is too large ({0} bytes)")]
    TooMuchInput(usize),
    #[error("the pseudo console is closed")]
    Closed,
}

impl PtyError {
    fn win(context: &'static str) -> Self {
        Self::Windows {
            context,
            source: io::Error::last_os_error(),
        }
    }
}

/// Everything needed to start one pane process.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Custom environment entries merged over the daemon environment.
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

impl SpawnOptions {
    pub fn new(command: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            cols,
            rows,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyExit {
    pub code: i32,
    pub observed_at: Instant,
}

struct PaneState {
    exit: Option<PtyExit>,
    input_error: Option<String>,
    closed: bool,
}

struct Shared {
    process: AtomicUsize,
    job: AtomicUsize,
    conpty: Mutex<Option<HPCON>>,
    state: Mutex<PaneState>,
    alive: AtomicBool,
    queued_input_bytes: AtomicUsize,
}

impl Shared {
    fn close_conpty(&self) {
        let handle = self.conpty.lock().ok().and_then(|mut guard| guard.take());
        if let Some(handle) = handle {
            // ClosePseudoConsole drains the output pipe. The dedicated reader
            // thread continues draining while this call waits, so this cannot
            // deadlock the way closing without a reader would.
            unsafe { ClosePseudoConsole(handle) };
        }
    }

    fn terminate(&self) {
        let job = self.job.load(Ordering::Acquire);
        if job != 0 {
            let job = job as HANDLE;
            unsafe {
                TerminateJobObject(job, 1);
            }
        } else {
            let process = self.process.load(Ordering::Acquire);
            if process != 0 {
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(process as HANDLE, 1);
                }
            }
        }
        self.alive.store(false, Ordering::Release);
        self.close_conpty();
    }
}

/// A live ConPTY process and its bounded input/output channels.
pub struct PtyProcess {
    pid: u32,
    command: String,
    input_tx: Option<SyncSender<Vec<u8>>>,
    output_rx: Receiver<Vec<u8>>,
    shared: Arc<Shared>,
    started: Instant,
}

impl PtyProcess {
    /// Create the two anonymous pipes, pseudo console, process and job object.
    pub fn spawn(options: SpawnOptions) -> Result<Self, PtyError> {
        if options.command.trim().is_empty() {
            return Err(PtyError::Invalid("command may not be empty".into()));
        }
        if options.command.contains('\0') {
            return Err(PtyError::Invalid("command contains NUL".into()));
        }
        for arg in &options.args {
            if arg.contains('\0') {
                return Err(PtyError::Invalid("argument contains NUL".into()));
            }
        }
        let cols = options.cols.clamp(1, i16::MAX as u16);
        let rows = options.rows.clamp(1, i16::MAX as u16);

        let (host_input_read, host_input_write) = create_pipe()?;
        let (host_output_read, host_output_write) = create_pipe()?;

        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        let mut conpty: HPCON = 0;
        let hr = unsafe {
            CreatePseudoConsole(
                size,
                host_input_read as HANDLE,
                host_output_write as HANDLE,
                0,
                &mut conpty,
            )
        };
        if hr < 0 {
            close_handle(host_input_read);
            close_handle(host_input_write);
            close_handle(host_output_read);
            close_handle(host_output_write);
            return Err(PtyError::Invalid(format!(
                "CreatePseudoConsole failed with HRESULT 0x{hr:08X}"
            )));
        }

        // ConPTY has duplicated/referenced these handles. Keeping our copies
        // open would prevent EOF after the child exits.
        close_handle(host_input_read);
        close_handle(host_output_write);

        let result = unsafe { spawn_child(&options, conpty) };
        let (process_info, job) = match result {
            Ok(values) => values,
            Err(error) => {
                unsafe { ClosePseudoConsole(conpty) };
                close_handle(host_input_write);
                close_handle(host_output_read);
                return Err(error);
            }
        };

        let shared = Arc::new(Shared {
            process: AtomicUsize::new(process_info.hProcess as usize),
            job: AtomicUsize::new(job),
            conpty: Mutex::new(Some(conpty)),
            state: Mutex::new(PaneState {
                exit: None,
                input_error: None,
                closed: false,
            }),
            alive: AtomicBool::new(true),
            queued_input_bytes: AtomicUsize::new(0),
        });

        let pid = process_info.dwProcessId;
        if !process_info.hThread.is_null() {
            close_handle(process_info.hThread as usize);
        }

        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
        let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(OUTPUT_QUEUE);

        spawn_input_writer(host_input_write, input_rx, shared.clone());
        spawn_output_reader(host_output_read, output_tx, shared.clone());
        spawn_exit_monitor(shared.clone());

        Ok(Self {
            pid,
            command: options.command,
            input_tx: Some(input_tx),
            output_rx,
            shared,
            started: Instant::now(),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    /// Queue terminal input. The queue is bounded; callers receive
    /// [`PtyError::InputBackpressure`] instead of blocking the daemon.
    pub fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        if bytes.len() > MAX_INPUT_CHUNK {
            return Err(PtyError::TooMuchInput(bytes.len()));
        }
        self.queue_input(bytes)
    }

    /// Queue a whole normalized paste, including optional bracket markers, as
    /// one ordered item. Rejection cannot leave half a bracketed paste behind.
    pub fn write_paste(&self, bytes: &[u8]) -> Result<(), PtyError> {
        if bytes.len() > fact_protocol::MAX_PASTE_BYTES + 12 {
            return Err(PtyError::TooMuchInput(bytes.len()));
        }
        self.queue_input(bytes)
    }

    fn queue_input(&self, bytes: &[u8]) -> Result<(), PtyError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !self.is_alive() {
            return Err(PtyError::Exited);
        }
        let tx = self.input_tx.as_ref().ok_or(PtyError::Exited)?;
        // Preserve the original 16 MiB memory bound even when queue items can
        // contain a 4 MiB paste instead of a normal 256 KiB input chunk.
        self.shared
            .queued_input_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(bytes.len())
                    .filter(|total| *total <= MAX_QUEUED_INPUT_BYTES)
            })
            .map_err(|_| PtyError::InputBackpressure)?;
        if tx.try_send(bytes.to_vec()).is_err() {
            self.shared
                .queued_input_bytes
                .fetch_sub(bytes.len(), Ordering::AcqRel);
            return Err(PtyError::InputBackpressure);
        }
        Ok(())
    }

    /// Change the pseudo console dimensions. The terminal model is resized by
    /// the daemon separately, before calling this method.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        let cols = cols.clamp(1, i16::MAX as u16);
        let rows = rows.clamp(1, i16::MAX as u16);
        let conpty = self
            .shared
            .conpty
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .ok_or(PtyError::Closed)?;
        let hr = unsafe {
            ResizePseudoConsole(
                conpty,
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
            )
        };
        if hr < 0 {
            return Err(PtyError::Invalid(format!(
                "ResizePseudoConsole failed with HRESULT 0x{hr:08X}"
            )));
        }
        Ok(())
    }

    /// Non-blocking output read used by the daemon tick.
    pub fn try_read(&self) -> Option<Vec<u8>> {
        self.output_rx.try_recv().ok()
    }

    /// The reader has reached EOF and will enqueue no further output. Check
    /// this before draining the queue to establish that an empty read is final.
    pub fn output_closed(&self) -> bool {
        self.shared.state.lock().is_ok_and(|state| state.closed)
    }

    /// Blocking helper for tests and tools. Returns `None` on timeout.
    pub fn read_timeout(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.output_rx.recv_timeout(timeout).ok()
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    pub fn exit(&self) -> Option<PtyExit> {
        self.shared.state.lock().ok().and_then(|state| state.exit)
    }

    pub fn input_error(&self) -> Option<String> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.input_error.clone())
    }

    /// Wait (bounded) for the process to exit, returning its exit code.
    pub fn wait_for_exit(&self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(exit) = self.exit() {
                return Some(exit.code);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Terminate the complete process tree. The pane remains readable until
    /// buffered output has been drained.
    pub fn kill(&self) {
        self.shared.terminate();
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.input_tx.take();
        self.shared.terminate();

        let process = self.shared.process.load(Ordering::Acquire);
        if process != 0 {
            // Give the monitor and reader a brief chance to observe exit. The
            // wait is on the process object and is safe with a concurrent wait.
            unsafe {
                WaitForSingleObject(process as HANDLE, 2_000);
            }
        }
        self.shared.close_conpty();

        let process = self.shared.process.swap(0, Ordering::AcqRel);
        if process != 0 {
            close_handle(process);
        }
        let job = self.shared.job.swap(0, Ordering::AcqRel);
        if job != 0 {
            close_handle(job);
        }
    }
}

fn create_pipe() -> Result<(usize, usize), PtyError> {
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();
    let ok = unsafe { CreatePipe(&mut read, &mut write, ptr::null::<SECURITY_ATTRIBUTES>(), 0) };
    if ok == 0 {
        return Err(PtyError::win("create an anonymous pipe"));
    }
    Ok((read as usize, write as usize))
}

fn close_handle(handle: usize) {
    if handle != 0 && handle as HANDLE != INVALID_HANDLE_VALUE {
        unsafe {
            CloseHandle(handle as HANDLE);
        }
    }
}

unsafe fn spawn_child(
    options: &SpawnOptions,
    conpty: HPCON,
) -> Result<(PROCESS_INFORMATION, usize), PtyError> {
    let mut attr_size = 0usize;
    InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attr_size);
    if attr_size == 0 {
        return Err(PtyError::win("size the process attribute list"));
    }
    let mut attr_storage = vec![0u8; attr_size];
    let attr_list = attr_storage.as_mut_ptr() as *mut c_void;
    if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
        return Err(PtyError::win("initialize the process attribute list"));
    }

    let attribute_ok = UpdateProcThreadAttribute(
        attr_list,
        0,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
        conpty as *const c_void,
        mem::size_of::<HPCON>(),
        ptr::null_mut(),
        ptr::null(),
    );
    if attribute_ok == 0 {
        DeleteProcThreadAttributeList(attr_list);
        return Err(PtyError::win("attach the pseudo console attribute"));
    }

    let mut startup: STARTUPINFOEXW = mem::zeroed();
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    // Explicitly null standard handles stop unrelated inheritable handles from
    // leaking into the pane. ConPTY supplies the actual console.
    startup.StartupInfo.hStdInput = ptr::null_mut();
    startup.StartupInfo.hStdOutput = ptr::null_mut();
    startup.StartupInfo.hStdError = ptr::null_mut();
    startup.lpAttributeList = attr_list;

    let application = wide(&options.command);
    let mut command_line = command_line(&options.command, &options.args);
    let cwd = options
        .cwd
        .as_ref()
        .map(|path| wide(&path.to_string_lossy()))
        .unwrap_or_default();
    let env_block = build_environment_block(&options.env);

    let mut process_info: PROCESS_INFORMATION = mem::zeroed();
    let success = CreateProcessW(
        application.as_ptr(),
        command_line.as_mut_ptr(),
        ptr::null::<SECURITY_ATTRIBUTES>(),
        ptr::null::<SECURITY_ATTRIBUTES>(),
        0,
        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
        env_block.as_ptr() as *const c_void,
        if cwd.is_empty() {
            ptr::null()
        } else {
            cwd.as_ptr()
        },
        &startup.StartupInfo as *const STARTUPINFOW,
        &mut process_info,
    );
    DeleteProcThreadAttributeList(attr_list);
    drop(attr_storage);

    if success == 0 {
        return Err(PtyError::win("start the pane process"));
    }

    let job = CreateJobObjectW(ptr::null(), ptr::null());
    if job.is_null() {
        // The pane still works; killing it will fall back to TerminateProcess.
        return Ok((process_info, 0));
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set = SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &limits as *const _ as *const c_void,
        mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    );
    if set == 0 || AssignProcessToJobObject(job, process_info.hProcess) == 0 {
        CloseHandle(job);
        return Ok((process_info, 0));
    }
    Ok((process_info, job as usize))
}

fn spawn_input_writer(handle: usize, receiver: Receiver<Vec<u8>>, shared: Arc<Shared>) {
    thread::Builder::new()
        .name("fact-pty-input".into())
        .spawn(move || {
            let mut file = unsafe { File::from_raw_handle(handle as *mut _) };
            while let Ok(chunk) = receiver.recv() {
                let result = file.write_all(&chunk);
                shared
                    .queued_input_bytes
                    .fetch_sub(chunk.len(), Ordering::AcqRel);
                if result.is_err() {
                    if let Ok(mut state) = shared.state.lock() {
                        state.input_error = Some("could not write pane input".into());
                    }
                    break;
                }
                if file.flush().is_err() {
                    break;
                }
            }
            // Dropping `file` closes the write end and lets the shell see EOF.
        })
        .expect("spawn ConPTY input thread");
}

fn spawn_output_reader(handle: usize, sender: SyncSender<Vec<u8>>, shared: Arc<Shared>) {
    thread::Builder::new()
        .name("fact-pty-output".into())
        .spawn(move || {
            let mut file = unsafe { File::from_raw_handle(handle as *mut _) };
            let mut buffer = vec![0u8; OUTPUT_CHUNK];
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if sender.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => break,
                    Err(_) => break,
                }
            }
            drop(sender);
            if let Ok(mut state) = shared.state.lock() {
                state.closed = true;
            }
        })
        .expect("spawn ConPTY output thread");
}

fn spawn_exit_monitor(shared: Arc<Shared>) {
    thread::Builder::new()
        .name("fact-pty-exit".into())
        .spawn(move || {
            let process = shared.process.load(Ordering::Acquire);
            if process == 0 {
                return;
            }
            let wait = unsafe { WaitForSingleObject(process as HANDLE, INFINITE) };
            let mut code: u32 = 1;
            if wait == WAIT_OBJECT_0 {
                unsafe {
                    GetExitCodeProcess(process as HANDLE, &mut code);
                }
            } else {
                code = 1;
            }
            if let Ok(mut state) = shared.state.lock() {
                state.exit = Some(PtyExit {
                    code: code as i32,
                    observed_at: Instant::now(),
                });
            }
            shared.alive.store(false, Ordering::Release);
            // EOF for the output pipe depends on closing the pseudo console.
            shared.close_conpty();
        })
        .expect("spawn ConPTY exit thread");
}

fn command_line(command: &str, args: &[String]) -> Vec<u16> {
    let mut line = String::new();
    line.push_str(&quote_arg(command));
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    // CreateProcessW may modify the command line buffer; keep it mutable.
    let mut wide = wide(&line);
    // `wide` already has one trailing NUL.
    wide.reserve(1);
    wide
}

fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\\')
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Merge the daemon environment with pane-specific values and serialize a
/// double-NUL-terminated, case-insensitively sorted Unicode environment block.
fn build_environment_block(custom: &BTreeMap<String, String>) -> Vec<u16> {
    let mut entries: HashMap<String, (String, String)> = HashMap::new();
    let block = unsafe { GetEnvironmentStringsW() };
    if !block.is_null() {
        let mut cursor = block;
        loop {
            let len = unsafe {
                let mut len = 0usize;
                while *cursor.add(len) != 0 {
                    len += 1;
                }
                len
            };
            if len == 0 {
                break;
            }
            let slice = unsafe { std::slice::from_raw_parts(cursor, len) };
            let text = String::from_utf16_lossy(slice);
            if let Some((key, value)) = text.split_once('=') {
                if !key.is_empty() {
                    entries.insert(
                        key.to_ascii_uppercase(),
                        (key.to_string(), value.to_string()),
                    );
                }
            }
            cursor = unsafe { cursor.add(len + 1) };
        }
        unsafe { FreeEnvironmentStringsW(block) };
    }

    for (key, value) in custom {
        entries.insert(key.to_ascii_uppercase(), (key.clone(), value.clone()));
    }

    let mut ordered: Vec<(String, String)> = entries.into_values().collect();
    ordered.sort_by_key(|entry| entry.0.to_ascii_uppercase());
    let mut out = Vec::new();
    for (key, value) in ordered {
        out.extend(OsStr::new(&format!("{key}={value}")).encode_wide());
        out.push(0);
    }
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    out
}

/// Quote a command line according to the rules used by CommandLineToArgvW.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_arguments_like_windows() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("with space"), "\"with space\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("a\\b"), "\"a\\b\"");
        assert_eq!(quote_arg("a\\"), "\"a\\\\\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn environment_block_is_double_nul_terminated() {
        let mut custom = BTreeMap::new();
        custom.insert("FACT_TEST_UNIT".to_string(), "yes".to_string());
        let block = build_environment_block(&custom);
        assert!(block.ends_with(&[0, 0]));
        let text = String::from_utf16_lossy(&block);
        assert!(text.contains("FACT_TEST_UNIT=yes"));
    }
}
