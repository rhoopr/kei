# Windows scratch validation for #913 and #914

**Local review draft. The published guide stays immutable at its reviewed commit.** The intended test build is `0.24.2-dev`, source `386e68cdb0b2302933946ba821327f616f73ce81`, schema 34. It includes the merged fixes. It changes account-state/authentication handling from your schema-28 `0.24.1`; please keep the installed binary and normal setup untouched.

Obtain the qualified run/download link, artifact ID/digest, full ZIP/executable SHA-256 values, and exact-commit companion links/hashes from the accompanying maintainer comment. Do not run without those verified details. They are supplied separately after the build; do not edit this committed guide to insert runtime values or its own hash.

The diagnostic build and helper are unsigned. If SmartScreen, Defender or PowerShell policy blocks them, stop and report the warning; do not disable protections or change ExecutionPolicy. Maintainer handoff requires the offline Windows PowerShell 5.1/7 and inspector gates to pass. Those synthetic checks do not replace validation of your actual Windows/NTFS library.

## Prepare independent copies

1. Stop your normal kei processes and scheduled/service runs before taking a consistent backup of config, state/session files, all SQLite companions (`-wal`, `-shm`, journals if present) and media. Keep that cold snapshot unchanged. Use ordinary independent copies for tests, with enough free space for all media/state copies and downloads. Avoid hard links, junctions, symlinks and cloud placeholders; use a local NTFS scratch tree outside production and snapshot directories. Never run the test executable with a production or snapshot path. Do not replace your installed executable or register a test service.
2. Record Windows build, NTFS and OS/process architecture. The package is x86_64, matching the existing Windows release. If your machine is ARM64, report that x64 emulation is involved; this does not test native ARM64. In Windows PowerShell: `[Environment]::OSVersion.VersionString; Get-CimInstance Win32_Processor | Select-Object Name,Architecture; $env:PROCESSOR_ARCHITECTURE; $env:PROCESSOR_ARCHITEW6432`. The CPU Architecture value is 9 for x64 or 12 for ARM64. [Microsoft documents these platform values](https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-processor). Stop if the binary cannot run normally; do not change security settings to force execution.
3. Download the named artifact from the approved run while logged into GitHub. Check the ZIP's full SHA-256 against the supplied value, extract into a new scratch `bin` directory, then check `kei.exe` against the independently supplied executable hash. Check `provenance.json` has the exact source above, target `x86_64-pc-windows-msvc`, and schema 34. `& $Exe --version` must print `kei 0.24.2-dev`; the version alone cannot identify this commit. The artifact expires after 14 days. If it is missing/expired or hashes differ, stop and request the same qualified build, rather than substituting another binary.
4. Open a separate PowerShell window for the tests. Clear inherited kei/account/password overrides without printing their values, so the new config is authoritative. This affects only that window. Select a new absolute scratch root, save the three companion plain text files there from the supplied exact-commit links, verify their supplied SHA-256 values, then dot-source the reviewed helper and enter your **exact** username/realm. If PowerShell policy blocks loading it, stop and tell us; do not change ExecutionPolicy/security settings to run it. Example paths below must be adjusted:

```powershell
Get-ChildItem Env: | Where-Object { $_.Name -like 'KEI_*' -or $_.Name -in @('ICLOUD_USERNAME','ICLOUD_PASSWORD','RUST_LOG') } |
    ForEach-Object { Remove-Item -LiteralPath ('Env:' + $_.Name) }
$TestRoot = 'D:\kei-test-386e68c'  # Outside production and cold snapshot; ordinary NTFS directory
$Exe = Join-Path $TestRoot 'bin\kei.exe'
. (Join-Path $TestRoot 'reporter-tools.ps1')
$ExpectedExeSHA256 = Read-Host 'Full executable SHA-256 from the qualified handoff'
Set-KeiTestBinary -Exe $Exe -ExpectedSHA256 $ExpectedExeSHA256
$Username = Read-Host 'Exact configured Apple account username'
$Realm = 'com'  # Use cn only if that is your actual account realm
```

Each helper-created case has its own config, state/auth/cache/health/locks, media, logs and process `TEMP`/`TMP`. The config explicitly disables all EXIF/XMP outputs and has no watch, server, notification, report or external password-command settings. Do not copy your entire live config over it. For #913, transfer only the exact account realm, filters, photo policies and three folder templates that produced the copied archive. Leave the absolute `data_dir` and media directory alone. Durable settings belong in TOML; current main has no global `--data-dir` flag. The helper also sets `KEI_DATA_DIR` for every command.

