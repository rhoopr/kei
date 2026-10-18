//! Bounded, offline acceptance of local files for legacy migration.
use crate::state::db::trusted_legacy::{MAX_TARGETS, TrustedLegacyFile};
use crate::state::{SqliteStateDb, error::StateError};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrustedLegacyPlan {
    version: u32,
    root: PathBuf,
    temp_suffix: String,
    allow_hardlinks: bool,
    pub(crate) targets: Vec<TrustedLegacyFile>,
}

struct Retained {
    path: crate::fs_util::ConfinedPath,
    file: std::fs::File,
    target: TrustedLegacyFile,
}
fn check_temporary(root: &Path, path: &Path, suffix: &str) -> anyhow::Result<()> {
    for suffix in [suffix, ".part"] {
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(suffix);
        anyhow::ensure!(
            !crate::fs_util::ConfinedPath::open(
                root,
                Path::new(&temporary),
                crate::fs_util::ConfinedParents::Existing
            )?
            .entry_exists()?,
            "Temporary write is pending"
        );
    }
    Ok(())
}
fn retain(
    root: &Path,
    target: &TrustedLegacyFile,
    suffix: &str,
    allow_hardlinks: bool,
) -> anyhow::Result<Retained> {
    check_temporary(root, &target.path, suffix)?;
    let path = crate::fs_util::ConfinedPath::open(
        root,
        &target.path,
        crate::fs_util::ConfinedParents::Existing,
    )?;
    let mut file = path.open_regular()?;
    let links = crate::fs_util::file_link_count(&file)?;
    anyhow::ensure!(
        links == 1 || (allow_hardlinks && links > 0),
        "Hardlinked files require the existing explicit legacy hardlink opt-in"
    );
    let identity = crate::fs_util::file_identity(&file)?;
    let actual =
        super::file::fingerprint_open_file_snapshot_blocking(&mut file, &target.path)?.fingerprint;
    anyhow::ensure!(
        data_encoding::HEXLOWER.encode(&actual.sha256) == target.local_sha256,
        "Local bytes changed"
    );
    path.validate_identity(identity)?;
    let mut target = target.clone();
    target.local_size = actual.size;
    Ok(Retained { path, file, target })
}

pub(crate) async fn plan(
    db: &SqliteStateDb,
    library: &str,
    root: &Path,
    temp_suffix: &str,
    allow_hardlinks: bool,
    cancel: &CancellationToken,
) -> anyhow::Result<TrustedLegacyPlan> {
    let targets = db.trusted_legacy_candidates(library).await?;
    anyhow::ensure!(
        !targets.is_empty(),
        "No eligible preserved legacy current-child files"
    );
    let mut accepted = Vec::new();
    for target in targets {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Trusted legacy file planning cancelled"
        );
        anyhow::ensure!(
            !super::file::has_replacement_journal(root, &target.path).await?,
            "Replacement recovery is pending"
        );
        let (root, suffix) = (root.to_path_buf(), temp_suffix.to_owned());
        accepted.push(
            tokio::task::spawn_blocking(move || retain(&root, &target, &suffix, allow_hardlinks))
                .await??
                .target,
        );
    }
    Ok(TrustedLegacyPlan {
        version: 1,
        root: root.to_path_buf(),
        temp_suffix: temp_suffix.into(),
        allow_hardlinks,
        targets: accepted,
    })
}

pub(crate) async fn apply(
    db: &SqliteStateDb,
    plan: TrustedLegacyPlan,
    root: &Path,
    temp_suffix: &str,
    allow_hardlinks: bool,
    cancel: &CancellationToken,
) -> anyhow::Result<usize> {
    anyhow::ensure!(
        plan.version == 1
            && plan.root == root
            && plan.temp_suffix == temp_suffix
            && plan.allow_hardlinks == allow_hardlinks
            && !plan.targets.is_empty()
            && plan.targets.len() <= MAX_TARGETS,
        "Plan settings changed or unsupported plan"
    );
    let library = &plan.targets.first().context("Empty plan")?.library;
    anyhow::ensure!(
        plan.targets.iter().all(|t| &t.library == library),
        "Plan spans multiple libraries"
    );
    let mut files = Vec::new();
    for target in &plan.targets {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Trusted legacy file acceptance cancelled"
        );
        anyhow::ensure!(
            !super::file::has_replacement_journal(root, &target.path).await?,
            "Replacement recovery is pending"
        );
        let (root, suffix, owned_target) =
            (root.to_path_buf(), temp_suffix.to_owned(), target.clone());
        let retained = tokio::task::spawn_blocking(move || {
            retain(&root, &owned_target, &suffix, allow_hardlinks)
        })
        .await??;
        anyhow::ensure!(&retained.target == target, "Local file size changed");
        files.push(retained);
    }
    let count = plan.targets.len();
    let cancel = cancel.clone();
    db.apply_trusted_legacy_files(plan.targets, move || {
        let validate = || -> anyhow::Result<()> {
            use std::io::{Seek, SeekFrom};
            for mut retained in files {
                anyhow::ensure!(!cancel.is_cancelled(), "Acceptance cancelled");
                check_temporary(&plan.root, &retained.target.path, &plan.temp_suffix)?;
                let links = crate::fs_util::file_link_count(&retained.file)?;
                anyhow::ensure!(
                    links == 1 || (plan.allow_hardlinks && links > 0),
                    "Hardlink policy changed before commit"
                );
                let identity = crate::fs_util::file_identity(&retained.file)?;
                retained.path.validate_identity(identity)?;
                retained.file.seek(SeekFrom::Start(0))?;
                let actual = super::file::fingerprint_open_file_snapshot_blocking(
                    &mut retained.file,
                    &retained.target.path,
                )?
                .fingerprint;
                anyhow::ensure!(
                    actual.size == retained.target.local_size
                        && data_encoding::HEXLOWER.encode(&actual.sha256)
                            == retained.target.local_sha256,
                    "Accepted file changed before commit"
                );
                retained.path.validate_identity(identity)?;
            }
            anyhow::ensure!(!cancel.is_cancelled(), "Acceptance cancelled");
            Ok(())
        };
        validate().map_err(|_private_error| StateError::Invariant {
            operation: "trusted_legacy_file",
            detail: "accepted file changed or acceptance cancelled before commit".into(),
        })
    })
    .await?;
    Ok(count)
}
