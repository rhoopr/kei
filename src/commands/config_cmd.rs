#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print the resolved config to stdout"
)]

use crate::cli;
use crate::config;

/// Run the config show command: dump resolved config as TOML.
pub(crate) fn run_config_show(
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let cfg = config::Config::build(
        globals,
        &cli::PasswordArgs::default(),
        cli::SyncArgs::default(),
        toml,
    )?;
    let toml_config = cfg.to_toml();
    let output = toml::to_string_pretty(&toml_config)
        .map_err(|e| anyhow::anyhow!("failed to serialize config: {e}"))?;
    print!("{output}");
    Ok(())
}
