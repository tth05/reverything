pub mod file_attribute;
pub mod file_record;
pub mod volume;
pub mod mft;
pub mod index;
pub mod journal;

pub fn try_close_handle(handle: windows::Win32::Foundation::HANDLE) -> eyre::Result<()> {
    use eyre::WrapErr;

    unsafe {
        if windows::Win32::Foundation::CloseHandle(handle).is_ok() {
            return Ok(());
        }

        Err(eyre::Report::new(std::io::Error::last_os_error())).with_context(|| "Failed to close handle")
    }
}