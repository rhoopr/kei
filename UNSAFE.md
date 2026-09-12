# Unsafe usage audit

Audit date: 2026-08-24.

This file lists each `unsafe` block or expression in repo-owned production
Rust source. Test modules, `tests/`, plain-text mentions of "unsafe", Cargo
lint settings, and dependency lockfile entries are not counted. There are no
`unsafe fn`, `unsafe impl`, or raw `extern` declarations in production code.

## Production code

| Location | Usage | Justification | Removal assessment |
| --- | --- | --- | --- |
| `src/lib.rs::harden_process` | `libc::prctl(PR_SET_DUMPABLE, 0, 0, 0, 0)` in `harden_process`. | Disables Linux core dumps before secrets can leak into a dump. The call takes integer arguments only, so the local memory-safety risk is limited to correct FFI signature and constants. Failure is logged and ignored. | No safe `std` equivalent. It can be moved out of repo-local `unsafe` by adding a direct `rustix` dependency with the process APIs and calling `rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)`. Dropping the call would weaken credential hardening. |
| `src/lib.rs::harden_process` | `libc::setrlimit(RLIMIT_CORE, &rlim)` in `harden_process`. | Sets the core-file size limit to zero on Unix. The `rlimit` value is stack-allocated, initialized before the call, and the kernel only reads it during the syscall. Failure is logged and ignored. | No safe `std` equivalent. A direct `rustix` process dependency can replace this with `setrlimit(Resource::Core, Rlimit { current: 0, maximum: 0 })`. Dropping it would weaken the same hardening path. |
| `src/lib.rs::available_disk_space` (Unix) | `std::mem::zeroed::<libc::statvfs>()` in `available_disk_space`. | Creates an output buffer for `statvfs`. The struct is integer fields on supported Unix targets, and the following syscall overwrites it on success. | Removable. `fs4` is already a direct dependency and exposes safe `fs4::available_space(path)`, which would remove both this initialization and the raw `statvfs` call below. |
| `src/lib.rs::available_disk_space` (Unix) | `libc::statvfs(c_path.as_ptr(), &raw mut stat)` in `available_disk_space`. | Reads filesystem free-space data through a NUL-terminated path and a valid output pointer that outlives the call. | Removable with the same `fs4::available_space(path).ok()` rewrite as `src/lib.rs::available_disk_space` (Unix). |
| `src/lib.rs::available_disk_space` (Windows) | `GetDiskFreeSpaceExW(...)` in `available_disk_space`. | Reads filesystem free-space data through a NUL-terminated UTF-16 path and a valid output pointer. The API permits null pointers for the unused totals. | Removable with the same `fs4::available_space(path).ok()` rewrite. |
| `src/lib.rs::pid_is_alive` | `libc::kill(pid, 0)` in `pid_is_alive`. | Uses POSIX signal `0` as a process-existence probe. It does not deliver a signal; `EPERM` is treated as alive. | No safe `std` equivalent. A direct `rustix` process dependency can replace it with `test_kill_process`, preserving the `PERM` means alive case. The existing unsafe block is small and well-contained. |
| `src/lib.rs::main_inner` | `std::env::remove_var("ICLOUD_PASSWORD")` in `main_inner`. | Scrubs the password environment variable before the Tokio runtime creates worker threads. This is the narrow window where process environment mutation is safe under Rust's current rules. | Hard to remove while preserving the scrub. Safe `std` has no thread-safe process-env mutation API. Options are to stop scrubbing, move all work into a child process launched with a cleaned environment, or keep a tiny documented unsafe wrapper at startup. |
| `src/download/file.rs::renameat2_blocking` | `libc::syscall(SYS_renameat2, ...)` with `RENAME_NOREPLACE` or `RENAME_EXCHANGE`. | Promotes a verified `.part` file without overwrite, or atomically exchanges it with an explicitly authorized truncated file. The paths are owned `CString`s and no Rust references are retained by the kernel. | No safe `std` equivalent for either Linux rename mode. A direct `rustix` fs dependency can replace this with `renameat_with` and matching flags. Do not rewrite either path as an existence check followed by `rename`; that would reintroduce a race. |
| `src/download/file.rs::rename_exchange_blocking` (macOS) | `libc::renamex_np(..., RENAME_SWAP)` in `rename_exchange_blocking`. | Atomically exchanges a verified `.part` file with the exact truncated file approved by repair planning. Both paths are owned `CString`s and remain valid for the call. | No safe `std` equivalent for an atomic path exchange. A direct safe wrapper could move the FFI out of kei. A sequence of ordinary renames would add a crash window. |
| `src/download/file.rs::move_file_no_replace_blocking` (Windows) | Windows `MoveFileExW(..., MOVEFILE_WRITE_THROUGH)` in `move_file_no_replace_blocking`. | Promotes the `.part` file on Windows with no-overwrite semantics and write-through intent. The UTF-16 paths are NUL-terminated and live for the call. | Not cleanly removable with `std` while preserving write-through behavior. `std::fs::rename` would hide this Windows-specific durability choice. A safe wrapper crate could move the unsafe out of kei, but the platform call is still needed for the current semantics. |
| `src/download/file.rs::replace_file_with_backup_blocking` (Windows) | Windows `ReplaceFileW(..., REPLACEFILE_WRITE_THROUGH)` in `replace_file_with_backup_blocking`. | Atomically replaces an explicitly authorized truncated file and moves its old bytes to a distinct backup path for post-exchange verification or restoration. Every UTF-16 path is NUL-terminated and valid for the call. | No safe `std` equivalent provides atomic replacement with a backup. A safe Windows wrapper could move the FFI out of kei, but ordinary remove and rename calls would weaken crash and race safety. |
| `src/fs_util.rs::ConfinedRegularFile::remove` (Unix) | `libc::unlinkat(...)` removes the verified name relative to the retained parent directory descriptor. | The descriptor and NUL-terminated name remain live for the call. Descriptor-relative removal prevents an ancestor rename and symlink swap from redirecting deletion outside the download root. | No safe `std` equivalent removes a name relative to an open directory. A direct `rustix` dependency could provide a safe wrapper. Replacing this with `std::fs::remove_file` would restore the race. |
| `src/fs_util.rs::ConfinedRegularFile::remove` (Windows) | `SetFileInformationByHandle(..., FileDispositionInfo, ...)` marks the verified file handle for deletion. | The handle was opened with delete access and remains live. The disposition value has the required layout and size. Deletion stays bound to the opened file if an ancestor changes. | No safe `std` API deletes by an existing file handle. A safe Windows wrapper could move the FFI out of kei. Path-based removal would restore the race. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Unix) | `OwnedFd::from_raw_fd(...)` takes ownership of each successful `open` or `openat` result. | A nonnegative descriptor from the kernel is newly owned and is transferred exactly once. `OwnedFd` closes it on every return path. | A safe descriptor-relative filesystem crate could own this conversion. `std` does not expose `openat`. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Unix) | `libc::open(..., O_DIRECTORY | O_NOFOLLOW)` opens the configured root. | The path is NUL-terminated and remains live for the call. The flags require a real directory and reject a final symlink. | A direct `rustix` dependency could provide a safe wrapper. Opening by ordinary path later would not protect removal. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Unix) | `libc::openat(..., O_DIRECTORY | O_NOFOLLOW)` opens each descendant directory from the retained parent descriptor. | Both the descriptor and NUL-terminated component remain live. Each successful handle fixes the next lookup beneath the verified directory. | A direct `rustix` dependency could provide a safe wrapper. Rechecking with `symlink_metadata` would leave a race before deletion. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Unix) | `libc::fstatat(..., AT_SYMLINK_NOFOLLOW)` inspects the final directory entry. | The parent descriptor, NUL-terminated name, and output storage remain valid. The no-follow flag inspects a final symlink instead of its target. | A direct `rustix` dependency could provide a safe wrapper. `std` cannot inspect relative to an open directory. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Unix) | `MaybeUninit::assume_init()` reads the successful `fstatat` result. | The value is read only after `fstatat` returns success, which initializes the complete `libc::stat` structure. | A safe syscall wrapper would return an initialized value and remove this expression. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Windows) | `GetFileInformationByHandle(...)` reads attributes for the opened root and file. | The handle remains live and the output pointer references storage with the exact Windows structure layout. | A safe Windows wrapper could return the initialized handle information. `std` does not expose reparse attributes for these handles. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Windows) | `MaybeUninit::assume_init()` reads the successful handle-information result. | The value is read only after `GetFileInformationByHandle` returns success and initializes the full structure. | A safe Windows wrapper would return an initialized value and remove this expression. |
| `src/fs_util.rs::open_confined_regular_file_platform` (Windows) | `GetFinalPathNameByHandleW(...)` resolves each retained handle for an exact destination comparison. | The handle and UTF-16 output buffer remain live. The supplied length matches the writable buffer. The call repeats if the buffer is too small. | A safe Windows wrapper could return the final path. A lexical path check alone would not detect an ancestor reparse-point swap. |
| `src/download/metadata.rs::exif_datetime_to_iso` | `String::as_bytes_mut()` in `exif_datetime_to_iso`. | Mutates three delimiter bytes in an owned `String`. The length and delimiter checks prove the byte positions exist, and the replacement bytes are ASCII, so UTF-8 stays valid. | Removable. Build a `Vec<u8>` from `s.as_bytes()`, mutate the vector with safe indexing after the same length checks, and convert with `String::from_utf8`. A formatting-based rewrite would also work. |
| `src/personality/tty_echo.rs::EchoGuard::install` | `std::mem::zeroed::<termios>()` before `tcgetattr` in `EchoGuard::install`. | Prepares a `termios` output buffer. `tcgetattr` fills it before fields are read. | Removable by switching the module to safe termios bindings, for example a direct `rustix` dependency with the `termios` feature. That would also remove the related `tcgetattr` and `tcsetattr` unsafe calls. |
| `src/personality/tty_echo.rs::EchoGuard::install` | `tcgetattr(STDIN_FILENO, &mut t)` in `EchoGuard::install`. | Reads terminal flags from stdin into a valid `termios` pointer. Failure returns `None`. | Removable with safe termios bindings such as `rustix::termios::tcgetattr(std::io::stdin())`. |
| `src/personality/tty_echo.rs::EchoGuard::install` | `tcsetattr(STDIN_FILENO, TCSANOW, &t)` in `EchoGuard::install`. | Writes the modified terminal flags back to stdin. The input pointer is valid for the call. | Removable with safe termios bindings such as `rustix::termios::tcsetattr`. |
| `src/personality/tty_echo.rs::EchoGuard::restore_now` | `std::mem::zeroed::<termios>()` before restore-time `tcgetattr`. | Prepares a `termios` output buffer during terminal restore. | Removable with the same safe termios rewrite as `src/personality/tty_echo.rs::EchoGuard::install`. |
| `src/personality/tty_echo.rs::EchoGuard::restore_now` | `tcgetattr(STDIN_FILENO, &mut t)` in `restore_now`. | Reads the current terminal flags so only the saved local flags are restored. Failure is ignored because shutdown recovery is best-effort. | Removable with the same safe termios rewrite as `src/personality/tty_echo.rs::EchoGuard::install`. |
| `src/personality/tty_echo.rs::EchoGuard::restore_now` | `tcsetattr(STDIN_FILENO, TCSANOW, &t)` in `restore_now`. | Restores the saved terminal flags. Failure is ignored because the next shell prompt normally resets tty state. | Removable with the same safe termios rewrite as `src/personality/tty_echo.rs::EchoGuard::install`. |
| `src/service/env.rs::effective_uid` | `libc::geteuid()` in `effective_uid`. | Centralizes effective-UID lookup for Unix service backends. `geteuid` is stateless and has no pointer or aliasing preconditions. | Removable by adding a direct safe wrapper dependency, for example `rustix::process::geteuid` or `nix::unistd::geteuid`. This is a good cleanup target because tests duplicate this same unsafe call. |

