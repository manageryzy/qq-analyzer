use std::fs;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct WorkingCopy {
    pub method: &'static str,
    pub bytes: u64,
    pub fallback_reason: Option<String>,
}

pub fn create_working_copy(
    source: &Path,
    target: &Path,
    force: bool,
) -> anyhow::Result<WorkingCopy> {
    if target.exists() {
        if force {
            fs::remove_file(target)?;
        } else {
            let target_bytes = target.metadata()?.len();
            let source_bytes = source.metadata().ok().map(|meta| meta.len());
            return Ok(WorkingCopy {
                method: "existing",
                bytes: target_bytes,
                fallback_reason: source_bytes.and_then(|bytes| {
                    if bytes == target_bytes {
                        None
                    } else {
                        Some(format!(
                            "existing working copy reused; current source_size={bytes}"
                        ))
                    }
                }),
            });
        }
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    match block_clone_file(source, target) {
        Ok(bytes) => Ok(WorkingCopy {
            method: "block_clone",
            bytes,
            fallback_reason: None,
        }),
        Err(clone_err) => {
            ensure_copy_fallback_allowed(source, target)?;
            let fallback_reason = clone_err.to_string();
            copy_file(source, target)?;
            Ok(WorkingCopy {
                method: "copy_fallback",
                bytes: target.metadata()?.len(),
                fallback_reason: Some(fallback_reason),
            })
        }
    }
}

pub fn remove_working_copy(path: &Path) -> anyhow::Result<bool> {
    let mut removed = false;
    if path.exists() {
        fs::remove_file(path)?;
        removed = true;
    }
    for suffix in ["-journal", "-wal", "-shm"] {
        let sidecar = PathBufExt::with_appended_suffix(path, suffix);
        if sidecar.exists() {
            fs::remove_file(sidecar)?;
            removed = true;
        }
    }
    Ok(removed)
}

trait PathBufExt {
    fn with_appended_suffix(&self, suffix: &str) -> std::path::PathBuf;
}

impl PathBufExt for Path {
    fn with_appended_suffix(&self, suffix: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}{}", self.display(), suffix))
    }
}

fn copy_file(source: &Path, target: &Path) -> anyhow::Result<()> {
    let tmp = target.with_extension(format!(
        "{}.copying",
        target.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    fs::copy(source, &tmp)?;
    fs::rename(tmp, target)?;
    Ok(())
}

#[cfg(not(windows))]
fn ensure_copy_fallback_allowed(source: &Path, target: &Path) -> anyhow::Result<()> {
    if is_wsl_mount(source) || is_wsl_mount_target(target) {
        anyhow::bail!(
            "block clone is unavailable here and copy fallback was refused on a WSL /mnt path; run the Windows executable for large QQ database snapshots"
        );
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_copy_fallback_allowed(_source: &Path, _target: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn is_wsl_mount(path: &Path) -> bool {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    text.starts_with("/mnt/")
}

#[cfg(not(windows))]
fn is_wsl_mount_target(path: &Path) -> bool {
    if is_wsl_mount(path) {
        return true;
    }
    path.parent().is_some_and(is_wsl_mount)
}

#[cfg(not(windows))]
fn block_clone_file(_source: &Path, _target: &Path) -> anyhow::Result<u64> {
    anyhow::bail!("NTFS block clone requires the Windows build")
}

#[cfg(windows)]
fn block_clone_file(source: &Path, target: &Path) -> anyhow::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, SetEndOfFile, SetFilePointerEx, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
        FILE_BEGIN, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;

    #[repr(C)]
    struct DuplicateExtentsData {
        file_handle: HANDLE,
        source_file_offset: i64,
        target_file_offset: i64,
        byte_count: i64,
    }

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn last_error(context: &str) -> anyhow::Error {
        anyhow::anyhow!("{context}: Windows error {}", unsafe { GetLastError() })
    }

    let bytes = source.metadata()?.len();
    let source_w = wide(source);
    let target_w = wide(target);
    let source_handle = unsafe {
        CreateFileW(
            source_w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if source_handle == INVALID_HANDLE_VALUE {
        return Err(last_error("CreateFileW source"));
    }
    let source_handle = OwnedHandle(source_handle);

    let target_handle = unsafe {
        CreateFileW(
            target_w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            ptr::null_mut(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if target_handle == INVALID_HANDLE_VALUE {
        return Err(last_error("CreateFileW target"));
    }
    let target_handle = OwnedHandle(target_handle);

    if unsafe { SetFilePointerEx(target_handle.0, bytes as i64, ptr::null_mut(), FILE_BEGIN) } == 0
    {
        return Err(last_error("SetFilePointerEx target"));
    }
    if unsafe { SetEndOfFile(target_handle.0) } == 0 {
        return Err(last_error("SetEndOfFile target"));
    }

    let mut data = DuplicateExtentsData {
        file_handle: source_handle.0,
        source_file_offset: 0,
        target_file_offset: 0,
        byte_count: bytes as i64,
    };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            target_handle.0,
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            &mut data as *mut DuplicateExtentsData as *mut _,
            std::mem::size_of::<DuplicateExtentsData>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = last_error("FSCTL_DUPLICATE_EXTENTS_TO_FILE");
        let _ = fs::remove_file(target);
        return Err(err);
    }
    Ok(bytes)
}
