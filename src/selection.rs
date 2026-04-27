//! Per-category selection model for the v0.13 selection-flags redesign.
//!
//! Each category (`albums`, `smart_folders`, `libraries`) has its own
//! selector. Selectors are the resolved view of zero or more raw user inputs:
//! sentinel words (`all`, `none`, `primary`, `shared`, `all-with-sensitive`),
//! literal names, and `!literal-name` exclusions. Parsing happens at config
//! resolution time — not at iCloud-call time — so invalid combinations fail
//! before we open a network connection.
//!
//! The four-category bundle is [`Selection`], stored on
//! [`crate::config::Config`]. The `commands::service` resolver consumes a
//! `Selection` plus the live album/library map and emits concrete sync
//! passes.
//!
//! See `.scratch/specs/selection-flags.md` for the design rationale.

use std::collections::BTreeSet;

/// Which categories of content the resolver can exclude from a category-wide
/// "all" sweep. Same shape across album / smart-folder / library selectors.
type ExcludeSet = BTreeSet<String>;

/// Album selection.
///
/// Defaults to [`AlbumSelector::All`] with no exclusions, matching the
/// "every user album" baseline of `kei sync` with no flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumSelector {
    /// Sentinel `none`: explicitly skip every album pass.
    None,
    /// Sentinel `all` (or implicit when only `!name` exclusions are passed):
    /// every user album except those listed in `excluded`.
    All { excluded: ExcludeSet },
    /// Explicit named albums (with optional `!name` excludes layered on top —
    /// rare but legal for forward compatibility with shell-glob expansion).
    Named {
        included: BTreeSet<String>,
        excluded: ExcludeSet,
    },
}

impl Default for AlbumSelector {
    fn default() -> Self {
        Self::All {
            excluded: ExcludeSet::new(),
        }
    }
}

/// Smart-folder selection.
///
/// Defaults to [`SmartFolderSelector::None`]: smart folders aren't fetched
/// unless the user opts in. This matches today's behaviour of suppressing
/// smart folders during the `-a all` enumeration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum SmartFolderSelector {
    /// Skip every smart-folder pass.
    #[default]
    None,
    /// Sentinel `all` / `all-with-sensitive`: every smart folder. The
    /// `include_sensitive` flag toggles whether Hidden and Recently Deleted
    /// are included.
    All {
        include_sensitive: bool,
        excluded: ExcludeSet,
    },
    /// Explicit named smart folders.
    Named {
        included: BTreeSet<String>,
        excluded: ExcludeSet,
    },
}

/// Library selection. Different shape from the other two because the
/// `primary` and `shared` sentinels carve out disjoint subsets.
///
/// Default: `primary = true`, everything else empty (today's `--library
/// PrimarySync` behaviour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySelector {
    /// Sentinel `primary`: include the PrimarySync zone.
    pub primary: bool,
    /// Sentinel `shared`: include every SharedSync-* zone.
    pub shared_all: bool,
    /// Explicit zone names or friendly aliases (e.g. `shared:Owner Name`,
    /// truncated `SharedSync-A1B2C3D4`, full UUID).
    pub named: BTreeSet<String>,
    /// `!name` exclusions, applied after the include set is resolved.
    pub excluded: ExcludeSet,
}

impl Default for LibrarySelector {
    fn default() -> Self {
        Self {
            primary: true,
            shared_all: false,
            named: BTreeSet::new(),
            excluded: ExcludeSet::new(),
        }
    }
}

impl LibrarySelector {
    /// True if the selector resolves to zero libraries.
    pub fn is_empty(&self) -> bool {
        !self.primary && !self.shared_all && self.named.is_empty()
    }
}

/// Bundle of every per-category selector plus the unfiled-pass toggle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub albums: AlbumSelector,
    pub smart_folders: SmartFolderSelector,
    pub libraries: LibrarySelector,
    /// Run the unfiled (no-album) pass. Default `true` — orthogonal to
    /// `albums`, so `--album Vacation` still produces an unfiled pass unless
    /// `--unfiled false` is also passed.
    pub unfiled: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            albums: AlbumSelector::default(),
            smart_folders: SmartFolderSelector::default(),
            libraries: LibrarySelector::default(),
            unfiled: true,
        }
    }
}