## Reconciliation leaf checks

- `src/fs_util.rs::file_identity` (Windows): handle-information calls
  write into correctly sized storage. The live handle outlasts each call;
  `assume_init` runs only after success. The result pins comparisons to the
  opened file. Safe `std` has no equivalent stable Windows file-ID API.
- `src/download/file.rs::rename_confined_macos` (macOS):
  `renameatx_np` receives retained descriptors, NUL-terminated names, and
  `RENAME_EXCL`. The kernel
  rejects an existing destination atomically. An existence check followed by
  ordinary rename would not preserve no-overwrite publication.

## Reconciliation ancestor checks

- `src/fs_util.rs::openat_owned` transfers each successful syscall descriptor
  into one `OwnedFd`. The retained directory and NUL-terminated name remain
  live; the variadic mode has the promoted C integer type required on macOS.
- `ConfinedPath::open_platform` uses `mkdirat` beneath retained directories,
  then opens each component with `O_DIRECTORY | O_NOFOLLOW`. Every created
  directory's parent is synced before traversal continues.
- `ConfinedPath::entry_exists` uses `fstatat` with `AT_SYMLINK_NOFOLLOW` and
  reads its output only after success. It never follows a replaced leaf.
- `src/download/file.rs::renameat2_confined_blocking`,
  `rename_confined_macos`, and `hard_link_confined` retain both parent
  descriptors and NUL-terminated names for no-overwrite publication.
- Windows attribute probes use live handles and correctly sized output
  storage. Initialization is assumed only after the syscall succeeds.
- `std` has no equivalent descriptor-relative traversal/publication API.
  Replacing these calls with ordinary path operations would restore the race.

## Best production removal candidates

The easiest local removals are:

1. Replace `available_disk_space` with `fs4::available_space`, removing
   both Unix expressions and the Windows FFI call.
2. Rewrite `exif_datetime_to_iso` without `String::as_bytes_mut`, removing
   its local `unsafe` block.
3. Add a direct `rustix` dependency for process, fs, and termios wrappers. That
   would remove most Unix syscall wrappers while preserving their semantics.
