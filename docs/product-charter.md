# kei product charter

kei is a reliability-first local backup engine for cloud-hosted photos and
videos.

Today kei backs up iCloud Photos. The product direction is broader: source
media from cloud services, keep a useful local catalog, and write files or
exports that users can trust.

## Product promise

kei copies photo and video libraries into storage the user controls, with safe
file writes, resumable sync, and enough state to explain what happened.

The default product promise is local backup, not photo management. kei should
make a local archive safer and easier to operate before it grows into
destructive cleanup, provider-to-provider sync, or downstream upload workflows.

## Who kei is for

- People backing up iCloud Photos to a local disk, NAS, server, or external
  drive.
- Docker, systemd, launchd, Windows service, and other headless users.
- Large-library users who need retryable sync, progress, reports, and clear
  failure states.
- People migrating from `icloudpd` or adopting an existing local archive.
- Self-hosted photo users who need a dependable local source for tools such as
  Immich.

## Product principles

- User data is sacred. kei must not lose, corrupt, truncate, overwrite, or
  silently discard media or metadata.
- Local file changes are opt-in. Capturing metadata in the state database is
  safe by default. Rewriting, deleting, or pruning local files needs an explicit
  user action.
- Sync should be explainable. A user should be able to tell what kei scanned,
  what it downloaded, what failed, and whether the next run can continue safely.
- Headless operation is first-class. Watch mode, Docker, service managers,
  health files, notifications, and reports are product surfaces, not afterthoughts.
- Provider quirks stay at provider boundaries. iCloud-specific behavior should
  not leak into the core model unless the risk is named and contained.
- The local state database should become a supported catalog. It already tracks
  useful truth about assets, files, checksums, albums, libraries, and sync
  history. Future commands should expose that truth directly.

## Current focus

The near-term requirement is stability and reliability. No new product surface
should be prioritized until normal sync, interrupted-run recovery, reporting,
retry behavior, token safety, and support paths feel rock solid.

Backup confidence is the user-visible proof of that reliability.

Users should be able to answer practical questions without reading logs:

- Is kei running?
- What is it working on?
- What is already backed up?
- What failed?
- Can the next run recover safely?
- Did sync tokens advance only when it was safe?
- Are any local files missing or damaged?
- What should I send when I need help?

This points first to hardening existing sync, recovery, status, reports, and
retry behavior. Support tools such as manifest export or a redacted diagnostic
bundle should come only when they help prove or debug reliability. Catalog query,
provider expansion, destination work, UI, and deletion workflows are later.

## Non-goals

- A full photo manager UI.
- Silent destructive sync.
- iCloud-side deletion bundled into normal backup.
- Chasing every inherited `icloudpd` flag when a clearer kei-native workflow is
  better.
- Treating private provider APIs as safe. If kei has to depend on them, the risk
  should be documented and guarded with tests.

## Roadmap source of truth

[GitHub milestones](https://github.com/rhoopr/kei/milestones) and their issues track committed release work. The `later` milestone groups deferred work without promising a release.
