//! Shared filesystem primitives.

use std::path::{Path, PathBuf};

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileIdentity {
    Extended { volume: u64, index: [u8; 16] },
    Legacy { volume: u32, index: u64 },
}

pub(crate) fn file_identity(file: &std::fs::File) -> std::io::Result<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        };

        let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        // SAFETY: the file handle is live and `info` points to writable storage
        // of the exact FileIdInfo structure requested from Windows.
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as isize,
                FileIdInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if result != 0 {
            // SAFETY: the successful call initialized the complete structure.
            let info = unsafe { info.assume_init() };
            if info.FileId.Identifier != [0; 16] {
                return Ok(FileIdentity::Extended {
                    volume: info.VolumeSerialNumber,
                    index: info.FileId.Identifier,
                });
            }
        }

        let mut legacy = std::mem::MaybeUninit::uninit();
        // SAFETY: the file handle is live and `legacy` points to writable
        // storage of the exact structure initialized by Windows.
        let result = unsafe {
            GetFileInformationByHandle(file.as_raw_handle() as isize, legacy.as_mut_ptr())
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the successful call initialized the complete structure.
        let legacy = unsafe { legacy.assume_init() };
        let index = (u64::from(legacy.nFileIndexHigh) << 32) | u64::from(legacy.nFileIndexLow);
        if index == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "filesystem did not provide a stable file identity",
            ));
        }
        Ok(FileIdentity::Legacy {
            volume: legacy.dwVolumeSerialNumber,
            index,
        })
    }
}

#[cfg(unix)]
fn openat_owned(
    directory: std::os::fd::RawFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    // SAFETY: the directory descriptor and name remain live for the call. A
    // successful descriptor is newly owned.
    let raw = unsafe { libc::openat(directory, name.as_ptr(), flags, libc::c_uint::from(mode)) };
    if raw < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: `raw` is a newly owned descriptor returned by openat.
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
    }
}

