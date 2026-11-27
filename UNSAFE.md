# Unsafe usage audit

Audit date: 2026-11-27.

This file lists each `unsafe` block or expression in repo-owned production
Rust source. Test modules, `tests/`, plain-text mentions of "unsafe", Cargo
lint settings, and dependency lockfile entries are not counted. There are no
`unsafe fn`, `unsafe impl`, or raw `extern` declarations in production code.

The Linux sidecar receipt regression uses a one-byte shared mapping of its private fixture to exercise edits that do not advance ctime. The test keeps the file nonempty and the mapping writable until its guard unmaps it.

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
| `src/download/file/platform.rs::renameat2_blocking` | `libc::syscall(SYS_renameat2, ...)` with `RENAME_NOREPLACE` or `RENAME_EXCHANGE`. | Promotes a verified `.part` file without overwrite, or atomically exchanges it with an explicitly authorized truncated file or a preservation-backed owned primary slot. The paths are owned `CString`s and no Rust references are retained by the kernel. | No safe `std` equivalent for either Linux rename mode. A direct `rustix` fs dependency can replace this with `renameat_with` and matching flags. Do not rewrite either path as an existence check followed by `rename`; that would reintroduce a race. |
| `src/download/file/platform.rs::rename_exchange_blocking` (macOS) | `libc::renamex_np(..., RENAME_SWAP)` in `rename_exchange_blocking`. | Atomically exchanges a verified `.part` file with the exact file approved by truncated-repair planning or the preservation-backed layout owner. Both paths are owned `CString`s and remain valid for the call. | No safe `std` equivalent for an atomic path exchange. A direct safe wrapper could move the FFI out of kei. A sequence of ordinary renames would add a crash window. |
| `src/fs_util.rs::move_file_no_replace_blocking` (Windows) | Windows `MoveFileExW(..., MOVEFILE_WRITE_THROUGH)` in `move_file_no_replace_blocking`. | Promotes media staging files and private migration-backup directories without replacement or cross-volume copy fallback. The lossless UTF-16 paths are absolute, NUL-terminated and live for the call, with extended drive or UNC namespaces for ordinary paths. | Not cleanly removable with `std` while preserving native write-through behavior. This is the Windows publication contract, not a claim of POSIX directory-fsync equivalence or physical power-loss survival. |
| `src/download/file/platform.rs::replace_file_with_backup_blocking` (Windows) | Windows `ReplaceFileW(..., REPLACEFILE_WRITE_THROUGH)` in `replace_file_with_backup_blocking`. | Atomically replaces an explicitly authorized truncated file and moves its old bytes to a distinct backup path for post-exchange verification or restoration. Every lossless UTF-16 path is absolute, NUL-terminated and valid for the call, with extended drive or UNC namespaces for ordinary paths. | No safe `std` equivalent provides atomic replacement with a backup. A safe Windows wrapper could move the FFI out of kei, but ordinary remove and rename calls would weaken crash and race safety. |
| `src/download/metadata.rs::exif_datetime_to_iso` | `String::as_bytes_mut()` in `exif_datetime_to_iso`. | Mutates three delimiter bytes in an owned `String`. The length and delimiter checks prove the byte positions exist, and the replacement bytes are ASCII, so UTF-8 stays valid. | Removable. Build a `Vec<u8>` from `s.as_bytes()`, mutate the vector with safe indexing after the same length checks, and convert with `String::from_utf8`. A formatting-based rewrite would also work. |
| `src/service/env.rs::effective_uid` | `libc::geteuid()` in `effective_uid`. | Centralizes effective-UID lookup for Unix service backends. `geteuid` is stateless and has no pointer or aliasing preconditions. | Removable by adding a direct safe wrapper dependency, for example `rustix::process::geteuid` or `nix::unistd::geteuid`. This is a good cleanup target because tests duplicate this same unsafe call. |

## Reconciliation leaf checks

- `src/fs_util.rs::file_identity` (Windows): handle-information calls
  write into correctly sized storage. The live handle outlasts each call;
  `assume_init` runs only after success. The result pins comparisons to the
  opened file. Safe `std` has no equivalent stable Windows file-ID API.

## Reconciliation ancestor checks

- `src/fs_util.rs::openat_owned` transfers each successful syscall descriptor
  into one `OwnedFd`. The retained directory and NUL-terminated name remain
  live; the variadic mode has the promoted C integer type required on macOS.
