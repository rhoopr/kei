//! Exact startup input retained privately for pre-migration recovery.

use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Default)]
pub(crate) enum ConfigSource {
    #[default]
    Unavailable,
    File {
        path: PathBuf,
        bytes: Arc<[u8]>,
    },
    Absent {
        path: PathBuf,
        explicitly_selected: bool,
    },
}

impl std::fmt::Debug for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "ConfigSource::Unavailable",
            Self::File { .. } => "ConfigSource::File(<private>)",
            Self::Absent { .. } => "ConfigSource::Absent(<private>)",
        })
    }
}

impl ConfigSource {
    pub(crate) fn recovery_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::File { path, .. }
            | Self::Absent {
                path,
                explicitly_selected: true,
            } => Some(path),
            Self::Absent { .. } | Self::Unavailable => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigSource;

    #[test]
    fn startup_config_source_debug_never_exposes_bytes_or_paths() {
        let source = ConfigSource::File {
            path: "private-account/config.toml".into(),
            bytes: std::sync::Arc::from(&b"password=private-secret"[..]),
        };
        assert_eq!(format!("{source:?}"), "ConfigSource::File(<private>)");
        assert_eq!(
            format!(
                "{:?}",
                ConfigSource::Absent {
                    path: "private-account/config.toml".into(),
                    explicitly_selected: false,
                }
            ),
            "ConfigSource::Absent(<private>)"
        );
    }
}