// ── Parsing ─────────────────────────────────────────────────────────────────

/// Parse a single raw album entry. Returns the canonical token plus a flag
/// indicating whether it was an exclusion (`!name`).
fn split_exclusion(raw: &str) -> (&str, bool) {
    raw.strip_prefix('!').map_or((raw, false), |r| (r, true))
}

/// Insert into a BTreeSet, bailing if the value was already present. The
/// duplicate-check pattern repeats across every parser; this lets callers
/// describe the offending item once.
fn insert_unique(
    set: &mut BTreeSet<String>,
    value: String,
    flag: &str,
    name: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        set.insert(value),
        "--{flag} '{name}' specified more than once"
    );
    Ok(())
}

/// Parse `--album` raw values into an [`AlbumSelector`]. `default_to_all` is
/// true when the user passed no `--album` flag at all (so the bare `!Foo`
/// exclusion case still resolves to "all minus Foo").
pub(crate) fn parse_album_selector(
    raw: &[String],
    default_to_all: bool,
) -> anyhow::Result<AlbumSelector> {
    if raw.is_empty() {
        return Ok(if default_to_all {
            AlbumSelector::default()
        } else {
            AlbumSelector::None
        });
    }

    let mut has_all = false;
    let mut has_none = false;
    let mut included: BTreeSet<String> = BTreeSet::new();
    let mut excluded: BTreeSet<String> = BTreeSet::new();

    for entry in raw {
        let trimmed = entry.trim();
        anyhow::ensure!(!trimmed.is_empty(), "--album value must not be empty");
        let (name, is_exclude) = split_exclusion(trimmed);
        if name.eq_ignore_ascii_case("all") {
            anyhow::ensure!(
                !is_exclude,
                "'!all' is not a valid --album value; pass --album none instead"
            );
            has_all = true;
        } else if name.eq_ignore_ascii_case("none") {
            anyhow::ensure!(
                !is_exclude,
                "'!none' is not a valid --album value; just omit it"
            );
            has_none = true;
        } else if is_exclude {
            insert_unique(
                &mut excluded,
                name.to_string(),
                "album",
                &format!("!{name}"),
            )?;
        } else {
            insert_unique(&mut included, name.to_string(), "album", name)?;
        }
    }

    contradiction_check("album", &included, &excluded)?;

    if has_none {
        anyhow::ensure!(
            !has_all && included.is_empty() && excluded.is_empty(),
            "'--album none' cannot be combined with other --album values"
        );
        return Ok(AlbumSelector::None);
    }
    if has_all {
        anyhow::ensure!(
            included.is_empty(),
            "'--album all' cannot be combined with literal album names; use '!name' to exclude one"
        );
        return Ok(AlbumSelector::All { excluded });
    }
    if !included.is_empty() {
        return Ok(AlbumSelector::Named { included, excluded });
    }
    // Only exclusions present → "all minus excluded".
    Ok(AlbumSelector::All { excluded })
}