#[cfg(unix)]
fn open_unix_directory(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;

    Ok(std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?
        .into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfinedParents {
    Existing,
    Create,
}

#[derive(Debug)]
pub(crate) struct ConfinedPath {
    #[cfg(unix)]
    root: PathBuf,
    path: PathBuf,
    #[cfg(unix)]
    parent_components: Vec<std::ffi::OsString>,
    #[cfg(unix)]
    parent_dir: std::os::fd::OwnedFd,
    #[cfg(unix)]
    name: std::ffi::CString,
    #[cfg(windows)]
    directory_handles: Vec<std::fs::File>,
    #[cfg(unix)]
    parent_identity: FileIdentity,
}

impl ConfinedPath {
    pub(crate) fn open(
        root: &Path,
        path: &Path,
        parents: ConfinedParents,
    ) -> std::io::Result<Self> {
        let root = absolute_lexical(root)?;
        let path = absolute_lexical(path)?;
        let relative = path.strip_prefix(&root).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is outside {}: {error}", path.display(), root.display()),
            )
        })?;
        let mut components = relative
            .components()
            .map(|component| match component {
                std::path::Component::Normal(value) => Ok(value.to_os_string()),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{} is not a confined descendant", path.display()),
                )),
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let name = components.pop().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} does not name a file beneath the root", path.display()),
            )
        })?;
        Self::open_platform(root, path, components, name, parents)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sibling(&self, path: &Path) -> std::io::Result<Self> {
        let path = absolute_lexical(path)?;
        if path.parent() != self.path.parent() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{} is not a sibling of {}",
                    path.display(),
                    self.path.display()
                ),
            ));
        }
        let name = path
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{} does not name a file", path.display()),
                )
            })?;
        #[cfg(windows)]
        let _ = &name;
        Ok(Self {
            #[cfg(unix)]
            root: self.root.clone(),
            path,
            #[cfg(unix)]
            parent_components: self.parent_components.clone(),
            #[cfg(unix)]
            parent_dir: self.parent_dir.try_clone()?,
            #[cfg(unix)]
            name: confined_cstring(&name)?,
            #[cfg(windows)]
            directory_handles: self
                .directory_handles
                .iter()
                .map(std::fs::File::try_clone)
                .collect::<std::io::Result<Vec<_>>>()?,
            #[cfg(unix)]
            parent_identity: self.parent_identity,
        })
    }

    pub(crate) fn open_regular(&self) -> std::io::Result<std::fs::File> {
        self.validate_namespace()?;
        self.open_regular_read()
    }

    pub(crate) fn open_optional_regular(&self) -> std::io::Result<Option<std::fs::File>> {
        match self.open_regular() {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn create_new_regular(&self) -> std::io::Result<std::fs::File> {
        self.validate_namespace()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            Ok(std::fs::File::from(openat_owned(
                self.parent_dir.as_raw_fd(),
                &self.name,
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666,
            )?))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&self.path)?;
            ensure_windows_regular(&file, &self.path)?;
            Ok(file)
        }
    }

    pub(crate) fn validate_identity(
        &self,
        expected: FileIdentity,
    ) -> std::io::Result<std::fs::File> {
        self.validate_namespace()?;
        let file = self.open_regular_read()?;
        if file_identity(&file)? != expected {
            return Err(confined_identity_changed_error(&self.path));
        }
        Ok(file)
    }

    pub(crate) fn entry_exists(&self) -> std::io::Result<bool> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: the retained parent descriptor and NUL-terminated name
            // remain live, and `stat` is valid writable output storage.
            let result = unsafe {
                libc::fstatat(
                    self.parent_dir.as_raw_fd(),
                    self.name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(error)
            }
        }
        #[cfg(windows)]
        {
            match open_windows_entry_attributes(&self.path) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    pub(crate) fn sync_parent(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let file = std::fs::File::from(self.parent_dir.try_clone()?);
            file.sync_all()
        }
        #[cfg(windows)]
        {
            Ok(())
        }
    }

    #[cfg(unix)]
    pub(crate) fn parent_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;

        self.parent_dir.as_raw_fd()
    }

    #[cfg(unix)]
    pub(crate) fn name_cstr(&self) -> &std::ffi::CStr {
        &self.name
    }

    fn open_regular_read(&self) -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
            let file = std::fs::File::from(
                openat_owned(self.parent_dir.as_raw_fd(), &self.name, flags, 0).map_err(
                    |error| {
                        if error.raw_os_error() == Some(libc::ELOOP) {
                            non_regular_error(&self.path)
                        } else {
                            error
                        }
                    },
                )?,
            );
            if !file.metadata()?.file_type().is_file() {
                return Err(non_regular_error(&self.path));
            }
            Ok(file)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_READ_ATTRIBUTES,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };

            let probe = open_windows_entry_attributes(&self.path)?;
            ensure_windows_regular(&probe, &self.path)?;
            drop(probe);
            let file = std::fs::OpenOptions::new()
                .access_mode(FILE_GENERIC_READ | FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&self.path)?;
            ensure_windows_regular(&file, &self.path)?;
            Ok(file)
        }
    }

    fn validate_namespace(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let mut current = open_unix_directory(&self.root)?;
            for component in &self.parent_components {
                let component = confined_cstring(component)?;
                current = openat_owned(
                    current.as_raw_fd(),
                    &component,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )?;
            }
            let current_file = std::fs::File::from(current.try_clone()?);
            if file_identity(&current_file)? != self.parent_identity {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} changed identity", self.path.display()),
                ));
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            Ok(())
        }
    }

    #[cfg(unix)]
    fn open_platform(
        root: PathBuf,
        path: PathBuf,
        parent_components: Vec<std::ffi::OsString>,
        name: std::ffi::OsString,
        parents: ConfinedParents,
    ) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;

        let mut parent_dir = open_unix_directory(&root)?;
        for component in &parent_components {
            let component = confined_cstring(component)?;
            parent_dir = match openat_owned(
                parent_dir.as_raw_fd(),
                &component,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            ) {
                Ok(directory) => directory,
                Err(error)
                    if parents == ConfinedParents::Create
                        && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    // SAFETY: the retained parent and component remain live.
                    if unsafe { libc::mkdirat(parent_dir.as_raw_fd(), component.as_ptr(), 0o777) }
                        != 0
                    {
                        let create_error = std::io::Error::last_os_error();
                        if create_error.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(create_error);
                        }
                    } else {
                        let parent_file = std::fs::File::from(parent_dir.try_clone()?);
                        parent_file.sync_all()?;
                    }
                    openat_owned(
                        parent_dir.as_raw_fd(),
                        &component,
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        0,
                    )?
                }
                Err(error) => return Err(error),
            };
        }
        let parent_file = std::fs::File::from(parent_dir.try_clone()?);
        let parent_identity = file_identity(&parent_file)?;
        Ok(Self {
            root,
            path,
            parent_components,
            parent_dir,
            name: confined_cstring(&name)?,
            parent_identity,
        })
    }

    #[cfg(windows)]
    fn open_platform(
        root: PathBuf,
        path: PathBuf,
        parent_components: Vec<std::ffi::OsString>,
        _name: std::ffi::OsString,
        parents: ConfinedParents,
    ) -> std::io::Result<Self> {
        let mut directory_handles = open_windows_directory_chain(&root)?;
        let mut parent_path = root.clone();
        for component in &parent_components {
            parent_path.push(component);
            if parents == ConfinedParents::Create {
                match std::fs::create_dir(&parent_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            directory_handles.push(open_windows_directory(&parent_path)?);
        }
        Ok(Self {
            path,
            directory_handles,
        })
    }
}

fn confined_identity_changed_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{} changed identity", path.display()),
    )
}

