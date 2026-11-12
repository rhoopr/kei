//! Windows creation-time privacy and native write-through bundle publication.

use std::ffi::{CStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidA, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetVolumeInformationByHandleW, READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::FILE_PERSISTENT_ACLS;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::fs_util::{
    ConfinedParents, ConfinedPath, FileIdentity, file_identity, move_file_no_replace_blocking,
    windows_api_path,
};

/// Carries the original object, not a later pathname observation, into publication.
#[must_use = "Keep the original stage guard until checked publication completes"]
pub(super) struct Stage {
    confinement: ConfinedPath,
    // Data/list access participates in share checks; attribute-only ancestor
    // probes alone must not be relied on to exclude parent renames.
    _parent_directory: File,
    directory: File,
    identity: FileIdentity,
}

impl Stage {
    pub(super) fn publish(&mut self, published: &Path) -> io::Result<()> {
        let destination = self.confinement.sibling(published)?;
        // Open the rename-compatible handle while the original no-delete handle
        // still pins the populated stage name. This open requests no DELETE
        // access, so it does not conflict with the original sharing restriction.
        let rename_handle =
            directory(self.confinement.path(), FILE_SHARE_READ | FILE_SHARE_DELETE)?;
        if file_identity(&rename_handle)? != self.identity {
            return Err(io::Error::other(
                "Backup stage changed identity before publication; migration was refused. Preserve the stage and stop other directory writers before retrying",
            ));
        }
        // Retain the original object and every parent capability continuously.
        // Only this leaf's no-delete restriction must be released for our move.
        drop(std::mem::replace(&mut self.directory, rename_handle));
        #[cfg(test)]
        tests::after_handoff(self.confinement.path())?;
        move_file_no_replace_blocking(self.confinement.path(), destination.path())?;
        let output = directory(destination.path(), FILE_SHARE_READ)?;
        if file_identity(&output)? != self.identity {
            return Err(io::Error::other(
                "Published backup directory changed identity from the original populated stage; migration was refused. Preserve both backup paths and stop other directory writers before retrying",
            ));
        }
        // Pin the verified final name again after the move. The parent guard
        // remains owned by this Stage throughout all checks and error paths.
        self.directory = output;
        Ok(())
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: only LocalAlloc-owned API outputs enter this wrapper. Null
        // is permitted; every non-null allocation is freed exactly once.
        unsafe { LocalFree(self.0) };
    }
}

fn current_user_sid() -> io::Result<String> {
    // SAFETY: this returns a borrowed pseudo-handle; it is never closed.
    let process = unsafe { GetCurrentProcess() };
    let mut raw = 0;
    // SAFETY: process is valid; raw is writable output for a newly owned token.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call returned a newly owned, valid token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw as *mut c_void) };
    let mut size = 0;
    // SAFETY: null storage with length zero requests only the required length.
    unsafe { GetTokenInformation(raw, TokenUser, std::ptr::null_mut(), 0, &mut size) };
    let bytes = usize::try_from(size).map_err(io::Error::other)?;
    const MAX_TOKEN_USER_BYTES: usize = 4096;
    if bytes < size_of::<TOKEN_USER>() || bytes > MAX_TOKEN_USER_BYTES {
        return Err(io::Error::other(
            "Windows returned an invalid token-user size",
        ));
    }
    // usize storage supplies TOKEN_USER's pointer alignment, unlike Vec<u8>.
    let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    // SAFETY: the aligned buffer has at least size writable bytes. The token
    // remains live, and the returned TOKEN_USER and SID stay inside this buffer.
    if unsafe { GetTokenInformation(raw, TokenUser, storage.as_mut_ptr().cast(), size, &mut size) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser output initialized this aligned structure.
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut text = std::ptr::null_mut();
    // SAFETY: user.Sid points into the live token output; text receives one
    // LocalAlloc-owned, NUL-terminated ASCII SID string on success.
    if unsafe { ConvertSidToStringSidA(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let allocation = LocalAllocation(text.cast());
    // SAFETY: the successful conversion supplied a live NUL-terminated string.
    let sid = unsafe { CStr::from_ptr(allocation.0.cast()) }
        .to_str()
        .map_err(io::Error::other)?
        .to_owned();
    drop(token);
    Ok(sid)
}

fn descriptor(sddl: &str) -> io::Result<LocalAllocation> {
    let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut output = std::ptr::null_mut();
    // SAFETY: wide is NUL-terminated and live; output is writable pointer
    // storage. Ownership is assumed only after a successful API result.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut output,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(output))
}

fn private_descriptor() -> io::Result<LocalAllocation> {
    let sid = current_user_sid()?;
    // Protected DACL: no parent ACEs; only this actual process user receives
    // access. OI/CI also protect SQLite-created companions by inheritance.
    descriptor(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})"))
}

fn attributes(descriptor: &LocalAllocation) -> io::Result<SECURITY_ATTRIBUTES> {
    Ok(SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    })
}

fn directory(path: &Path, sharing: u32) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(sharing)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "Backup directory is not a non-reparse directory",
        ));
    }
    Ok(file)
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<Stage> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Backup directory has no parent"))?;
    let guard = ConfinedPath::open(parent, path, ConfinedParents::Existing)?;
    let parent_handle = directory(parent, FILE_SHARE_READ)?;
    let mut flags = 0;
    // SAFETY: parent_handle stays live and flags is valid writable output.
    // Null optional outputs have zero lengths, as required by the API.
    if unsafe {
        GetVolumeInformationByHandleW(
            parent_handle.as_raw_handle() as isize,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if flags & FILE_PERSISTENT_ACLS == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Backup storage cannot preserve private Windows ACLs; stop kei and use an ACL-capable state directory before retrying",
        ));
    }
    let security = private_descriptor()?;
    let attributes = attributes(&security)?;
    let wide = windows_api_path(path)?;
    // SAFETY: wide, attributes and its descriptor remain live for the call.
    // CreateDirectoryW fails if the name exists; no permissive create precedes it.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = directory(path, FILE_SHARE_READ)?;
    let identity = file_identity(&directory)?;
    Ok(Stage {
        confinement: guard,
        _parent_directory: parent_handle,
        directory,
        identity,
    })
}

pub(super) fn private_file(path: &Path) -> io::Result<File> {
    let security = private_descriptor()?;
    let attributes = attributes(&security)?;
    let wide = windows_api_path(path)?;
    // SAFETY: all input storage outlives the call; CREATE_NEW refuses existing
    // entries. A successful returned handle is uniquely transferred to File.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            0,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a newly owned, valid file handle from the successful call.
    Ok(unsafe { File::from_raw_handle(raw as *mut c_void) })
}

#[cfg(test)]
pub(super) mod tests;
