use std::ffi::c_void;
use std::ops::{Deref, DerefMut};

use eyre::{bail, eyre, Context, Result};
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};

/// Owned Win32 handle that remembers whether it was opened for overlapped I/O.
pub struct Handle {
    pub raw: HANDLE,
    pub overlapped: bool,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.raw);
        }
    }
}

/// Manual-reset event used to wait for a single overlapped operation.
pub struct Event(HANDLE);

unsafe impl Send for Event {}

impl Event {
    pub fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, true, false, None)? }))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Page aligned buffer, required for unbuffered (`FILE_FLAG_NO_BUFFERING`) reads.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> Result<Self> {
        let ptr = unsafe { VirtualAlloc(None, len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if ptr.is_null() {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("VirtualAlloc of {} bytes failed", len));
        }
        Ok(Self {
            ptr: ptr as *mut u8,
            len,
        })
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFree(self.ptr as *mut c_void, 0, MEM_RELEASE);
        }
    }
}

/// An overlapped read that has been issued but not yet waited for. The OVERLAPPED lives in a box
/// so its address stays stable while the kernel owns it.
pub struct PendingRead {
    ov: Box<OVERLAPPED>,
    len: usize,
}

impl PendingRead {
    /// Blocks until the kernel is done with the read, ignoring its result.
    pub fn wait_event(&self) {
        unsafe {
            WaitForSingleObject(self.ov.hEvent, INFINITE);
        }
    }
}

/// Starts an overlapped read of `len` bytes at `offset` into `buf`.
///
/// # Safety
/// `buf` must stay valid and untouched until [`finish_read`] returns for the result.
pub unsafe fn begin_read(
    handle: &Handle,
    offset: u64,
    buf: *mut u8,
    len: usize,
    event: &Event,
) -> Result<PendingRead> {
    let mut ov = Box::new(OVERLAPPED::default());
    ov.Anonymous.Anonymous.Offset = offset as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    ov.hEvent = event.0;

    let res = ReadFile(
        handle.raw,
        Some(std::slice::from_raw_parts_mut(buf, len)),
        None,
        Some(&mut *ov),
    );
    if let Err(e) = res {
        if e.code() != ERROR_IO_PENDING.to_hresult() {
            return Err(e)
                .with_context(|| format!("ReadFile at {} ({} bytes) failed", offset, len));
        }
    }

    Ok(PendingRead { ov, len })
}

/// Waits for a read started with [`begin_read`] and checks that it was complete.
pub fn finish_read(handle: &Handle, read: PendingRead) -> Result<()> {
    let mut transferred = 0u32;
    unsafe { GetOverlappedResult(handle.raw, &*read.ov, &mut transferred, true) }
        .with_context(|| "GetOverlappedResult failed")?;
    if transferred as usize != read.len {
        bail!("Short read: {} of {} bytes", transferred, read.len);
    }
    Ok(())
}

/// Blocking read on an overlapped handle.
pub fn read_at(handle: &Handle, offset: u64, buf: &mut [u8]) -> Result<()> {
    let event = Event::new()?;
    let read = unsafe { begin_read(handle, offset, buf.as_mut_ptr(), buf.len(), &event)? };
    finish_read(handle, read)
}

/// Issues a DeviceIoControl and waits for it, regardless of whether the handle is overlapped.
/// Returns the number of output bytes.
pub fn ioctl(
    handle: &Handle,
    code: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
) -> windows::core::Result<u32> {
    let (in_ptr, in_len) = match input {
        Some(i) => (Some(i.as_ptr() as *const c_void), i.len() as u32),
        None => (None, 0),
    };
    let out_ptr = Some(output.as_mut_ptr() as *mut c_void);
    let out_len = output.len() as u32;
    let mut bytes = 0u32;

    unsafe {
        if !handle.overlapped {
            DeviceIoControl(
                handle.raw,
                code,
                in_ptr,
                in_len,
                out_ptr,
                out_len,
                Some(&mut bytes),
                None,
            )?;
            return Ok(bytes);
        }

        let event = Event::new().map_err(|_| windows::core::Error::from_thread())?;
        let mut ov = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        if let Err(e) = DeviceIoControl(
            handle.raw,
            code,
            in_ptr,
            in_len,
            out_ptr,
            out_len,
            None,
            Some(&mut ov),
        ) {
            if e.code() != ERROR_IO_PENDING.to_hresult() {
                return Err(e);
            }
        }
        GetOverlappedResult(handle.raw, &ov, &mut bytes, true)?;
        Ok(bytes)
    }
}

/// Reads a POD value with an ioctl that takes no input.
pub fn ioctl_struct<T: Default>(handle: &Handle, code: u32) -> Result<T> {
    let mut value = T::default();
    let out =
        unsafe { std::slice::from_raw_parts_mut(&mut value as *mut T as *mut u8, size_of::<T>()) };
    ioctl(handle, code, None, out).map_err(|e| eyre!(e))?;
    Ok(value)
}
