//! Folder-template defaults and token validation.

/// Default template for album passes: a flat per-album folder. Users opt
/// into a date hierarchy by passing `--folder-structure-albums "{album}/%Y..."`.
pub(crate) const DEFAULT_FOLDER_STRUCTURE_ALBUMS: &str = "{album}";

/// Default template for smart-folder passes: a flat per-smart-folder folder.
pub(crate) const DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS: &str = "{smart-folder}";

/// Which folder-structure flag a template was supplied via. Drives the
/// per-template token rules: each kind allows exactly one category token
/// (`{album}` for albums, `{smart-folder}` for smart folders, none for the
/// unfiled base) and `{library}` is allowed in all three.
///
/// See also [`crate::commands::PassKind`], which classifies the same three
/// categories at *render* time. The two enums look identical but encode
/// different rules: `TemplateKind::Unfiled` forbids category tokens.
#[derive(Debug, Clone, Copy)]
pub(super) enum TemplateKind {
    /// `--folder-structure-albums`.
    Albums,
    /// `--folder-structure-smart-folders`.
    SmartFolders,
    /// `--folder-structure` (unfiled / library-wide).
    Unfiled,
}

impl TemplateKind {
    fn flag_name(self) -> &'static str {
        match self {
            Self::Albums => "--folder-structure-albums",
            Self::SmartFolders => "--folder-structure-smart-folders",
            Self::Unfiled => "--folder-structure",
        }
    }

    /// The category token (if any) that this template scope owns. Used for
    /// placement rules and for rejecting category tokens in the unfiled
    /// template.
    fn category_token(self) -> Option<&'static str> {
        use crate::download::paths::{TOKEN_ALBUM, TOKEN_SMART_FOLDER};
        match self {
            Self::Albums => Some(TOKEN_ALBUM),
            Self::SmartFolders => Some(TOKEN_SMART_FOLDER),
            Self::Unfiled => None,
        }
    }
}

