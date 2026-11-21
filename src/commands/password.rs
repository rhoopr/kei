#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print credential-store status to stdout"
)]

use crate::cli;
use crate::config;
use crate::credential;
use crate::password::{self, ExposeSecret, SecretString};

/// Run the password subcommand: set, clear, or show backend.
pub(crate) fn run_password(
    action: cli::PasswordAction,
    globals: &config::GlobalArgs,
    pw: &cli::PasswordArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let (username, _password, domain, cookie_directory) = config::resolve_auth(globals, pw, toml)?;

    if username.is_empty() {
        anyhow::bail!(
            "Set your iCloud username with ICLOUD_USERNAME or [auth].username before managing a password."
        );
    }

    let store = credential::CredentialStore::new(&username, &cookie_directory, domain.as_str());

    match action {
        cli::PasswordAction::Set => {
            let input =
                password_set_input(pw, toml, input_mode, globals.config_source.recovery_path())?;
            let backend = store.store(input.expose_secret())?;
            println!("Password stored in {} backend.", backend.as_str());
        }
        cli::PasswordAction::Clear => {
            store.delete()?;
            println!("Stored credential removed.");
        }
        cli::PasswordAction::Backend => {
            println!("{}", store.backend_name());
        }
    }
    Ok(())
}

fn password_set_input(
    pw: &cli::PasswordArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<SecretString> {
    let toml_auth = toml.and_then(|config| config.auth.as_ref());
    if let Some(command) = config::resolve_password_command(pw, toml_auth) {
        return password::run_password_command(&command);
    }
    if let Some(path) = config::resolve_password_file(pw, toml_auth) {
        return password::read_password_file(&path);
    }

    if !input_mode.can_prompt() {
        anyhow::bail!(
            "Cannot prompt because stdin is not a terminal. Use `{}` or `{}`.",
            cli::command_for_config("password --password-file <PATH> set", config_path),
            cli::command_for_config("password --password-command <COMMAND> set", config_path),
        );
    }

    let input = rpassword::prompt_password("iCloud Password: ")
        .map_err(|e| anyhow::anyhow!("Could not read password: {e}"))?;
    anyhow::ensure!(!input.is_empty(), "Password cannot be empty.");
    Ok(SecretString::from(input))
}