Use `Invoke-KeiCase` for each command below. It keeps combined logs and the actual exit code. A password may be requested interactively; complete 2FA on your trusted device. Do not put passwords or codes in public logs. No `password set`, `password clear`, reset, repair, reconcile, force-empty, or migration is needed for a **fresh** test case. Main ignores legacy auth files: do not copy or rename `0.24.1` sessions/credentials into its new account namespace. An existing exact-username keyring credential may be read, but these commands do not ask to change it. If authentication is rejected or throttled, stop that lane and report it separately; do not repeat logins aggressively.

## #913: affected archive, then unchanged sync

Create a fresh case, copy the cold snapshot's **media only** into its empty media directory, preserving relative folders/names, and adapt the permitted policy sections as above. Do not copy its database or legacy auth. Include Unfiled and `Hidden` if they are still the scope you reported. The helper defaults to Unfiled only; explicitly set `smart_folders = ['Hidden']` for your original Hidden pass and copy the original smart-folder template. Keep original rendition/RAW/Live Photo policies unchanged for this archive lane.

```powershell
$i = New-KeiCase -TestRoot $TestRoot -Name '913-archive-import' -Username $Username -Realm $Realm
# Copy only independent media into $i.Media; edit permitted policy settings now.
Get-KeiMediaInventory $i.Media | Export-Csv (Join-Path $i.Logs 'media-before.csv') -NoTypeInformation -ErrorAction Stop
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'login' -KeiArgs @('login')
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'dry-run' -KeiArgs @('import-existing','--dry-run','--no-progress-bar')
```

Record scanned, matched, unmatched, refused and hash-error counts, broken down by pass/rendition where available. We expect the child-derived name-id7 mismatch to be fixed; **6,041 matches is not guaranteed** if the cloud library, selected policies or files differ, or ownership remains ambiguous. Do not rename files to guessed master/child suffixes or disable guards. If this still matches zero, or paths look wrong, stop before real import and retain evidence.

Dry-run can initialize schema/account binding and refresh scratch auth; it must leave asset rows, child/master mappings and legacy-owner claims unchanged, and media hashes unchanged. Before the first dry-run, use the optional read-only state inspector described below (no DB is an empty asset/mapping/owner baseline), then compare after it. A second dry-run checks an already initialized database as well. Do not use SQLite file hashes to judge logical ownership writes.

```powershell
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'dry-run-repeat' -KeiArgs @('import-existing','--dry-run','--no-progress-bar')
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'import' -KeiArgs @('import-existing','--no-progress-bar')
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'status-import' -KeiArgs @('status')
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'manifest-import' -KeiArgs @('manifest','--format','json')
# Each invocation is a new process: the next command reopens the adopted state.
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'unchanged-sync' -KeiArgs @('sync','--no-progress-bar','--no-friendly')
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'status-reopen' -KeiArgs @('status','--failed')
Invoke-KeiCase -Case $i -Exe $Exe -RunLabel 'manifest-reopen' -KeiArgs @('manifest','--format','json')
Get-KeiMediaInventory $i.Media | Export-Csv (Join-Path $i.Logs 'media-after.csv') -NoTypeInformation -ErrorAction Stop
```

Compare adopted sample rows by `(library, asset_id, version)`, path, byte size and local SHA-256 with source mapping evidence. Fresh imported rows should use CPLAsset child identities and the sync-derived filenames, including collisions. The reopened sync should keep already adopted sample paths/hashes and downloaded status, with no duplicate rows, alternate paths or redownload of those files. Scan logs even if exit is 0. Newly selected/missing renditions or cloud changes are separate from re-downloading the adopted sample. For representative originals, Live Photo still/MOV, edited and alternative renditions, RAW+JPEG under each policy present in your library, and same-name/id7 collisions, record which cases are available and selected. Missing categories are untested, not passing.

For an additional **current sync → fresh import → reopen → unchanged sync** check, use a separate fresh case with empty media, sync a bounded representative scope, then copy only the resulting media into another new case with exactly the same scope/path/photo policies. Run that new case's dry-run/import/reopen sequence above. Use a separate pair of cases for each changed RAW or rendition policy. Do not reset or reuse an existing database to manufacture freshness. Collision/ambiguous names may intentionally stay unmatched; a refused/missing-file case must not gain a mapping or legacy-owner claim. Only examine these negative cases in extra disposable copies; do not edit SQL rows or cloud assets.

## #914: fresh concurrent twin runs

Repeat five times initially, serially, using unique `914-twins-01` through `914-twins-05` cases with **empty media and no DB**. Use a fresh main-format login for each. Keep `threads = 10`, no bandwidth cap, `%Y/%m`, `name-id7`, `albums = ['none']`, `smart_folders = ['none']`, original resolution and metadata outputs off. This preserves concurrent downloads; do not serialize the workers to hide the race. Retain every run, including failures. Use your existing golden archive copies for the full SHA-256 reference; the issue only contains a truncated hash.

