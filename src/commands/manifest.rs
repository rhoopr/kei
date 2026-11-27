#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print an export to stdout"
)]

use serde::Serialize;

use crate::cli;
use crate::config;
use crate::sync_core::state::ListedFile;

/// One local copy in the catalog.
#[derive(Debug, Serialize)]
struct ManifestRow {
    library: String,
    /// The photo's `CPLAsset` record name, or its master for a copy carried
    /// over from the previous state.
    asset_id: String,
    version: String,
    /// Relative to the download directory.
    local_path: String,
    /// Album record name, `smart:<name>`, or empty for the unfiled copy.
    album: String,
    size_bytes: u64,
    local_checksum: Option<String>,
    status: String,
    /// Whether the copy is still selected. Unselected copies keep their files.
    selected: bool,
}

impl From<ListedFile> for ManifestRow {
    fn from(file: ListedFile) -> Self {
        Self {
            library: file.zone,
            asset_id: file.asset,
            version: file.rendition,
            local_path: file.path,
            album: file.album,
            size_bytes: file.size,
            local_checksum: file.sha256,
            status: file.state.to_owned(),
            selected: file.wanted,
        }
    }
}

pub(crate) async fn run_manifest(
    args: cli::ManifestArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let (location, state) = super::core_state(globals, toml).await?;
    let Some(state) = state else {
        anyhow::bail!(
            "No state database found at {}. Run a sync first to create the local catalog.",
            location.current.display()
        );
    };
    let mut rows: Vec<ManifestRow> = state
        .list_files(&[])
        .await?
        .into_iter()
        .map(ManifestRow::from)
        .collect();
    rows.extend(
        state
            .list_imported()
            .await?
            .into_iter()
            .map(|copy| ManifestRow {
                library: copy.zone,
                asset_id: copy.master,
                version: copy.rendition,
                local_path: copy.path,
                album: String::new(),
                size_bytes: copy.size,
                local_checksum: copy.sha256,
                status: "carried_over".to_owned(),
                selected: true,
            }),
    );
    match args.format {
        cli::ManifestFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        cli::ManifestFormat::Csv => {
            print!("{}", render_csv(&rows));
        }
    }
    Ok(())
}

fn render_csv(rows: &[ManifestRow]) -> String {
    const HEADER: &[&str] = &[
        "library",
        "asset_id",
        "version",
        "local_path",
        "album",
        "size_bytes",
        "local_checksum",
        "status",
        "selected",
    ];
    let mut out = String::new();
    push_csv_record(&mut out, HEADER);
    for row in rows {
        let size = row.size_bytes.to_string();
        let selected = row.selected.to_string();
        push_csv_record(
            &mut out,
            &[
                &row.library,
                &row.asset_id,
                &row.version,
                &row.local_path,
                &row.album,
                &size,
                row.local_checksum.as_deref().unwrap_or(""),
                &row.status,
                &selected,
            ],
        );
    }
    out
}

fn push_csv_record(out: &mut String, fields: &[&str]) {
    for (idx, field) in fields.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        push_csv_field(out, field);
    }
    out.push('\n');
}

fn push_csv_field(out: &mut String, field: &str) {
    let must_quote = field
        .bytes()
        .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'));
    if !must_quote {
        out.push_str(field);
        return;
    }

    out.push('"');
    for ch in field.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_fields_that_need_it() {
        let rows = vec![ManifestRow {
            library: "PrimarySync".into(),
            asset_id: "A1".into(),
            version: "original".into(),
            local_path: "Trip, 2024/IMG \"1\".JPG".into(),
            album: String::new(),
            size_bytes: 3,
            local_checksum: None,
            status: "done".into(),
            selected: true,
        }];
        let csv = render_csv(&rows);
        let mut lines = csv.lines();
        assert_eq!(
            lines.next().unwrap(),
            "library,asset_id,version,local_path,album,size_bytes,local_checksum,status,selected"
        );
        assert_eq!(
            lines.next().unwrap(),
            "PrimarySync,A1,original,\"Trip, 2024/IMG \"\"1\"\".JPG\",,3,,done,true"
        );
    }
}
