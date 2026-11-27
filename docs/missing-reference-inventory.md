# Missing-reference inventory diagnostic

> **Retired in v0.30.** The sync engine no longer tracks missing parent references, and `kei doctor --missing-reference-inventory` now refuses with that explanation. This page describes the earlier releases that offered it.

`kei doctor --live --json --missing-reference-inventory` scans the qualified
private PrimarySync inventory and reports missing parent-reference shapes.
It downloads no media, opens no catalogue, saves no checkpoint and performs no
repair. The eight ambiguous metadata ownership cases are outside its scope.
This opt-in mode bypasses normal doctor checks and sync startup. Ordinary
`doctor`, `sync` and offline `support-export` retain their existing behavior.

The command needs existing cached authentication validated within ten minutes.
It copies only the three account-scoped cookie/session/cache files into private
temporary scratch, using at most 1 MiB per file. It rejects symlinks, hardlinks,
changes during copying, missing or malformed data and future/expired cache times.
It never runs a credential command, login, validation refresh or reauthentication.
An authentication failure stops the diagnostic. Original auth files stay unchanged.

Use an external hard timeout when running this potentially long inventory:

```sh
timeout --signal=TERM --kill-after=5s 30m \
  kei --config /config/config.toml doctor --live --json --missing-reference-inventory \
  > missing-reference-inventory.json
```

`timeout` above is the GNU utility. The built-in thirty-minute async deadline and
Ctrl-C handling cover requests and cancellation, but cannot interrupt synchronous
JSON parsing. External termination can leave private scratch files behind; use an
isolated disposable temporary directory or container. No media mount is needed.
The Docker image's default `service run` command does not invoke this diagnostic;
pass the explicit command instead. Its argv classifier skips wrapper ownership
traversal for this mode while retaining numeric privilege drop. Existing service/Homebrew invocations stay unchanged.

The scan permits at most 1,000 inventory pages, 200,000 received records, 64 MiB
per inventory response, 1 GiB charged response allowance, 256 MiB retained records,
three discovery requests and one targeted lookup. HTTP redirects, protocol retries,
application retries and compressed responses are disabled/refused. Failed responses
retain their reserved byte allowance; completed bytes and outstanding reservations
are reported separately because partially received failed bodies are not measured.
The byte caps bound accepted response bodies, not HTTP/TLS framing or process RSS.

Output is fixed-category JSON, at most 64 KiB, with up to 32 groups and explicit
omitted-group/observation counts. It contains no record names or their hashes,
account identifiers, tokens, paths, filenames, checksums or provider error text.
Only one deterministically selected missing-reference identity may be looked up.
That lookup's result cannot be extrapolated to every group.

`scan: eof_observed` means the inventory reached a validated EOF, not that sync
acceptance or ownership was established. History describes occurrences within this
scan. A scoped reference returned by the later lookup cannot repair evidence at
the inventory's earlier cursor. Missing and foreign scope remain distinct from
payload shape. No catalogue family membership is claimed.

Incomplete input, discovery, scan, budget or cancellation results exit nonzero.
Inspect `scan`, `reason` and `transport.stop_reason`; do not infer zero missing
references from an incomplete result. EOF output exits successfully even if its
single lookup was unavailable. JSON is emitted to stdout without an automatic
upload or output-file overwrite guard. Review it locally before sharing it.