fn non_regular_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{} is not a regular file", path.display()),
    )
}

#[cfg(unix)]
fn confined_cstring(value: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(value.as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[cfg(windows)]
fn open_windows_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let attributes = windows_file_attributes(&file)?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} is not a non-reparse directory", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_directory_chain(path: &Path) -> std::io::Result<Vec<std::fs::File>> {
    let path = absolute_lexical(path)?;
    let mut current = PathBuf::new();
    let mut handles = Vec::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        handles.push(open_windows_directory(&current)?);
    }
    if handles.is_empty() {
        handles.push(open_windows_directory(&path)?);
    }
    Ok(handles)
}

#[cfg(windows)]
fn open_windows_entry_attributes(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(windows)]
fn windows_file_attributes(file: &std::fs::File) -> std::io::Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut info = std::mem::MaybeUninit::uninit();
    // SAFETY: the handle is live and `info` points to writable storage of the
    // exact structure initialized by GetFileInformationByHandle.
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as isize, info.as_mut_ptr()) };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized the complete structure.
    Ok(unsafe { info.assume_init() }.dwFileAttributes)
}

#[cfg(windows)]
fn ensure_windows_regular(file: &std::fs::File, path: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let attributes = windows_file_attributes(file)?;
    if attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || !file.metadata()?.file_type().is_file()
    {
        return Err(non_regular_error(path));
    }
    Ok(())
}

pub(crate) enum ConfinedFileOpen {
    Regular(ConfinedRegularFile),
    Retire,
    OutsideRoot,
}

pub(crate) struct ConfinedRegularFile {
    modified_secs: i64,
    #[cfg(unix)]
    parent: std::os::fd::OwnedFd,
    #[cfg(unix)]
    name: std::ffi::CString,
    #[cfg(windows)]
    file: std::fs::File,
}

impl ConfinedRegularFile {
    pub(crate) fn modified_secs(&self) -> i64 {
        self.modified_secs
    }

    pub(crate) fn remove(self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            // SAFETY: `parent` is a live directory descriptor, `name` is
            // NUL-terminated, and both remain valid for the syscall.
            let result = unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0) };
            if result == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
            };

            let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
            // SAFETY: the file handle is live and was opened with DELETE
            // access. `disposition` has the layout and size required by
            // FileDispositionInfo and outlives the call.
            let result = unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle() as isize,
                    FileDispositionInfo,
                    std::ptr::from_ref(&disposition).cast(),
                    std::mem::size_of_val(&disposition) as u32,
                )
            };
            if result != 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
    }
}