/// Cross-template token & placement validator. Enforces:
///
/// - `{album}` is only valid in `--folder-structure-albums`; same for
///   `{smart-folder}` and `--folder-structure-smart-folders`. The opposite
///   token in either category template, or *any* category token in the
///   unfiled template, bails with a pointer to the right flag.
/// - Single occurrence of every token ({album}, {smart-folder}, {library}).
/// - `{library}`, when present, must be the leading path segment.
/// - When `{library}` and the category token coexist, the category token
///   must immediately follow `{library}` (i.e. the second segment).
///
/// Bails at startup so misconfiguration surfaces before the first download.
pub(super) fn validate_template_tokens(
    folder_structure: &str,
    kind: TemplateKind,
) -> anyhow::Result<()> {
    use crate::download::paths::{TOKEN_ALBUM, TOKEN_LIBRARY, TOKEN_SMART_FOLDER};

    let stripped = crate::download::paths::strip_python_wrapper(folder_structure);
    let flag = kind.flag_name();
    let category = kind.category_token();

    // Reject category tokens that don't belong here.
    for (token, owner) in [
        (TOKEN_ALBUM, "--folder-structure-albums"),
        (TOKEN_SMART_FOLDER, "--folder-structure-smart-folders"),
    ] {
        if Some(token) == category {
            continue;
        }
        if stripped.contains(token) {
            anyhow::bail!(
                "`{token}` cannot be used in {flag}. Move it to {owner}. Template: \"{folder_structure}\""
            );
        }
    }

    // Single-occurrence checks for every token allowed in this kind.
    // `{library}` is always allowed; the category token is only allowed in
    // its owner kind.
    for token in [Some(TOKEN_LIBRARY), category].into_iter().flatten() {
        let count = stripped.matches(token).count();
        if count > 1 {
            anyhow::bail!(
                "`{token}` can appear only once in {flag}. Found {count} in \"{folder_structure}\"."
            );
        }
    }

    let segments: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
    let has_library = stripped.contains(TOKEN_LIBRARY);

    if has_library && segments.first() != Some(&TOKEN_LIBRARY) {
        anyhow::bail!(
            "`{TOKEN_LIBRARY}` must be the first path segment in {flag}. Template: \"{folder_structure}\""
        );
    }
    if let Some(cat) = category.filter(|c| stripped.contains(*c)) {
        let expected_index = if has_library { 1 } else { 0 };
        if segments.get(expected_index) != Some(&cat) {
            let position = if has_library {
                "must immediately follow '{library}'"
            } else {
                "must be the first path segment"
            };
            anyhow::bail!("`{cat}` {position} in {flag}. Template: \"{folder_structure}\"");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TemplateKind, validate_template_tokens};

    #[test]
    fn validate_template_tokens_accepts_default_per_category_templates() {
        validate_template_tokens("%Y/%m/%d", TemplateKind::Unfiled).unwrap();
        validate_template_tokens("{album}", TemplateKind::Albums).unwrap();
        validate_template_tokens("{smart-folder}", TemplateKind::SmartFolders).unwrap();
    }
    #[test]
    fn validate_template_tokens_accepts_library_prefix_in_every_kind() {
        validate_template_tokens("{library}/%Y/%m/%d", TemplateKind::Unfiled).unwrap();
        validate_template_tokens("{library}/{album}/%Y", TemplateKind::Albums).unwrap();
        validate_template_tokens("{library}/{smart-folder}", TemplateKind::SmartFolders).unwrap();
        // `{library}` standalone is fine (single-segment template).
        validate_template_tokens("{library}", TemplateKind::Unfiled).unwrap();
    }
    #[test]
    fn validate_template_tokens_rejects_misplaced_library_token() {
        let err = validate_template_tokens("%Y/{library}/%m", TemplateKind::Unfiled).unwrap_err();
        assert!(
            err.to_string()
                .contains("`{library}` must be the first path segment"),
            "{err}"
        );
        let err =
            validate_template_tokens("{album}/{library}/%Y", TemplateKind::Albums).unwrap_err();
        assert!(
            err.to_string().contains("`{library}` must be the first"),
            "{err}"
        );
    }
    #[test]
    fn validate_template_tokens_rejects_album_token_outside_album_template() {
        let err = validate_template_tokens("{album}/%Y", TemplateKind::Unfiled).unwrap_err();
        assert!(
            err.to_string().contains("--folder-structure-albums"),
            "{err}"
        );

        let err = validate_template_tokens("{album}", TemplateKind::SmartFolders).unwrap_err();
        assert!(
            err.to_string().contains("--folder-structure-albums"),
            "{err}"
        );
    }
    #[test]
    fn validate_template_tokens_rejects_smart_folder_token_outside_smart_folders_template() {
        let err = validate_template_tokens("{smart-folder}", TemplateKind::Unfiled).unwrap_err();
        assert!(
            err.to_string().contains("--folder-structure-smart-folders"),
            "{err}"
        );
        let err = validate_template_tokens("{smart-folder}", TemplateKind::Albums).unwrap_err();
        assert!(
            err.to_string().contains("--folder-structure-smart-folders"),
            "{err}"
        );
    }
    #[test]
    fn validate_template_tokens_rejects_duplicate_tokens() {
        let err = validate_template_tokens("{library}/{library}/{album}", TemplateKind::Albums)
            .unwrap_err();
        assert!(err.to_string().contains("can appear only once"), "{err}");

        let err = validate_template_tokens("{album}/{album}", TemplateKind::Albums).unwrap_err();
        assert!(err.to_string().contains("can appear only once"), "{err}");
    }
    #[test]
    fn validate_template_tokens_rejects_category_after_extra_segments() {
        // `{library}/%Y/{album}` puts `{album}` in segment 3, but the rule
        // is "immediately following `{library}`" — segment 2.
        let err =
            validate_template_tokens("{library}/%Y/{album}", TemplateKind::Albums).unwrap_err();
        assert!(
            err.to_string()
                .contains("must immediately follow '{library}'"),
            "{err}"
        );
    }
    #[test]
    fn validate_template_tokens_accepts_strftime_after_category_token() {
        // Date hierarchy *inside* the album folder is fine.
        validate_template_tokens("{album}/%Y/%m/%d", TemplateKind::Albums).unwrap();
        validate_template_tokens("{library}/{smart-folder}/%Y", TemplateKind::SmartFolders)
            .unwrap();
    }
    #[test]
    fn validate_template_tokens_handles_python_wrapper() {
        validate_template_tokens("{:%Y/%m/%d}", TemplateKind::Unfiled).unwrap();
        validate_template_tokens("{:{album}/%Y}", TemplateKind::Albums).unwrap();
    }
}
