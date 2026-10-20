//! Authenticated local Windows named-pipe transport.
//!
//! Pipes are created in local-only mode with a protected DACL granting access
//! to the current user's SID (and SYSTEM). `FILE_FLAG_FIRST_PIPE_INSTANCE`
//! prevents another local process from squatting the daemon's pipe name.
//! Clients additionally open the pipe with an identification-only impersonation
//! level, so a malicious server cannot impersonate the client.
//!
//! Application-level authentication is still required: the daemon only accepts
//! `Hello` messages carrying the random token stored in the ACL-protected
//! `daemon.json` file.

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetSecurityDescriptorLength, GetTokenInformation, SetFileSecurityW, TokenUser,
    DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PeekNamedPipe,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

const LOCAL_PIPE_PREFIX: &str = r"\\.\pipe\";
const BUFFER_SIZE: u32 = 64 * 1024;
const CONNECT_RETRIES: usize = 40;

/// A protected security descriptor owned by the process.
pub struct PipeSecurity {
    blob: Box<[u8]>,
    sddl: String,
}

impl std::fmt::Debug for PipeSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeSecurity")
            .field("sddl", &self.sddl)
            .finish_non_exhaustive()
    }
}

impl PipeSecurity {
    /// DACL that grants full access only to the current user. SYSTEM is given
    /// access so services such as backup/antivirus can inspect the pipe.
    pub fn current_user_only() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let sddl = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)");
        Self::from_sddl(&sddl)
    }

    /// Build a descriptor from a valid SDDL string used by Windows security APIs.
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = wide(sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = unsafe { GetSecurityDescriptorLength(descriptor) } as usize;
        let blob = if length == 0 {
            unsafe { LocalFree(descriptor) };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "security descriptor has zero length",
            ));
        } else {
            let slice = unsafe { std::slice::from_raw_parts(descriptor as *const u8, length) };
            let copy = slice.to_vec().into_boxed_slice();
            unsafe { LocalFree(descriptor) };
            copy
        };
        Ok(Self {
            blob,
            sddl: sddl.to_string(),
        })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.blob.as_ptr() as *mut c_void,
            bInheritHandle: 0,
        }
    }

    /// The textual descriptor, useful for `fact doctor` diagnostics and tests.
    pub fn sddl(&self) -> &str {
        &self.sddl
    }
}

/// Restrict an existing file or directory to the current user.
pub fn secure_file_acl(path: &Path) -> io::Result<()> {
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    let sid = current_user_sid()?;
    let sddl = if is_dir {
        format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)")
    } else {
        format!("D:P(A;;FA;;;{sid})(A;;FA;;;SY)")
    };
    let security = PipeSecurity::from_sddl(&sddl)?;
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        SetFileSecurityW(
            wide_path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            security.blob.as_ptr() as PSECURITY_DESCRIPTOR,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read the DACL of an existing file or directory as SDDL, for diagnostics.
pub fn file_dacl_sddl(path: &Path) -> io::Result<String> {
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let mut text: *mut u16 = ptr::null_mut();
    let mut text_len = 0u32;
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            &mut text_len,
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            LocalFree(descriptor);
        }
        return Err(error);
    }
    let result = if text.is_null() || text_len == 0 {
        String::new()
    } else {
        // The Win32 length includes the terminating null (and some Windows
        // versions have been observed to report a second padding null).
        let mut len = text_len as usize;
        while len > 0 && unsafe { *text.add(len - 1) } == 0 {
            len -= 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(text, len) };
        String::from_utf16_lossy(slice)
    };
    unsafe {
        LocalFree(text as *mut c_void);
        LocalFree(descriptor);
    }
    Ok(result)
}

/// The current process user's SID as a string, for ACL diagnostics.
pub fn current_user_sid_string() -> io::Result<String> {
    current_user_sid()
}

/// Listening endpoint for a local named pipe.
pub struct PipeListener {
    name: String,
    security: PipeSecurity,
    next: Option<HANDLE>,
}

impl std::fmt::Debug for PipeListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeListener")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

// SAFETY: a listener is owned by exactly one thread after creation. The raw
// pointer inside `security` refers to an owned security-descriptor blob; it is
// only read synchronously while creating pipe instances, never shared.
unsafe impl Send for PipeListener {}

