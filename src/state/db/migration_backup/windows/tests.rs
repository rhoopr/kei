use std::collections::BTreeMap;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};

use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

use super::{
    LocalAllocation, attributes, create_private_dir, descriptor, directory, private_descriptor,
    private_file,
};

static SUBSTITUTIONS: LazyLock<Mutex<BTreeMap<PathBuf, Arc<AtomicBool>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

pub(crate) struct Substitution {
    root: PathBuf,
    fired: Arc<AtomicBool>,
}

impl Substitution {
    pub(crate) fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

impl Drop for Substitution {
    fn drop(&mut self) {
        SUBSTITUTIONS.lock().unwrap().remove(&self.root);
    }
}

pub(crate) fn substitute_after_handoff(root: &Path) -> Substitution {
    let root = crate::fs_util::absolute_confined_path(root).unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    assert!(
        SUBSTITUTIONS
            .lock()
            .unwrap()
            .insert(root.clone(), Arc::clone(&fired))
            .is_none()
    );
    Substitution { root, fired }
}

pub(super) fn after_handoff(source: &Path) -> std::io::Result<()> {
    let Some(parent) = source.parent() else {
        return Ok(());
    };
    let fired = SUBSTITUTIONS
        .lock()
        .map_err(|_poisoned| std::io::Error::other("stage substitution hook poisoned"))?
        .get(parent)
        .cloned();
    let Some(fired) = fired else {
        return Ok(());
    };
    if fired.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Parent confinement must survive the sharing handoff. Without it this
    // real rename could redirect both publication pathnames at once.
    assert!(
        std::fs::rename(parent, parent.with_extension("redirected-by-test")).is_err(),
        "parent confinement was dropped before publication"
    );
    // This is an actual namespace substitution, not a fabricated identity.
    // The original leaf handle now allows delete sharing for our own move.
    std::fs::rename(source, parent.join("original-stage.partial"))?;
    std::fs::create_dir(source)?;
    std::fs::write(source.join("manifest.json"), br#"{"substituted":true}"#)?;
    std::fs::write(
        source.join("foreign-evidence"),
        b"substituted directory bytes",
    )?;
    fired.store(true, Ordering::SeqCst);
    Ok(())
}

fn broad_parent(root: &Path) -> PathBuf {
    let broad = root.join("broad-parent");
    let security = descriptor("D:(A;OICI;FA;;;WD)").unwrap();
    let attributes = attributes(&security).unwrap();
    let path = crate::fs_util::windows_api_path(&broad).unwrap();
    assert_ne!(
        // SAFETY: path and the creation-time descriptor stay live for the call.
        unsafe { super::CreateDirectoryW(path.as_ptr(), &attributes) },
        0
    );
    broad
}

fn assert_private(file: &std::fs::File) {
    let mut security = LocalAllocation(std::ptr::null_mut());
    let mut owner = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut();
    assert_eq!(
        // SAFETY: file is live and each output is writable. security owns the
        // descriptor; owner and acl borrow its storage until scope end.
        unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as isize,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut security.0,
            )
        },
        0
    );
    let expected = private_descriptor().unwrap();
    let mut expected_owner = std::ptr::null_mut();
    let mut defaulted = 0;
    assert_ne!(
        // SAFETY: expected is a live valid descriptor; outputs are writable.
        unsafe { GetSecurityDescriptorOwner(expected.0, &mut expected_owner, &mut defaulted) },
        0
    );
    // SAFETY: both successful APIs supplied live valid SIDs.
    assert_ne!(unsafe { EqualSid(owner, expected_owner) }, 0);
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        // SAFETY: security is valid and both output fields are writable.
        unsafe { GetSecurityDescriptorControl(security.0, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);
    assert!(!acl.is_null());
    assert_eq!(
        // SAFETY: GetSecurityInfo supplied a valid ACL in the live descriptor.
        unsafe { (*acl).AceCount },
        1,
        "no inherited Everyone/Admin grants"
    );
    let mut ace = std::ptr::null_mut();
    // SAFETY: acl contains one ACE and ace is writable output.
    assert_ne!(unsafe { GetAce(acl, 0, &mut ace) }, 0);
    // SAFETY: our explicit descriptor creates one ACCESS_ALLOWED_ACE; the
    // returned ACE remains inside the live descriptor throughout these reads.
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(allowed.Header.AceType, 0);
    assert_eq!(allowed.Mask, FILE_ALL_ACCESS);
    let sid = std::ptr::addr_of!(allowed.SidStart).cast_mut().cast();
    // SAFETY: SidStart begins the valid variable-length SID of this live ACE.
    assert_ne!(unsafe { EqualSid(sid, expected_owner) }, 0);
}

#[test]
fn owned_migration_backup_windows_creation_is_private_under_a_broad_parent() {
    let root = tempfile::tempdir().unwrap();
    let broad = broad_parent(root.path());
    let bundle = broad.join("private.partial");
    let guard = create_private_dir(&bundle).unwrap();
    assert_private(&guard.directory);
    let file = private_file(&bundle.join("config.toml")).unwrap();
    assert_private(&file);
    drop(file);
    assert!(private_file(&bundle.join("config.toml")).is_err());
    drop(guard);
    assert!(
        create_private_dir(&bundle).is_err(),
        "never reuse an occupied stage"
    );
}

#[test]
fn owned_migration_backup_windows_directory_publication_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    let stage = root.path().join("backup.partial");
    let mut guard = create_private_dir(&stage).unwrap();
    std::fs::write(stage.join("manifest.json"), b"prepared but not complete").unwrap();
    let published = root.path().join("backup");
    drop(create_private_dir(&published).unwrap());
    std::fs::write(published.join("kept"), b"previous evidence").unwrap();
    assert!(guard.publish(&published).is_err());
    assert_eq!(
        std::fs::read(published.join("kept")).unwrap(),
        b"previous evidence"
    );
    assert!(stage.join("manifest.json").exists());
    let free = root.path().join("completed");
    guard.publish(&free).unwrap();
    assert!(!stage.exists());
    assert_eq!(
        std::fs::read(free.join("manifest.json")).unwrap(),
        b"prepared but not complete"
    );
    assert_private(&directory(&free, super::FILE_SHARE_READ).unwrap());
}

