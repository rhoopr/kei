# First backup with Docker

Start with the [compatibility and authentication prerequisites](../README.md#before-you-install).
For an existing archive, use the [migration preview](migration-from-icloudpd.md)
instead. These steps use Docker Compose on the Docker host and a fresh empty
photo directory. NAS users can create the same mounts and settings in their
container manager; see [Synology](https://github.com/rhoopr/kei/wiki/Synology).

## Prepare persistent storage

In a new working directory, create `config` and `photos` directories:

```sh
mkdir config photos
```

Create `config/config.toml`, replacing the account value:

```toml
[auth]
username = "you@example.com"

[download]
directory = "/photos"
```

Create `compose.yaml` beside those directories:

```yaml
services:
  kei:
    image: ghcr.io/rhoopr/kei:latest
    volumes:
      - ./config:/config
      - ./photos:/photos
```

`./photos` on the host is `/photos` inside the container. `/config` retains the
config, credentials, sessions, and SQLite state between container runs. Both
mounts must be writable and persistent. This minimal config selects the primary
library's user albums and unfiled media, with original renditions and Live Photo
companions. Edited and alternative/RAW renditions are opt-in.

The image runs as root unless both `PUID` and `PGID` are set. For NAS or shared
storage, add these under the service using the numeric IDs of the host user
who should own the files (`id <user>` shows them):

```yaml
    environment:
      PUID: "1000"
      PGID: "1000"
```

Replace the example IDs. With both set, startup recursively repairs ownership
on `/config` and `/photos`. For host-managed photo permissions, set
`KEI_CHOWN_PHOTOS: "0"` in that environment block and prepare writable photo
permissions yourself; `/config` is still repaired. Do not mount an unrelated
archive for this trial.

## Log in and try a small sync

Run these in order, before starting a background service:

```sh
docker compose pull
docker compose run --rm -it kei password set --config /config/config.toml
docker compose run --rm -it kei login --config /config/config.toml
docker compose run --rm -it kei sync --config /config/config.toml --recent 10
docker compose run --rm kei status --config /config/config.toml
```

Use your Apple Account password and the trusted-device six-digit 2FA code.
The password command stores an encrypted credential under `/config`; protect
that mount as sensitive data. [Credentials](https://github.com/rhoopr/kei/wiki/Credentials)
describes password files and external secret managers as alternatives.

The sync overrides the image's default service command and runs once because
this config has no watch interval. `--recent 10` limits asset candidates, not
output files. Open expected media under the host's `photos` directory and follow
[Check the result](../README.md#check-the-result). Inspect warnings and recorded
pending/failed work before expanding the selection. Preserve both mounts.

## Run unattended

After verifying the trial, review the saved selection and add this to
`config/config.toml`:

```toml
[watch]
interval = 86400
```

Add `restart: unless-stopped` and `stop_grace_period: 30s` under the `kei` service,
then start it:

```sh
docker compose up -d
docker compose logs -f kei
```

The image's default command is `kei service run --config /config/config.toml`.
It syncs the saved selection, without the trial's one-run recent limit, and
repeats daily. `service run` also uses a daily fallback if no interval is saved.
Apple may require 2FA again. For this Compose service, request and submit a code:

```sh
docker compose exec --user UID:GID kei kei login get-code --config /config/config.toml
docker compose exec --user UID:GID kei kei login submit-code 123456 --config /config/config.toml
```

Replace `UID:GID` with the numeric owner IDs used for `/config` (your configured
PUID/PGID, or `0:0` for the default root setup), and `123456` with the new code.
`exec` bypasses the entrypoint, so it needs the correct user explicitly. The
waiting service resumes from the saved session. See
[Authentication](https://github.com/rhoopr/kei/wiki/Authentication) for details.
Do not run a second sync against the same state while the service is active.

See [Docker reference](https://github.com/rhoopr/kei/wiki/Docker) for monitoring,
secrets, and version tags, and [Support export](support-export.md) for reviewed
offline diagnostics. Pin a release tag for intentional upgrades and read
[Upgrade and compatibility](v0.25.3-upgrade.md) before changing versions. Source
on `main`, Docker `latest`, and Homebrew can be at different versions.