impl PipeListener {
    /// Bind the first instance. Fails if another process has already created an
    /// instance with this name.
    pub fn bind(name: &str) -> io::Result<Self> {
        validate_pipe_name(name)?;
        let security = PipeSecurity::current_user_only()?;
        let handle = create_instance(name, &security, true)?;
        Ok(Self {
            name: name.to_string(),
            security,
            next: Some(handle),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Block until a client connects. The peer's process token is checked
    /// against the daemon's user SID before the connection is handed back.
    pub fn accept(&mut self) -> io::Result<PipeConnection> {
        let handle = match self.next.take() {
            Some(handle) => handle,
            None => create_instance(&self.name, &self.security, false)?,
        };

        let connected = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) };
        if connected == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_PIPE_CONNECTED {
                unsafe { CloseHandle(handle) };
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }

        let mut peer_pid = 0u32;
        let ok = unsafe { GetNamedPipeClientProcessId(handle, &mut peer_pid) };
        if ok == 0 {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(error);
        }

        let same_user = peer_is_current_user(peer_pid)?;
        if !same_user {
            unsafe { CloseHandle(handle) };
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named-pipe client is not the current user",
            ));
        }

        let file = unsafe { File::from_raw_handle(handle as *mut _) };
        Ok(PipeConnection {
            file,
            client_pid: Some(peer_pid),
            server_pid: None,
        })
    }
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        if let Some(handle) = self.next.take() {
            unsafe { CloseHandle(handle) };
        }
    }
}

/// A connected duplex named-pipe stream.
pub struct PipeConnection {
    file: File,
    client_pid: Option<u32>,
    server_pid: Option<u32>,
}

impl std::fmt::Debug for PipeConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeConnection")
            .field("client_pid", &self.client_pid)
            .field("server_pid", &self.server_pid)
            .finish_non_exhaustive()
    }
}

impl PipeConnection {
    /// PID of the client that connected to this server-side pipe.
    pub fn client_pid(&self) -> Option<u32> {
        self.client_pid
    }

    /// PID of the server for a client-side connection.
    pub fn server_pid(&self) -> Option<u32> {
        self.server_pid
    }

    /// Bytes currently queued in the pipe, without blocking. Event readers use
    /// this to remain cancellable while waiting for the next frame.
    pub fn bytes_available(&self) -> io::Result<u32> {
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                self.file.as_raw_handle() as HANDLE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(available)
    }
}

impl Read for PipeConnection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for PipeConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Connect a client to a local FACT daemon pipe.
///
/// Retries briefly because a just-started daemon may still be creating the next
/// listening instance between sequential connections.
pub fn client_connect(name: &str) -> io::Result<PipeConnection> {
    validate_pipe_name(name)?;
    let wide_name = wide(name);
    let desired_access = windows_sys::Win32::Foundation::GENERIC_READ
        | windows_sys::Win32::Foundation::GENERIC_WRITE;
    let flags: FILE_FLAGS_AND_ATTRIBUTES = SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
    let mut last_error = None;
    for attempt in 0..CONNECT_RETRIES {
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                desired_access,
                0,
                ptr::null::<SECURITY_ATTRIBUTES>(),
                OPEN_EXISTING,
                flags,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            let mut server_pid = 0u32;
            let ok = unsafe {
                windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(
                    handle,
                    &mut server_pid,
                )
            };
            let server_pid = (ok != 0).then_some(server_pid);
            let file = unsafe { File::from_raw_handle(handle as *mut _) };
            return Ok(PipeConnection {
                file,
                client_pid: None,
                server_pid,
            });
        }
        let error = io::Error::last_os_error();
        let raw_error = error.raw_os_error();
        last_error = Some(error);
        if attempt + 1 < CONNECT_RETRIES {
            if raw_error == Some(ERROR_PIPE_BUSY as i32) {
                // WaitNamedPipe returns false on timeout; retry anyway.
                unsafe {
                    windows_sys::Win32::System::Pipes::WaitNamedPipeW(wide_name.as_ptr(), 250)
                };
            } else {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "named pipe is unavailable")))
}

/// Compatibility helper for server code that prefers a free function.
pub fn server_accept(listener: &mut PipeListener) -> io::Result<PipeConnection> {
    listener.accept()
}

fn create_instance(name: &str, security: &PipeSecurity, first: bool) -> io::Result<HANDLE> {
    let wide_name = wide(name);
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let attributes = security.attributes();
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_SIZE,
            BUFFER_SIZE,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn validate_pipe_name(name: &str) -> io::Result<()> {
    if !name.starts_with(LOCAL_PIPE_PREFIX) || name.len() <= LOCAL_PIPE_PREFIX.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pipe name must start with {LOCAL_PIPE_PREFIX:?}"),
        ));
    }
    if name.contains('\0') || name.contains('\\') && name[LOCAL_PIPE_PREFIX.len()..].contains('\\')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipe name must be a single local pipe component",
        ));
    }
    Ok(())
}

fn current_user_sid() -> io::Result<String> {
    let sid = token_user_sid(unsafe { GetCurrentProcess() })?;
    sid_to_string(&sid)
}