/// Open a regular file beneath `root` without allowing a directory-link swap
/// to redirect a later removal. The returned guard retains the verified
/// directory or file handle through [`ConfinedRegularFile::remove`].
pub(crate) fn open_confined_regular_file(
    root: &Path,
    path: &Path,
) -> std::io::Result<ConfinedFileOpen> {
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(ConfinedFileOpen::OutsideRoot);
    };
    let components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    let Some(components) = components else {
        return Ok(ConfinedFileOpen::Retire);
    };
    if components.is_empty() {
        return Ok(ConfinedFileOpen::Retire);
    }

    open_confined_regular_file_platform(root, &components)
}

#[cfg(unix)]
fn open_confined_regular_file_platform(
    root: &Path,
    components: &[&std::ffi::OsStr],
) -> std::io::Result<ConfinedFileOpen> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    fn owned_fd(raw: libc::c_int) -> std::io::Result<OwnedFd> {
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a nonnegative descriptor returned by open/openat is newly
        // owned by this function and is transferred exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    fn path_target_changed(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) || error.raw_os_error() == Some(libc::ELOOP)
    }

    fn cstring(value: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
        std::ffi::CString::new(value.as_bytes())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
    }

    let root = cstring(root.as_os_str())?;
    // SAFETY: `root` is a live NUL-terminated path. No borrowed pointer is
    // retained after the syscall.
    let raw = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    let mut parent = match owned_fd(raw) {
        Ok(fd) => fd,
        Err(error) if path_target_changed(&error) => return Ok(ConfinedFileOpen::Retire),
        Err(error) => return Err(error),
    };

    let Some((file_name, parent_components)) = components.split_last() else {
        return Ok(ConfinedFileOpen::Retire);
    };
    for component in parent_components {
        let component = cstring(component)?;
        // SAFETY: `parent` is live and `component` is NUL-terminated. The
        // returned descriptor, when successful, is newly owned.
        let raw = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        parent = match owned_fd(raw) {
            Ok(fd) => fd,
            Err(error) if path_target_changed(&error) => return Ok(ConfinedFileOpen::Retire),
            Err(error) => return Err(error),
        };
    }

    let name = cstring(file_name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `parent` and `name` are live, and `stat` points to writable
    // storage of the exact type the kernel initializes on success.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return if path_target_changed(&error) {
            Ok(ConfinedFileOpen::Retire)
        } else {
            Err(error)
        };
    }
    // SAFETY: successful fstatat initialized every field of `stat`.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Ok(ConfinedFileOpen::Retire);
    }

    Ok(ConfinedFileOpen::Regular(ConfinedRegularFile {
        modified_secs: stat.st_mtime.max(0),
        parent,
        name,
    }))
}

