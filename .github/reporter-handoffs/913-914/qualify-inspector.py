"""Provider-free synthetic SQLite qualification for the exact shipped inspector."""

import argparse
import hashlib
import importlib.util
import json
import sqlite3
import sys
from pathlib import Path


def qualify(companion_root: Path, fixture_root: Path) -> dict:
    if fixture_root.exists() or not fixture_root.is_absolute():
        raise ValueError("Use a new absolute disposable fixture root")
    fixture_root.mkdir(parents=True)
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("reporter_inspector", companion_root / "inspect-state.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    assert module.inspect(fixture_root)["asset_rows"] == []
    database = fixture_root / "account-v1-fixture.db"
    writer = sqlite3.connect(database)
    try:
        writer.execute("PRAGMA journal_mode=WAL")
        writer.executescript("""
        PRAGMA user_version=34;
        CREATE TABLE assets (id TEXT, status TEXT);
        CREATE TABLE asset_master_mappings (asset_record_name TEXT, master_record_name TEXT);
        CREATE TABLE legacy_master_state_owners (master_record_name TEXT, asset_record_name TEXT);
        CREATE TABLE owned_temp_files (path BLOB, claimed_at INTEGER);
        """)
        writer.execute("INSERT INTO assets VALUES (?, ?)", ("unrelated-child", "downloaded"))
        writer.execute("INSERT INTO asset_master_mappings VALUES (?, ?)", ("unrelated-child", "different-master"))
        writer.execute("INSERT INTO owned_temp_files VALUES (?, ?)", (b"\x00\xffpath", 123))
        writer.commit()
        wal = Path(str(database) + "-wal")
        before = [hashlib.sha256(path.read_bytes()).hexdigest() for path in (database, wal)]
        real_connect = sqlite3.connect
        readonly_opens = []

        def verified_connect(*args, **kwargs):
            assert kwargs.get("uri") is True and args[0].endswith("?mode=ro")
            connection = real_connect(*args, **kwargs)
            try:
                connection.execute("CREATE TABLE forbidden_write (value INTEGER)")
            except sqlite3.OperationalError as error:
                assert "readonly" in str(error).lower()
            else:
                connection.close()
                raise AssertionError("Inspector opened a writable database")
            readonly_opens.append(True)
            return connection

        # Instrument only the synthetic inspector's opening contract. The writer
        # stays open to preserve committed WAL content, not to model live-user DBs.
        module.sqlite3.connect = verified_connect
        try:
            report = module.inspect(fixture_root)
            assert report["schema"] == 34 and report["integrity"] == ["ok"]
            assert report["asset_rows"] == [{"id": "unrelated-child", "status": "downloaded"}]
            assert report["mappings"][0]["master_record_name"] == "different-master"
            assert report["legacy_owners"] == []
            assert report["temp_claims"][0]["path"] == {"blob_hex": "00ff70617468"}
            json.dumps(report)
            assert before == [hashlib.sha256(path.read_bytes()).hexdigest() for path in (database, wal)]
            writer.close()
            assert module.inspect(fixture_root)["asset_rows"] == report["asset_rows"]
            assert len(readonly_opens) == 2
        finally:
            module.sqlite3.connect = real_connect
        second = fixture_root / "account-v1-another.db"
        second.touch()
        try:
            module.inspect(fixture_root)
        except ValueError:
            pass
        else:
            raise AssertionError("Multiple databases were accepted")
    finally:
        writer.close()
    return {"result": "passed", "python": sys.version, "sqlite": sqlite3.sqlite_version,
            "provider_operations": 0,
            "checks": ["empty state", "read-only URI/write refusal", "schema/integrity", "committed WAL",
                       "child/master/owner rows", "BLOB JSON", "unchanged DB/WAL contents", "reopen", "multiple DB refusal"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("companion_root", type=Path)
    parser.add_argument("fixture_root", type=Path)
    parser.add_argument("proof_path", type=Path)
    args = parser.parse_args()
    result = qualify(args.companion_root.resolve(strict=True), args.fixture_root)
    args.proof_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