fn token_user_sid(process: HANDLE) -> io::Result<Vec<u8>> {
    let mut token: HANDLE = ptr::null_mut();
    let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut needed = 0u32;
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // `TOKEN_USER` contains pointers, so the buffer must be aligned for
        // `usize`, not merely for `u8`.
        let word = std::mem::size_of::<usize>();
        let mut buffer = vec![0usize; (needed as usize).div_ceil(word)];
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if needed as usize > buffer.len() * word {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "token information buffer is too small",
            ));
        }
        let token_user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        sid_bytes(token_user.User)
    })();
    unsafe { CloseHandle(token) };
    result
}

fn sid_bytes(entry: SID_AND_ATTRIBUTES) -> io::Result<Vec<u8>> {
    if entry.Sid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token user has a null SID",
        ));
    }
    // A SID is at most 68 bytes (8-byte header + 15 sub-authorities).
    let header = unsafe { std::slice::from_raw_parts(entry.Sid as *const u8, 8) };
    let count = header[1] as usize;
    let length = 8 + count * 4;
    let bytes = unsafe { std::slice::from_raw_parts(entry.Sid as *const u8, length) };
    Ok(bytes.to_vec())
}

fn sid_to_string(sid: &[u8]) -> io::Result<String> {
    let mut string_ptr: *mut u16 = ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(sid.as_ptr() as *mut c_void, &mut string_ptr) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0usize;
    while unsafe { *string_ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(string_ptr, len) };
    let result = String::from_utf16_lossy(slice);
    unsafe { LocalFree(string_ptr as *mut c_void) };
    Ok(result)
}

fn peer_is_current_user(pid: u32) -> io::Result<bool> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let peer = token_user_sid(process);
    unsafe { CloseHandle(process) };
    let peer = peer?;
    let current = token_user_sid(unsafe { GetCurrentProcess() })?;
    let equal = peer.as_ptr() as *mut c_void;
    let current_ptr = current.as_ptr() as *mut c_void;
    Ok(unsafe { EqualSid(equal, current_ptr) } != 0)
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_pipe_names_are_validated() {
        assert!(validate_pipe_name(r"\\.\pipe\fact-test").is_ok());
        assert!(validate_pipe_name(r"\\server\pipe\fact").is_err());
        assert!(validate_pipe_name(r"\\.\pipe\").is_err());
        assert!(validate_pipe_name(r"\\.\pipe\a\b").is_err());
    }

    #[test]
    fn current_user_security_descriptor_parses() {
        let security = PipeSecurity::current_user_only().unwrap();
        assert!(security.sddl().contains("S-1-"));
        assert!(security.sddl().starts_with("D:P"));
        assert!(!security.sddl().contains("WD)"));
    }

    #[test]
    fn file_acl_diagnostics_report_the_current_user() {
        let path = std::env::temp_dir().join(format!(
            "fact-acl-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"acl test").unwrap();
        secure_file_acl(&path).unwrap();
        let dacl = file_dacl_sddl(&path).unwrap();
        let sid = current_user_sid_string().unwrap();
        // SDDL renders well-known account SIDs (such as the built-in
        // Administrator used by CI runners) as aliases instead of raw SIDs.
        let alias = ["SY", "LA", "BA", "DA", "EA", "SA"]
            .iter()
            .any(|alias| dacl.contains(&format!(";;;{alias})")));
        assert!(
            dacl.contains(&sid) || alias,
            "DACL {dacl} did not grant the current user {sid}"
        );
        assert_eq!(
            dacl.matches("(A;;").count(),
            2,
            "DACL should grant exactly the current user and SYSTEM: {dacl}"
        );
        assert!(dacl.contains(";;;SY)"), "DACL is missing SYSTEM: {dacl}");
        assert!(!dacl.ends_with('\0'));
        assert!(!dacl.contains(";;;WD)"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pipe_round_trip_and_same_user_check() {
        let name = format!(
            r"\\.\pipe\fact-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let mut listener = PipeListener::bind(&name).unwrap();
        let client = std::thread::spawn({
            let name = name.clone();
            move || {
                let mut connection = client_connect(&name).unwrap();
                connection.write_all(b"ping").unwrap();
                let mut buffer = [0u8; 4];
                connection.read_exact(&mut buffer).unwrap();
                assert_eq!(&buffer, b"pong");
            }
        });
        let mut server = listener.accept().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while server.bytes_available().unwrap() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(server.bytes_available().unwrap(), 4);
        let mut buffer = [0u8; 4];
        server.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"ping");
        server.write_all(b"pong").unwrap();
        assert_eq!(server.client_pid(), Some(client_pid_hint()));
        client.join().unwrap();
    }

    fn client_pid_hint() -> u32 {
        std::process::id()
    }
}