#[test]
fn owned_migration_backup_windows_native_helpers_keep_long_and_unpaired_paths() {
    let root = tempfile::tempdir().unwrap();
    let long = root
        .path()
        .join("nested".repeat(20))
        .join("nested".repeat(20));
    std::fs::create_dir_all(&long).unwrap();
    let mut name: Vec<u16> = "backup-".encode_utf16().collect();
    name.push(0xd800);
    name.extend(".partial".encode_utf16());
    let stage = long.join(std::ffi::OsString::from_wide(&name));
    assert!(stage.as_os_str().encode_wide().count() > 260);
    let mut guard = create_private_dir(&stage).unwrap();
    let file = private_file(&stage.join("config.toml")).unwrap();
    assert_private(&file);
    drop(file);
    let published = stage.with_extension("complete");
    guard.publish(&published).unwrap();
    assert!(published.join("config.toml").exists());
}

#[test]
fn owned_migration_backup_windows_substituted_stage_is_not_its_own_identity_proof() {
    let root = tempfile::tempdir().unwrap();
    let broad = broad_parent(root.path());
    let stage = broad.join("backup.partial");
    let mut guard = create_private_dir(&stage).unwrap();
    std::fs::write(stage.join("state.db"), b"original populated snapshot").unwrap();
    std::fs::write(stage.join("manifest.json"), b"original manifest").unwrap();
    assert!(
        std::fs::rename(&stage, broad.join("too-early.partial")).is_err(),
        "the population guard must initially deny delete sharing"
    );
    let substitution = substitute_after_handoff(&broad);
    let published = broad.join("backup");
    let error = guard.publish(&published).unwrap_err();
    assert!(
        substitution.fired(),
        "actual substitution must have completed"
    );
    assert!(
        error.to_string().contains("original populated stage"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(broad.join("original-stage.partial/state.db")).unwrap(),
        b"original populated snapshot"
    );
    assert_eq!(
        std::fs::read(published.join("foreign-evidence")).unwrap(),
        b"substituted directory bytes"
    );
    drop(guard);
    drop(substitution);
    // This parent is genuinely renameable when the retained capabilities are
    // gone; permission failure must not masquerade as confinement evidence.
    let redirected = broad.with_extension("redirected-by-test");
    std::fs::rename(&broad, &redirected).unwrap();
    std::fs::rename(&redirected, &broad).unwrap();
}