- `ConfinedPath::open_platform` uses `mkdirat` beneath retained directories,
  then opens each component with `O_DIRECTORY | O_NOFOLLOW`. Every created
  directory's parent is synced before traversal continues.
- `ConfinedPath::entry_exists` uses `fstatat` with `AT_SYMLINK_NOFOLLOW` and
  reads its output only after success. It never follows a replaced leaf.
- `src/download/file/platform.rs::hard_link_confined` retains both parent
  descriptors and NUL-terminated names for no-overwrite publication.
- `src/fs_util.rs::windows_file_information` and Windows attribute probes use
  live handles and correctly sized output storage. Initialization is assumed
  only after the syscall succeeds. The same initialized structure supplies
  the link count used to reject shared file objects during legacy preservation;
  stable `std` does not expose that Windows handle field.
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

## Journaled Linux replacement

- `src/download/file/replacement_recovery.rs::unlink`: `unlinkat` uses a
  retained parent descriptor and a NUL-terminated leaf. It never follows the
  leaf. Byte checks authorize removal; unknown entries retain the journal.
- `move_to_owned_slot`: `renameat` moves into a never-used slot inside an
  exclusively created owner-only journal directory. Both directory descriptors
  and names outlive the call. Public-target restoration never uses rename.
- `Journal::create`: `mkdirat` uses a retained parent and refuses an existing
  leaf. Mode `0700` keeps recovery entries private.
- `Journal::open`: `openat` uses a retained parent, `O_NOFOLLOW`, and
  `O_NONBLOCK`. The resulting descriptor becomes one owned `File`; its identity
  must match the no-follow regular-file check before locking or reading.
- The replacement recovery test launcher installs a seccomp filter in its
  child process before exec. The filter lives through `prctl`, rejects only
  flagged `renameat2` calls, and leaves the parent process unchanged.


## Disposable Linux late-I/O qualification fixture

`tests/fixtures/late_io_fixture.c` interposes `write`, `fdatasync` and `fsync`
only inside an exclusive disposable test child. It is compiled locally and
passed through that child's `LD_PRELOAD`; production executables do not load it.
`pthread_once` resolves matching libc forwarding signatures before concurrent
calls. `fstat` and bounded `/proc/self/fd` reads restrict injected failures to
regular `.part` files beneath the immutable child-owned root, or the existing
`.kei-reconcile-*.kei-tmp` copy files for the reconciliation file-sync case.
Directory cases restrict failures to child-owned paths; strict reconciliation
matches the recorded target parent only after target bytes exist, and strict
replacement matches the sidecar parent only after a synced journal commit marker.
Successful prefix writes precede injected write errors. Atomically renamed audit
markers contain only operation, errno and byte progress. A monotonic ten-second
child-local rendezvous holds EIO writes until transport shutdown/cancellation;
polling waits do not claim physical storage timing or durability. The existing
Rust test-only terminal selector forces the journal fallback without constructing
a sync error; the preload fixture supplies the actual syscall error.

The fixture never changes a running host service or fault setting. Its parent
runs one child per case and checks the actual syscall marker plus filesystem
and reopened SQLite outcomes. Environment variables are supplied to the child
without mutating the multithreaded parent's environment. No production Rust
unsafe block or stronger power-loss/storage promise is introduced.

## Test-only allocation high-water regression

`src/test_helpers/allocation.rs` wraps `System` only in library test builds. Each unsafe allocator call forwards the caller's original pointer,
layout and size unchanged, without dereferencing or retaining pointers.
Const thread-local counters allocate nothing and isolate synchronous production
path measurements from parallel tests. The evidence measures net live Rust heap
growth and individual allocation sizes, not RSS or the C++ toolkit's heap.

## Linux session-lock inheritance test

The test-only `auth::session::lock_inheritance::Holder` forks a descriptor
holder. Its child uses only open, flock, close, poll, read, write and _exit
with preallocated storage. It never allocates, acquires Rust locks or runs
destructors after fork. Every protocol exit explicitly unlocks only locks that
the child acquired. The parent uses bounded socket I/O and reaps its child.
Only synthetic sessions are used; diagnostics contain process and descriptor
numbers, probe operations and outcomes, without account identifiers or paths.
