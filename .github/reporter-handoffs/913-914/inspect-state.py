"""Read closed scratch SQLite databases, with no cloud calls or SQL writes.

Optional Python 3 helper. Output contains private IDs/paths; retain locally.
Point at scratch data only. Compare logical rows, not SQLite file hashes.
"""

import argparse
import json
import sqlite3
from pathlib import Path


def inspect(data_dir: Path) -> dict:
    root = data_dir.resolve(strict=True)
    databases = sorted(root.glob("account-v1-*.db"))
    if len(databases) > 1:
        raise ValueError("Multiple state databases; confirm the intended account and inspect separately")
    if not databases:
        return {"schema": None, "asset_rows": [], "mappings": [], "legacy_owners": [], "temp_claims": []}
    database = databases[0]
    if database.is_symlink():
        raise ValueError("State database must be an ordinary scratch copy")
    # Do not use immutable=1: it can hide committed WAL content.
    connection = sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True)
    try:
        connection.execute("PRAGMA query_only = ON")
        names = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        connection.row_factory = sqlite3.Row

        def rows(table: str) -> list:
            if table not in names:
                return []
            return sorted(
                (
                    {key: {"blob_hex": value.hex()} if isinstance(value, bytes) else value
                     for key, value in dict(row).items()}
                    for row in connection.execute(f'SELECT * FROM "{table}"')
                ),
                key=lambda row: json.dumps(row, sort_keys=True),
            )

        return {
            "database": database.name,
            "schema": connection.execute("PRAGMA user_version").fetchone()[0],
            "integrity": [row[0] for row in connection.execute("PRAGMA integrity_check")],
            "metadata": rows("metadata"),
            "account_owner": rows("account_owner"),
            "asset_rows": rows("assets"),
            "mappings": rows("asset_master_mappings"),
            "legacy_owners": rows("legacy_master_state_owners"),
            "temp_claims": rows("owned_temp_files"),
        }
    finally:
        connection.close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scratch_data_dir", type=Path)
    args = parser.parse_args()
    print(json.dumps(inspect(args.scratch_data_dir), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
