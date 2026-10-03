//! Access control for the pipe and the data directory. Both expose every file name on the
//! machine, so they must not be readable by everyone.

use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use windows::core::HSTRING;
use windows::Win32::Foundation::{LocalFree, ERROR_ALREADY_EXISTS, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::CreateDirectoryW;

/// The pipe: SYSTEM and administrators get full access, interactively logged on users may
/// connect (read/write) to search.
pub const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

/// The data directory: only SYSTEM and administrators, inherited by the files in it.
pub const DATA_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// A security descriptor parsed from SDDL, ready to pass as `SECURITY_ATTRIBUTES`.
pub struct SecurityAttributes {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

unsafe impl Send for SecurityAttributes {}
unsafe impl Sync for SecurityAttributes {}

impl SecurityAttributes {
    pub fn from_sddl(sddl: &str) -> Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .with_context(|| format!("Invalid security descriptor {}", sddl))?;

        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            },
        })
    }

    pub fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl Drop for SecurityAttributes {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.descriptor.0)));
        }
    }
}

/// `%ProgramData%\Reverything`, created with restricted access if it does not exist.
pub fn data_dir() -> Result<PathBuf> {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let dir = base.join("Reverything");
    create_private_dir(&dir)?;
    Ok(dir)
}

fn create_private_dir(dir: &Path) -> Result<()> {
    let sa = SecurityAttributes::from_sddl(DATA_DIR_SDDL)?;
    match unsafe { CreateDirectoryW(&HSTRING::from(dir.as_os_str()), Some(sa.as_ptr())) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => Ok(()),
        Err(e) => Err(e).with_context(|| format!("Failed to create {}", dir.display())),
    }
}