```powershell
$t = New-KeiCase -TestRoot $TestRoot -Name '914-twins-01' -Username $Username -Realm $Realm
Invoke-KeiCase -Case $t -Exe $Exe -RunLabel 'login' -KeiArgs @('login')
Invoke-KeiCase -Case $t -Exe $Exe -RunLabel 'first-sync' -AllowFailure -KeiArgs @(
    'sync','--no-progress-bar','--no-friendly',
    '--skip-created-before','2026-03-23T21:22:55',
    '--skip-created-after','2026-03-23T21:23:05')
Get-KeiMediaInventory $t.Media | Export-Csv (Join-Path $t.Logs 'media-first.csv') -NoTypeInformation -ErrorAction Stop
Select-String -Path (Join-Path $t.Logs 'first-sync.log') -Pattern 'Download failed|Targeted retry failed|changed identity|Another kei process|os error 2|re-fetching URLs|retrying failed downloads'
Invoke-KeiCase -Case $t -Exe $Exe -RunLabel 'status-first' -KeiArgs @('status','--failed')
Invoke-KeiCase -Case $t -Exe $Exe -RunLabel 'manifest-first' -KeiArgs @('manifest','--format','json')
```

**A first-pass twin failure fails this check even when cleanup retries recover it, both files exist and exit is 0.** Confirm the selected window still includes the two distinct twins, both under `2026\03`, different name-id7 names, 481,897 bytes each, and SHA-256 equal to each other and to both golden copies. The old summary `2 downloaded, 34 skipped, 0 failed (36 total)` is evidence of the old harness, not a required new total. Inspect all error/retry lines, not just the sample patterns above.

After each successful first run, run another sync with those same date arguments to reopen state; it should preserve both hashes/paths without re-downloading either twin. Check two distinct downloaded rows, no failed/pending twin rows, no owned-temp claims after completion, and no `.kei-tmp` or other unexpected part files anywhere under that case's media/temp directories. Retain directories as evidence; do not manually delete orphan temp files to produce a clean result. Stop and report a first-pass failure, unexpected path/hash, authentication problem or cloud change. If five runs pass, additional batches can be agreed without losing the original five results or hammering login/2FA.

## State evidence, optional migration, and return to normal use

The optional `inspect-state.py` requires an already available Python 3; no installation is assumed. Run it only after all commands using that scratch case have exited, with output under its logs, for example `python inspect-state.py <absolute scratch data path> > <absolute scratch log path>`. Use it before/after dry-runs, after adoption, and after each reopened sync. Check schema 34, integrity `ok`, logical asset IDs/versions/paths/status/checksums, child/master mappings, guarded legacy-owner rows and empty temp claims. Its output contains private IDs/paths. If Python is unavailable, the same named tables can be inspected with your existing SQLite viewer in read-only mode; never issue SQL writes. `status` and `manifest` provide row evidence but cannot alone prove mapping/ownership invariants. Keep SQLite companions together for inspection; do not use `immutable=1` to hide WAL content.

If you also want to validate upgrading schema-28 state, use a **separate optional lane**, not the fresh import/twin cases. First independently confirm that the entire legacy DB belongs to the exact configured account/realm. Copy the cold DB and companions into a disposable migration-source directory, with a new destination `data_dir`, then run `migrate-state --legacy-db <that disposable DB> --confirm-ownership` using that lane's scratch config. It performs a fresh password login and may require 2FA (`--code` is supported for adoption). Inspect status/manifest and schema/integrity, and compare the preserved source copy's hashes before/after. Do not rename auth files, overwrite an existing destination, infer mixed ownership, or attempt a schema downgrade. **Migrated rows retain old absolute media paths: do not run sync, import, verify, repair, reconcile or resets against that migrated state until every retained media/metadata/reconciliation path has a separately approved safe isolation plan.** This handoff only validates its adoption/status/export; it does not edit stored paths.

For rollback, stop the test process and close the dedicated PowerShell window to drop its environment overrides. Preserve all test evidence. Your installed `0.24.1`, production config/state/media and untouched cold snapshot remain separate; resume the normal setup only with its original binary and original paths, never with migrated/test DBs or scratch sessions. If anything reached a production path, stop and report it before resuming or restoring over evidence.

Please return binary/source/run identity, Windows/NTFS/architecture, sanitized policy settings, per-case counts/exit codes, availability of each rendition category, first-pass error/retry results, twin sizes/full-hash comparisons, state/mapping/owner/temp checks, and whether originals/snapshot stayed unchanged. Keep passwords, 2FA codes, cookies/sessions, URLs with tokens, full manifests/SQLite exports and private filenames/IDs out of public comments; share only redacted samples/summary. The full-library pending behavior involving #770 remains a separate investigation.
