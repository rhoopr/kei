# Credential storage

kei stores your iCloud password so it can re-authenticate without prompting. There are several backends, each with different security properties.

## OS keyring (recommended)

When available, kei uses the platform's native credential store:

- macOS: Keychain
- Linux: Secret Service (D-Bus, e.g. GNOME Keyring)
- Windows: Credential Manager

Passwords are protected by your OS login credentials and hardware security where supported. This is the default on native installs.

```sh
kei password set    # stores interactively
kei password backend  # shows which backend is active
```

## Encrypted file (automatic fallback)

In environments without a keyring - Docker containers, headless servers, WSL without D-Bus - kei falls back to an AES-256-GCM encrypted file in the config directory.

This prevents casual exposure: the password won't show up in `grep`, `cat`, `docker inspect`, or accidental config backups. But the encryption key lives in the same directory as the ciphertext. Anyone with filesystem access to the config volume can decrypt it.

The fallback exists so `--save-password` works out of the box in containers without extra setup. If that tradeoff doesn't work for you, use one of the explicit options below.

## Explicit password sources

For tighter control, pass the password at runtime instead of storing it:

| Method | Flag | Use case |
|--------|------|----------|
| Environment variable | `ICLOUD_PASSWORD` | Simple, but visible in `docker inspect` |
| File | `--password-file /run/secrets/pw` | Docker secrets, mounted files |
| Shell command | `--password-command "op read ..."` | 1Password, Vault, pass, etc. |

`--password-command` is the strongest option for headless deployments. The password is fetched on demand, never written to disk, and held in memory only during authentication (then zeroized).

## Recommendations

- **Desktop/laptop**: Use the OS keyring (default). Run `kei password set`.
- **Docker**: Use `--password-command` with an external secret manager, or Docker secrets via `--password-file`. The encrypted file fallback is fine for personal use but don't rely on it for shared infrastructure.
- **Cron/systemd**: Same as Docker. `--password-file` or `--password-command` avoids storing secrets in service files.