#[cfg(windows)]
fn open_confined_regular_file_platform(
    root: &Path,
    components: &[&std::ffi::OsStr],
) -> std::io::Result<ConfinedFileOpen> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
    };

    fn open(path: &Path, directory: bool) -> std::io::Result<std::fs::File> {
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let access = FILE_READ_ATTRIBUTES | if directory { 0 } else { DELETE };
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
        std::fs::OpenOptions::new()
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(flags)
            .open(path)
    }

    fn info(file: &std::fs::File) -> std::io::Result<u32> {
        let mut info = std::mem::MaybeUninit::uninit();
        // SAFETY: the handle is live and `info` points to writable storage of
        // the exact structure initialized by GetFileInformationByHandle.
        let result =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as isize, info.as_mut_ptr()) };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the successful call initialized the full structure.
        Ok(unsafe { info.assume_init() }.dwFileAttributes)
    }

    fn final_path(file: &std::fs::File) -> std::io::Result<std::path::PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

        let mut buffer = vec![0_u16; 512];
        loop {
            // SAFETY: the handle and buffer are live, and the buffer length
            // matches the writable UTF-16 storage supplied to Windows.
            let written = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle() as isize,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    0,
                )
            } as usize;
            if written == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if written < buffer.len() {
                buffer.truncate(written);
                return Ok(std::ffi::OsString::from_wide(&buffer).into());
            }
            buffer.resize(written + 1, 0);
        }
    }

    let root_file = match open(root, true) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfinedFileOpen::Retire);
        }
        Err(error) => return Err(error),
    };
    let root_attributes = info(&root_file)?;
    if root_attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || root_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Ok(ConfinedFileOpen::Retire);
    }
    let root_final = final_path(&root_file)?;
    let path = components
        .iter()
        .fold(root.to_path_buf(), |mut path, part| {
            path.push(part);
            path
        });
    let file = match open(&path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfinedFileOpen::Retire);
        }
        Err(error) => return Err(error),
    };
    let attributes = info(&file)?;
    if attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Ok(ConfinedFileOpen::Retire);
    }
    let file_final = final_path(&file)?;
    let expected_final = components.iter().fold(root_final, |mut path, part| {
        path.push(part);
        path
    });
    if file_final != expected_final {
        return Ok(ConfinedFileOpen::Retire);
    }
    let modified_secs = file
        .metadata()?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    Ok(ConfinedFileOpen::Regular(ConfinedRegularFile {
        modified_secs,
        file,
    }))
}

/// Resolve `path` against the current directory and remove lexical `.` and
/// `..` components without following filesystem symlinks.
pub(crate) fn absolute_lexical(path: &Path) -> std::io::Result<std::path::PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Remove `path`, treating `NotFound` as success and logging any other
/// error at `warn!`. Use this in cleanup paths (`.part` cleanup, corrupt
/// session-file deletion, EXDEV unwind) where a previous `let _ =` was
/// silently dropping errors that violated the "no silent failures"
/// invariant.
///
/// Used by both the default XMP writer and the native no-`xmp` EXIF writer.
/// The async sibling `log_remove_async` is available for callers already on a
/// tokio task.
pub(crate) fn log_remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "Failed to remove file during cleanup"
        );
    }
}

/// Async sibling of [`log_remove`] for callers already on a tokio task;
/// uses `tokio::fs::remove_file` so it doesn't block a runtime worker.
pub(crate) async fn log_remove_async(path: &Path) {
    if let Err(e) = tokio::fs::remove_file(path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "Failed to remove file during cleanup"
        );
    }
}

/// Open `path`'s parent directory and `fsync` it so a preceding `rename`'s
/// directory entry survives a power loss. Unix-only; on Windows this is a
/// no-op because the std API doesn't expose a directory handle for fsync.
///
/// Errors from the open or sync are returned to the caller. Callers that
/// want best-effort durability without bubbling the error should log and
/// drop it themselves.
pub(crate) fn fsync_parent_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or(Path::new("."));
        let dir = std::fs::File::open(parent)?;
        dir.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Async wrapper around [`fsync_parent_dir`] that runs the blocking
/// syscall on the blocking pool and swallows every error class with a
/// warn. Use when the caller has already committed to the rename
/// being durable enough on its own (the bytes are at `path`, the
/// metadata flush is best-effort).
pub(crate) async fn fsync_parent_dir_async_best_effort(path: &Path) {
    let path_buf = path.to_path_buf();
    match tokio::task::spawn_blocking(move || fsync_parent_dir(&path_buf)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "parent-dir fsync failed; durability of rename not guaranteed"
        ),
        Err(join_err) => tracing::warn!(
            path = %path.display(),
            error = %join_err,
            "parent-dir fsync task panicked; durability of rename not guaranteed"
        ),
    }
}

/// Install `src` at `dst` atomically.
///
/// Prefers `rename` (atomic on the same device); on EXDEV, copies to a
/// sibling of `dst` on the destination device and renames that sibling
/// into place so a mid-copy crash can't expose a half-written `dst`.
///
/// `src`'s data is fsynced before the rename and `dst`'s parent directory
/// is fsynced after, so a power loss between the rename returning and the
/// kernel committing data + directory blocks can't leave `dst` pointing
/// at an uninitialised file or vanish on the next mount.
pub(crate) fn atomic_install(src: &Path, dst: &Path) -> std::io::Result<()> {
    atomic_install_with(src, dst, |s, d| std::fs::rename(s, d))
}