/// Parse `--smart-folder` raw values. Default is `None` — smart folders are
/// off unless the user opts in.
pub(crate) fn parse_smart_folder_selector(raw: &[String]) -> anyhow::Result<SmartFolderSelector> {
    if raw.is_empty() {
        return Ok(SmartFolderSelector::None);
    }

    let mut has_all = false;
    let mut has_all_sensitive = false;
    let mut has_none = false;
    let mut included: BTreeSet<String> = BTreeSet::new();
    let mut excluded: BTreeSet<String> = BTreeSet::new();

    for entry in raw {
        let trimmed = entry.trim();
        anyhow::ensure!(
            !trimmed.is_empty(),
            "--smart-folder value must not be empty"
        );
        let (name, is_exclude) = split_exclusion(trimmed);
        if name.eq_ignore_ascii_case("all") {
            anyhow::ensure!(!is_exclude, "'!all' is not a valid --smart-folder value");
            has_all = true;
        } else if name.eq_ignore_ascii_case("all-with-sensitive") {
            anyhow::ensure!(
                !is_exclude,
                "'!all-with-sensitive' is not a valid --smart-folder value"
            );
            has_all_sensitive = true;
        } else if name.eq_ignore_ascii_case("none") {
            anyhow::ensure!(!is_exclude, "'!none' is not a valid --smart-folder value");
            has_none = true;
        } else if is_exclude {
            insert_unique(
                &mut excluded,
                name.to_string(),
                "smart-folder",
                &format!("!{name}"),
            )?;
        } else {
            insert_unique(&mut included, name.to_string(), "smart-folder", name)?;
        }
    }

    contradiction_check("smart-folder", &included, &excluded)?;

    if has_none {
        anyhow::ensure!(
            !has_all && !has_all_sensitive && included.is_empty() && excluded.is_empty(),
            "'--smart-folder none' cannot be combined with other --smart-folder values"
        );
        return Ok(SmartFolderSelector::None);
    }
    if has_all || has_all_sensitive {
        anyhow::ensure!(
            !(has_all && has_all_sensitive),
            "'--smart-folder all' and '--smart-folder all-with-sensitive' are mutually exclusive"
        );
        anyhow::ensure!(
            included.is_empty(),
            "'--smart-folder all' cannot be combined with literal names; use '!name' to exclude one"
        );
        return Ok(SmartFolderSelector::All {
            include_sensitive: has_all_sensitive,
            excluded,
        });
    }
    if !included.is_empty() {
        return Ok(SmartFolderSelector::Named { included, excluded });
    }
    // Only exclusions on the empty default is an interesting corner: warn at
    // a higher level, not here. Resolve to "every smart folder minus those".
    Ok(SmartFolderSelector::All {
        include_sensitive: false,
        excluded,
    })
}

/// Parse `--library` raw values into a [`LibrarySelector`].
///
/// Default (empty input) = `primary = true` only. Sentinels: `primary`,
/// `shared`, `all`, `none`, plus literal zone names / friendly aliases.
pub(crate) fn parse_library_selector(raw: &[String]) -> anyhow::Result<LibrarySelector> {
    if raw.is_empty() {
        return Ok(LibrarySelector::default());
    }

    let mut sel = LibrarySelector {
        primary: false,
        shared_all: false,
        named: BTreeSet::new(),
        excluded: BTreeSet::new(),
    };
    let mut has_all = false;
    let mut has_none = false;

    for entry in raw {
        let trimmed = entry.trim();
        anyhow::ensure!(!trimmed.is_empty(), "--library value must not be empty");
        let (name, is_exclude) = split_exclusion(trimmed);
        if name.eq_ignore_ascii_case("all") {
            anyhow::ensure!(!is_exclude, "'!all' is not a valid --library value");
            has_all = true;
        } else if name.eq_ignore_ascii_case("none") {
            anyhow::ensure!(!is_exclude, "'!none' is not a valid --library value");
            has_none = true;
        } else if name.eq_ignore_ascii_case("primary") {
            if is_exclude {
                insert_unique(
                    &mut sel.excluded,
                    "primary".to_string(),
                    "library",
                    "!primary",
                )?;
            } else {
                sel.primary = true;
            }
        } else if name.eq_ignore_ascii_case("shared") {
            if is_exclude {
                insert_unique(
                    &mut sel.excluded,
                    "shared".to_string(),
                    "library",
                    "!shared",
                )?;
            } else {
                sel.shared_all = true;
            }
        } else if is_exclude {
            insert_unique(
                &mut sel.excluded,
                name.to_string(),
                "library",
                &format!("!{name}"),
            )?;
        } else {
            insert_unique(&mut sel.named, name.to_string(), "library", name)?;
        }
    }

    if has_none {
        anyhow::bail!(
            "'--library none' is not allowed: kei needs at least one source library to sync"
        );
    }
    if has_all {
        sel.primary = true;
        sel.shared_all = true;
    }
    // If only exclusions or shared-only inputs were given without `primary`,
    // honour exactly what the user said. The "exclude implies all" rule for
    // libraries means: a bare `!Foo` would imply primary (the category
    // default), which we apply here.
    if !sel.primary && !sel.shared_all && sel.named.is_empty() && !sel.excluded.is_empty() {
        sel.primary = true;
    }

    contradiction_check("library", &sel.named, &sel.excluded)?;

    if sel.is_empty() {
        anyhow::bail!(
            "--library resolved to no libraries; pass at least one of primary / shared / a zone name"
        );
    }
    Ok(sel)
}

