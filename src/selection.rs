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
    /// Explicit zone names. Either the full CloudKit zone (`PrimarySync`,
    /// `SharedSync-A1B2C3D4-...-...`) or the truncated 8-char form
    /// (`SharedSync-A1B2C3D4`) that `{library}` renders into paths.
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
/// describe the offending item once. `display_name` is a closure so the
/// `format!("!{name}")` allocation only runs on the bail path, not on
/// every successful insert.
fn insert_unique(
    set: &mut BTreeSet<String>,
    value: String,
    flag: &str,
    display_name: impl FnOnce() -> String,
) -> anyhow::Result<()> {
    if !set.insert(value) {
        anyhow::bail!("--{flag} '{}' specified more than once", display_name());
    }
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
            insert_unique(&mut excluded, name.to_string(), "album", || {
                format!("!{name}")
            })?;
        } else {
            insert_unique(&mut included, name.to_string(), "album", || {
                name.to_string()
            })?;
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
            insert_unique(&mut excluded, name.to_string(), "smart-folder", || {
                format!("!{name}")
            })?;
        } else {
            insert_unique(&mut included, name.to_string(), "smart-folder", || {
                name.to_string()
            })?;
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
/// Default (empty input) = `primary = true` only. Accepted: bare sentinels
/// (`primary`, `shared`, `all`, `none`) and zone names (`PrimarySync`,
/// `SharedSync-A1B2C3D4` truncated, or the full UUID form), each with an
/// optional `!` prefix for exclusions. `shared:Owner`-style friendly forms
/// bail at parse time.
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
        // CloudKit zone names use `-`, never `:`. The `:` forms are an old
        // friendly-alias surface ("shared:Owner Name") that kei does not
        // resolve. Bail at parse time with the supported alternatives so
        // users don't get a "not found" surprise after a network round-trip.
        if name.contains(':') {
            anyhow::bail!(
                "--library values are bare names (`primary`, `shared`, `all`) or zone names (`PrimarySync`, `SharedSync-XYZ`); \
                 friendly forms like '{name}' are not supported. Run `kei list libraries` to see every zone."
            );
        }
        if name.eq_ignore_ascii_case("all") {
            anyhow::ensure!(!is_exclude, "'!all' is not a valid --library value");
            has_all = true;
        } else if name.eq_ignore_ascii_case("none") {
            anyhow::ensure!(!is_exclude, "'!none' is not a valid --library value");
            has_none = true;
        } else if name.eq_ignore_ascii_case("primary") {
            if is_exclude {
                insert_unique(&mut sel.excluded, "primary".to_string(), "library", || {
                    "!primary".to_string()
                })?;
            } else {
                sel.primary = true;
            }
        } else if name.eq_ignore_ascii_case("shared") {
            if is_exclude {
                insert_unique(&mut sel.excluded, "shared".to_string(), "library", || {
                    "!shared".to_string()
                })?;
            } else {
                sel.shared_all = true;
            }
        } else if is_exclude {
            insert_unique(&mut sel.excluded, name.to_string(), "library", || {
                format!("!{name}")
            })?;
        } else {
            insert_unique(&mut sel.named, name.to_string(), "library", || {
                name.to_string()
            })?;
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

/// Build a [`Selection`] from raw CLI/TOML inputs. Test-only convenience
/// for exercising every parser in one shot.
#[cfg(test)]
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
    fn library_friendly_alias_bails_at_parse_time() {
        // Friendly forms ("shared:Owner Name", "primary:something") aren't
        // supported. Bailing at parse beats a "not found" surprise after a
        // CloudKit round-trip.
        let err = parse_library_selector(&s(&["shared:Owner Name"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not supported"), "msg: {msg}");
        assert!(msg.contains("shared:Owner Name"), "msg: {msg}");
    }

    #[test]
    fn library_friendly_alias_bails_inside_exclusion() {
        // Same rule applies to `!shared:Owner` exclusions.
        let err = parse_library_selector(&s(&["!shared:Owner"])).unwrap_err();
        assert!(err.to_string().contains("not supported"));
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

    // ── Round-trip via to_raw() ────────────────────────────────────────
    //
    // Serialization round-trips.
    //
    // `to_raw()` is consumed by `report.rs`, `Config::to_toml()`, and
    // `download/mod.rs::compute_config_hash()`. A swapped `included`/
    // `excluded` order or a missing `format!("!{n}")` prefix would land
    // green if we only round-trip the all-sentinel case (the most common
    // fixture). Pin the named-with-excluded variants so a regression in
    // either prefix or ordering surfaces.

    #[test]
    fn selection_album_to_raw_round_trips_named_with_excludes() {
        // Arrange
        let original = AlbumSelector::Named {
            included: set(&["Vacation", "Wedding"]),
            excluded: set(&["Drafts", "Family"]),
        };

        // Act
        let raw = original.to_raw();
        let parsed = parse_album_selector(&raw, false).unwrap();

        // Assert: round-trip is a fixed point. Also pin the on-the-wire
        // shape so a future "!" prefix drop fails here even before the
        // re-parse runs.
        assert_eq!(parsed, original);
        assert!(
            raw.iter().any(|r| r == "!Drafts"),
            "exclusion prefix '!' must survive serialization; got {raw:?}"
        );
        assert!(
            raw.iter().any(|r| r == "!Family"),
            "exclusion prefix '!' must survive serialization; got {raw:?}"
        );
    }

    #[test]
    fn selection_album_to_raw_round_trips_all_with_excludes() {
        let original = AlbumSelector::All {
            excluded: set(&["Family"]),
        };
        let raw = original.to_raw();
        let parsed = parse_album_selector(&raw, false).unwrap();
        assert_eq!(parsed, original);
        assert_eq!(raw, vec!["all".to_string(), "!Family".to_string()]);
    }

    #[test]
    fn selection_smart_folder_to_raw_round_trips_named_with_excludes() {
        let original = SmartFolderSelector::Named {
            included: set(&["Favorites", "Videos"]),
            excluded: set(&["Hidden"]),
        };
        let raw = original.to_raw();
        let parsed = parse_smart_folder_selector(&raw).unwrap();
        assert_eq!(parsed, original);
        assert!(raw.iter().any(|r| r == "!Hidden"));
    }

    #[test]
    fn selection_smart_folder_to_raw_round_trips_all_with_sensitive() {
        let original = SmartFolderSelector::All {
            include_sensitive: true,
            excluded: set(&["Recently Deleted"]),
        };
        let raw = original.to_raw();
        let parsed = parse_smart_folder_selector(&raw).unwrap();
        assert_eq!(parsed, original);
        // The sensitive sentinel must survive verbatim.
        assert!(
            raw.iter().any(|r| r == "all-with-sensitive"),
            "all-with-sensitive sentinel must serialize verbatim; got {raw:?}"
        );
    }

    #[test]
    fn selection_library_to_raw_round_trips_named_with_excludes() {
        let original = LibrarySelector {
            primary: true,
            shared_all: false,
            named: set(&["SharedSync-A1B2C3D4"]),
            excluded: set(&["Foo"]),
        };
        let raw = original.to_raw();
        let parsed = parse_library_selector(&raw).unwrap();
        assert_eq!(parsed, original);
        assert!(raw.iter().any(|r| r == "!Foo"));
    }

    #[test]
    fn selection_library_all_with_exclusion_round_trips() {
        // `LibrarySelector::to_raw()` collapses to `all` when both
        // primary and shared_all are set with no named zones. A dropped
        // exclusion under that branch would silently change which zones
        // sync after a TOML round-trip.
        let original = LibrarySelector {
            primary: true,
            shared_all: true,
            named: BTreeSet::new(),
            excluded: set(&["Foo"]),
        };
        let raw = original.to_raw();
        let parsed = parse_library_selector(&raw).unwrap();
        assert_eq!(parsed, original);
        // Pin the wire shape so the `["all", "!Foo"]` collapsing branch
        // can't silently drop the `all` sentinel or reorder.
        assert_eq!(raw, vec!["all".to_string(), "!Foo".to_string()]);
    }

    #[test]
    fn selection_full_round_trip_via_build_selection() {
        // Take a non-default `Selection`, run every field through
        // `to_raw()`, feed back through `build_selection`, assert equal.
        // A future refactor that drops one `to_raw()` branch (e.g.
        // forgets `unfiled` because there's no `to_raw()` for it) lands
        // green without this.
        let original = Selection {
            albums: AlbumSelector::All {
                excluded: set(&["Family"]),
            },
            smart_folders: SmartFolderSelector::Named {
                included: set(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            libraries: LibrarySelector {
                primary: true,
                shared_all: true,
                named: BTreeSet::new(),
                excluded: BTreeSet::new(),
            },
            unfiled: false,
        };

        let raw_albums = original.albums.to_raw();
        let raw_smart = original.smart_folders.to_raw();
        let raw_lib = original.libraries.to_raw();

        let rebuilt =
            build_selection(&raw_albums, &raw_smart, &raw_lib, Some(original.unfiled)).unwrap();
        assert_eq!(rebuilt, original);
    }

    // ── Bare exclusion with default_to_all = false ────────────────────
    //
    // `parse_album_selector(["!Family"], false)` is not directly
    // tested. The implementation always returns `All { excluded }` for an
    // exclusion-only input regardless of `default_to_all`; pin that so a
    // future refactor that ties exclusions to `default_to_all` lands red.

    #[test]
    fn album_bare_exclusion_with_default_false_still_resolves_to_all() {
        let r = parse_album_selector(&s(&["!Family"]), false).unwrap();
        assert_eq!(
            r,
            AlbumSelector::All {
                excluded: set(&["Family"]),
            }
        );
    }

    // ── Unicode-whitespace boundary on parse_library_selector ─────────
    //
    // 3g boundary (adversarial): `entry.trim()` strips ASCII whitespace
    // but leaves Unicode whitespace like U+00A0 (NBSP) intact, so a
    // stray-NBSP-only entry would slip through as a literal zone name
    // and fail at the CloudKit boundary instead of at parse time.
    // "No silent failures" mandates the parse-time bail; if this test
    // fails, the production parser needs a `trim_matches(char::is_whitespace)`
    // (or equivalent) to match the spec.

    #[test]
    fn parse_library_selector_rejects_unicode_whitespace_only_entry() {
        // U+00A0 (NBSP) — a single-codepoint non-ASCII whitespace entry.
        let r = parse_library_selector(&s(&["\u{00A0}"]));
        assert!(
            r.is_err(),
            "single-NBSP entry must bail at parse time, got: {r:?}"
        );
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("must not be empty") || msg.contains("empty"),
            "error must name the empty-input cause; got: {msg}"
        );
    }
}
