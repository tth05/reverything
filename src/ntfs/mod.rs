use eyre::{eyre, Result};
use windows::core::PWSTR;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::Diagnostics::Debug::{FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM};

pub mod file_attribute;
pub mod file_record;
pub mod volume;
pub mod mft;
pub mod index;

pub fn get_last_error_message() -> Result<String> {
    let mut buf = [0u16; 500];
    let err = unsafe { GetLastError().0 };
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM,
            None,
            err,
            0,
            PWSTR(buf.as_mut_ptr()),
            buf.len() as u32,
            None,
        )
    };

    if n == 0 {
        return Err(eyre!("FormatMessageW failed: {}", unsafe {
            GetLastError().0
        }));
    }

    Ok(format!("(0x{:x?}) {}", err, u16_buf_to_string(&buf)?))
}

fn u16_buf_to_string(buf: &[u16]) -> Result<String> {
    Ok(String::from_utf16_lossy(
        &buf[..buf
            .iter()
            .position(|&x| x == 0)
            .ok_or(eyre!("buf is not null-terminated"))?],
    ))
}