/// Verify the same name doesn't appear as both an include and an exclude.
/// Spec: "Mixing positive and exclusion of the same name in any order: bail
/// at parse time."
fn contradiction_check(
    category: &str,
    included: &BTreeSet<String>,
    excluded: &BTreeSet<String>,
) -> anyhow::Result<()> {
    if let Some(name) = included.intersection(excluded).next() {
        anyhow::bail!("cannot both include and exclude '{name}' in --{category}; pick one");
    }
    Ok(())
}

/// Build a [`Selection`] from raw CLI/TOML inputs.
///
/// `cli_albums_explicit` is the raw `--album` list (may be empty).
/// `cli_smart_folders` and `cli_libraries` are the matching raw lists.
/// `unfiled_explicit` is the explicit `--unfiled` value if the user passed
/// one, `None` to use the default (`true`).
pub(crate) fn build_selection(
    raw_albums: &[String],
    raw_smart_folders: &[String],
    raw_libraries: &[String],
    unfiled_explicit: Option<bool>,
) -> anyhow::Result<Selection> {
    Ok(Selection {
        albums: parse_album_selector(raw_albums, true)?,
        smart_folders: parse_smart_folder_selector(raw_smart_folders)?,
        libraries: parse_library_selector(raw_libraries)?,
        unfiled: unfiled_explicit.unwrap_or(true),
    })
}

// ── Serialization helpers ───────────────────────────────────────────────────

impl AlbumSelector {
    /// Serialize back to the raw `Vec<String>` form a user would write on the
    /// CLI / in TOML. `None` and `All` with an empty exclusion set serialize
    /// to a single sentinel; everything else lists positives + `!exclusions`.
    pub fn to_raw(&self) -> Vec<String> {
        match self {
            Self::None => vec!["none".to_string()],
            Self::All { excluded } => std::iter::once("all".to_string())
                .chain(excluded.iter().map(|n| format!("!{n}")))
                .collect(),
            Self::Named { included, excluded } => included
                .iter()
                .cloned()
                .chain(excluded.iter().map(|n| format!("!{n}")))
                .collect(),
        }
    }
}

impl SmartFolderSelector {
    pub fn to_raw(&self) -> Vec<String> {
        match self {
            Self::None => Vec::new(),
            Self::All {
                include_sensitive,
                excluded,
            } => {
                let head = if *include_sensitive {
                    "all-with-sensitive"
                } else {
                    "all"
                };
                std::iter::once(head.to_string())
                    .chain(excluded.iter().map(|n| format!("!{n}")))
                    .collect()
            }
            Self::Named { included, excluded } => included
                .iter()
                .cloned()
                .chain(excluded.iter().map(|n| format!("!{n}")))
                .collect(),
        }
    }
}