/// fsync `path` if it exists. Treats NotFound as a no-op so callers don't
/// have to special-case the EXDEV path (where the original src was already
/// consumed by a copy).
///
/// Unix-only. On Windows `std::fs::File::open` returns a read-only handle
/// and `FlushFileBuffers` requires write access, so the natural
/// implementation here returns `ERROR_ACCESS_DENIED`. NTFS journals
/// metadata anyway, so the data-blocks-vs-rename ordering risk this
/// guards on Linux doesn't manifest the same way; treat the Windows path
/// as a no-op rather than carry a fragile reopen-with-write workaround.
fn fsync_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        match std::fs::File::open(path) {
            Ok(f) => f.sync_all(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Test hook: like [`atomic_install`] but accepts an injectable `rename` so
/// tests can force the EXDEV fallback without needing a real cross-device
/// setup. Only the initial `src -> dst` rename is injected; the fallback's
/// `sibling -> dst` rename is plain `std::fs::rename` (same-device, can't
/// fail with EXDEV).
fn atomic_install_with<R>(src: &Path, dst: &Path, rename: R) -> std::io::Result<()>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    fsync_file(src)?;
    if let Err(rename_err) = rename(src, dst) {
        let ext = dst.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
        let dst_sibling = dst.with_extension(format!("{ext}.kei-xdev-tmp-{}", std::process::id()));
        if let Err(copy_err) = std::fs::copy(src, &dst_sibling) {
            let _ = std::fs::remove_file(src);
            tracing::warn!(
                src = %src.display(),
                dst = %dst.display(),
                rename_err = %rename_err,
                copy_err = %copy_err,
                "rename failed and cross-device copy also failed"
            );
            return Err(rename_err);
        }
        // Fsync the sibling we just copied before renaming it into place.
        fsync_file(&dst_sibling)?;
        if let Err(final_err) = std::fs::rename(&dst_sibling, dst) {
            let _ = std::fs::remove_file(&dst_sibling);
            let _ = std::fs::remove_file(src);
            return Err(final_err);
        }
        let _ = std::fs::remove_file(src);
    }
    if let Err(e) = fsync_parent_dir(dst) {
        tracing::warn!(
            path = %dst.display(),
            error = %e,
            "fsync of parent directory failed after atomic_install"
        );
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn confined_path_rejects_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let destination = outside.path().join("nested/file.jpg");

        let error =
            ConfinedPath::open(root.path(), &destination, ConfinedParents::Create).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!destination.exists());
    }

    #[test]
    fn confined_path_creates_missing_nested_parents() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("one/two/three/file.jpg");
        let confined =
            ConfinedPath::open(root.path(), &destination, ConfinedParents::Create).unwrap();
        let mut file = confined.create_new_regular().unwrap();
        std::io::Write::write_all(&mut file, b"media").unwrap();
        file.sync_all().unwrap();
        let identity = file_identity(&file).unwrap();

        confined.validate_identity(identity).unwrap();
        confined.sync_parent().unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"media");
    }

    #[cfg(unix)]
    #[test]
    fn confined_path_rejects_linked_parent_without_external_write() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked = root.path().join("linked");
        symlink(outside.path(), &linked).unwrap();

        assert!(
            ConfinedPath::open(
                root.path(),
                &linked.join("file.jpg"),
                ConfinedParents::Create
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn confined_path_uses_retained_parent_and_detects_replacement() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let retained = root.path().join("retained");
        std::fs::create_dir(&parent).unwrap();
        let destination = parent.join("file.jpg");
        let confined =
            ConfinedPath::open(root.path(), &destination, ConfinedParents::Existing).unwrap();

        std::fs::rename(&parent, &retained).unwrap();
        symlink(outside.path(), &parent).unwrap();
        assert!(confined.create_new_regular().is_err());
        assert!(!retained.join("file.jpg").exists());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn same_device_rename_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.json");
        std::fs::write(&src, b"hello").unwrap();

        atomic_install(&src, &dst).expect("atomic_install");

        assert!(!src.exists(), "src must be consumed by the rename");
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");

        for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains("kei-xdev-tmp"),
                "unexpected sidecar tmp {name}"
            );
        }
    }

    #[test]
    fn missing_src_returns_err_without_touching_dst() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("nope.tmp");
        let dst = dir.path().join("dst.json");

        assert!(atomic_install(&src, &dst).is_err());
        assert!(!dst.exists(), "dst must not be created on failure");
    }

    /// Forces the rename to fail with a cross-device error, exercising the
    /// copy-to-sibling-then-rename fallback end-to-end. After the fallback,
    /// `dst` must contain the source bytes, `src` is removed, and no
    /// `.kei-xdev-tmp-*` file remains.
    #[test]
    fn exdev_fallback_installs_dst_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.json");
        std::fs::write(&src, b"xdev-payload").unwrap();

        let force_exdev = |_s: &Path, _d: &Path| -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                "simulated EXDEV",
            ))
        };

        atomic_install_with(&src, &dst, force_exdev).expect("EXDEV fallback should succeed");

        assert!(
            !src.exists(),
            "src must be removed after successful fallback"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"xdev-payload");

        for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains("kei-xdev-tmp"),
                "EXDEV fallback must clean up its sibling tmp: {name}"
            );
        }
    }

    /// `fsync_parent_dir` returns `Ok(())` for an
    /// extant directory on every supported platform. On Unix it actually
    /// opens and fsyncs the parent; on Windows it's a documented no-op. The
    /// test pins both platforms to "doesn't error" so a future regression
    /// that drops the cfg gate or changes the open mode surfaces here.
    #[test]
    fn fsync_parent_dir_succeeds_for_extant_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("anchor.txt");
        std::fs::write(&file, b"x").unwrap();
        fsync_parent_dir(&file).expect("fsync_parent_dir should succeed");
    }

    /// `fsync_parent_dir` on Unix surfaces a NotFound when the parent itself
    /// is missing; on other platforms it's a no-op and returns Ok. Pinning
    /// the Unix branch makes accidental swallowing of the error visible.
    #[cfg(unix)]
    #[test]
    fn fsync_parent_dir_unix_errors_when_parent_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nope/sub/file.txt");
        let err = fsync_parent_dir(&file).expect_err("missing parent should error on unix");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// Happy-path coverage: a same-device install of a freshly written
    /// src succeeds end-to-end with the new fsync calls in the chain.
    /// Regression guard if a future refactor drops the fsync of src or the
    /// parent fsync and breaks the call (e.g. by accidentally borrowing a
    /// closed File past sync_all).
    #[test]
    fn atomic_install_round_trip_fsyncs_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.json");
        let payload = b"durable payload";
        std::fs::write(&src, payload).unwrap();

        atomic_install(&src, &dst).expect("atomic_install with fsync should succeed");

        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), payload);
    }

    /// If the initial rename fails and the cross-device copy also fails
    /// (e.g. dst parent is read-only), `src` is removed and the original
    /// rename error is returned; `dst` is never created.
    #[test]
    fn exdev_fallback_with_copy_failure_surfaces_rename_err() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let nonexistent_parent = dir.path().join("no_such_dir");
        let dst = nonexistent_parent.join("dst.json");
        std::fs::write(&src, b"payload").unwrap();

        let force_exdev = |_s: &Path, _d: &Path| -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::CrossesDevices,
                "simulated EXDEV",
            ))
        };

        let err = atomic_install_with(&src, &dst, force_exdev).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::CrossesDevices);
        assert!(!dst.exists(), "dst must not be created when fallback fails");
        assert!(
            !src.exists(),
            "src must be cleaned up even when fallback fails"
        );
    }
}