impl LibrarySelector {
    pub fn to_raw(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.primary && self.shared_all && self.named.is_empty() {
            out.push("all".to_string());
        } else {
            if self.primary {
                out.push("primary".to_string());
            }
            if self.shared_all {
                out.push("shared".to_string());
            }
            for n in &self.named {
                out.push(n.clone());
            }
        }
        for n in &self.excluded {
            out.push(format!("!{n}"));
        }
        out
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    // ── AlbumSelector ─────────────────────────────────────────────────

    #[test]
    fn album_default_is_all() {
        assert_eq!(
            AlbumSelector::default(),
            AlbumSelector::All {
                excluded: BTreeSet::new()
            }
        );
    }

    #[test]
    fn album_empty_with_default_to_all() {
        let s = parse_album_selector(&[], true).unwrap();
        assert_eq!(
            s,
            AlbumSelector::All {
                excluded: BTreeSet::new()
            }
        );
    }

    #[test]
    fn album_empty_without_default_to_all() {
        let s = parse_album_selector(&[], false).unwrap();
        assert_eq!(s, AlbumSelector::None);
    }

    #[test]
    fn album_all_sentinel() {
        let r = parse_album_selector(&s(&["all"]), false).unwrap();
        assert_eq!(
            r,
            AlbumSelector::All {
                excluded: BTreeSet::new()
            }
        );
    }

    #[test]
    fn album_all_case_insensitive() {
        let r = parse_album_selector(&s(&["ALL"]), false).unwrap();
        assert_eq!(
            r,
            AlbumSelector::All {
                excluded: BTreeSet::new()
            }
        );
    }

    #[test]
    fn album_none_sentinel() {
        let r = parse_album_selector(&s(&["none"]), false).unwrap();
        assert_eq!(r, AlbumSelector::None);
    }

    #[test]
    fn album_named() {
        let r = parse_album_selector(&s(&["Vacation", "Family"]), false).unwrap();
        assert_eq!(
            r,
            AlbumSelector::Named {
                included: set(&["Vacation", "Family"]),
                excluded: BTreeSet::new(),
            }
        );
    }

    #[test]
    fn album_all_with_exclusion() {
        let r = parse_album_selector(&s(&["all", "!Family"]), false).unwrap();
        assert_eq!(
            r,
            AlbumSelector::All {
                excluded: set(&["Family"]),
            }
        );
    }

    #[test]
    fn album_bare_exclusion_implies_all() {
        let r = parse_album_selector(&s(&["!Family"]), true).unwrap();
        assert_eq!(
            r,
            AlbumSelector::All {
                excluded: set(&["Family"]),
            }
        );
    }

    #[test]
    fn album_named_with_exclusion() {
        let r = parse_album_selector(&s(&["Vacation", "!Family"]), false).unwrap();
        assert_eq!(
            r,
            AlbumSelector::Named {
                included: set(&["Vacation"]),
                excluded: set(&["Family"]),
            }
        );
    }

    #[test]
    fn album_contradiction_bails() {
        let err = parse_album_selector(&s(&["Vacation", "!Vacation"]), false).unwrap_err();
        assert!(err.to_string().contains("Vacation"));
    }

    #[test]
    fn album_all_plus_named_bails() {
        let err = parse_album_selector(&s(&["all", "Vacation"]), false).unwrap_err();
        assert!(err.to_string().contains("'--album all'"));
    }

    #[test]
    fn album_none_plus_other_bails() {
        let err = parse_album_selector(&s(&["none", "Vacation"]), false).unwrap_err();
        assert!(err.to_string().contains("'--album none'"));
    }

    #[test]
    fn album_duplicate_name_bails() {
        let err = parse_album_selector(&s(&["Vacation", "Vacation"]), false).unwrap_err();
        assert!(err.to_string().contains("Vacation"));
    }

    #[test]
    fn album_empty_string_bails() {
        let err = parse_album_selector(&s(&[""]), false).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn album_bang_sentinel_bails() {
        let err = parse_album_selector(&s(&["!all"]), false).unwrap_err();
        assert!(err.to_string().contains("'!all'"));
    }

    // ── SmartFolderSelector ────────────────────────────────────────────

    #[test]
    fn smart_folder_default_is_none() {
        assert_eq!(SmartFolderSelector::default(), SmartFolderSelector::None);
    }

    #[test]
    fn smart_folder_empty_is_none() {
        let r = parse_smart_folder_selector(&[]).unwrap();
        assert_eq!(r, SmartFolderSelector::None);
    }

    #[test]
    fn smart_folder_all_excludes_sensitive_by_default() {
        let r = parse_smart_folder_selector(&s(&["all"])).unwrap();
        assert_eq!(
            r,
            SmartFolderSelector::All {
                include_sensitive: false,
                excluded: BTreeSet::new(),
            }
        );
    }

    #[test]
    fn smart_folder_all_with_sensitive() {
        let r = parse_smart_folder_selector(&s(&["all-with-sensitive"])).unwrap();
        assert_eq!(
            r,
            SmartFolderSelector::All {
                include_sensitive: true,
                excluded: BTreeSet::new(),
            }
        );
    }

    #[test]
    fn smart_folder_all_and_all_with_sensitive_mutually_exclusive() {
        let err = parse_smart_folder_selector(&s(&["all", "all-with-sensitive"])).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn smart_folder_named() {
        let r = parse_smart_folder_selector(&s(&["Favorites", "Videos"])).unwrap();
        assert_eq!(
            r,
            SmartFolderSelector::Named {
                included: set(&["Favorites", "Videos"]),
                excluded: BTreeSet::new(),
            }
        );
    }

    #[test]
    fn smart_folder_exclusion_only_resolves_to_all() {
        let r = parse_smart_folder_selector(&s(&["!Hidden"])).unwrap();
        assert_eq!(
            r,
            SmartFolderSelector::All {
                include_sensitive: false,
                excluded: set(&["Hidden"]),
            }
        );
    }

    #[test]
    fn smart_folder_none_plus_other_bails() {
        let err = parse_smart_folder_selector(&s(&["none", "Favorites"])).unwrap_err();
        assert!(err.to_string().contains("'--smart-folder none'"));
    }

    // ── LibrarySelector ────────────────────────────────────────────────

    #[test]
    fn library_default_is_primary() {
        let r = LibrarySelector::default();
        assert!(r.primary);
        assert!(!r.shared_all);
        assert!(r.named.is_empty());
    }

    #[test]
    fn library_empty_input_is_primary() {
        let r = parse_library_selector(&[]).unwrap();
        assert_eq!(r, LibrarySelector::default());
    }

    #[test]
    fn library_primary_sentinel() {
        let r = parse_library_selector(&s(&["primary"])).unwrap();
        assert!(r.primary);
        assert!(!r.shared_all);
    }

    #[test]
    fn library_shared_sentinel() {
        let r = parse_library_selector(&s(&["shared"])).unwrap();
        assert!(!r.primary);
        assert!(r.shared_all);
    }

    #[test]
    fn library_all_sentinel() {
        let r = parse_library_selector(&s(&["all"])).unwrap();
        assert!(r.primary);
        assert!(r.shared_all);
    }

    #[test]
    fn library_primary_plus_shared() {
        let r = parse_library_selector(&s(&["primary", "shared"])).unwrap();
        assert!(r.primary);
        assert!(r.shared_all);
    }

    #[test]
    fn library_named_zones() {
        let r = parse_library_selector(&s(&["SharedSync-A1B2C3D4"])).unwrap();
        assert!(!r.primary);
        assert_eq!(r.named, set(&["SharedSync-A1B2C3D4"]));
    }

    #[test]
    fn library_friendly_alias() {
        let r = parse_library_selector(&s(&["shared:Owner Name"])).unwrap();
        assert_eq!(r.named, set(&["shared:Owner Name"]));
    }

    #[test]
    fn library_none_bails() {
        let err = parse_library_selector(&s(&["none"])).unwrap_err();
        assert!(err.to_string().contains("'--library none'"));
    }

    #[test]
    fn library_only_exclusion_implies_primary() {
        // `--library !shared` resolves to primary minus shared (which has no
        // effect since primary doesn't include shared) — still a valid setup.
        let r = parse_library_selector(&s(&["!Foo"])).unwrap();
        assert!(r.primary);
        assert_eq!(r.excluded, set(&["Foo"]));
    }

    #[test]
    fn library_excluded_named_collision_bails() {
        let err = parse_library_selector(&s(&["Foo", "!Foo"])).unwrap_err();
        assert!(err.to_string().contains("Foo"));
    }

    // ── Selection ─────────────────────────────────────────────────────

    #[test]
    fn selection_defaults() {
        let s = Selection::default();
        assert_eq!(s.albums, AlbumSelector::default());
        assert_eq!(s.smart_folders, SmartFolderSelector::None);
        assert!(s.libraries.primary);
        assert!(!s.libraries.shared_all);
        assert!(s.unfiled);
    }

    #[test]
    fn build_selection_no_input_is_default() {
        let s = build_selection(&[], &[], &[], None).unwrap();
        assert_eq!(s, Selection::default());
    }

    #[test]
    fn build_selection_unfiled_explicit_false() {
        let s = build_selection(&[], &[], &[], Some(false)).unwrap();
        assert!(!s.unfiled);
    }

    #[test]
    fn build_selection_full_example() {
        let s = build_selection(
            &s(&["all", "!Family"]),
            &s(&["Favorites"]),
            &s(&["primary", "shared"]),
            Some(true),
        )
        .unwrap();
        assert_eq!(
            s.albums,
            AlbumSelector::All {
                excluded: set(&["Family"]),
            }
        );
        assert_eq!(
            s.smart_folders,
            SmartFolderSelector::Named {
                included: set(&["Favorites"]),
                excluded: BTreeSet::new(),
            }
        );
        assert!(s.libraries.primary);
        assert!(s.libraries.shared_all);
        assert!(s.unfiled);
    }
}
