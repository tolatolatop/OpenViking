#!/usr/bin/env python3
"""Sync declared resources into OpenViking and maintain a lock file.

The script reads a desired-state TOML file, compares it with a lock file, runs
`ov add-resource <path> --to <uri> --wait` for changed resources, then writes
the successful state back to the lock file.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python < 3.11 fallback
    import tomli as tomllib  # type: ignore


DEFAULT_CONFIG = ".resources.toml"
DEFAULT_LOCK = ".resources.lock"
DEFAULT_EXCLUDES = {
    ".git",
    ".hg",
    ".svn",
    "__pycache__",
    ".DS_Store",
    "node_modules",
    "target",
    "dist",
    "build",
}


class SyncError(RuntimeError):
    pass


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def load_toml(path: Path, *, missing_ok: bool = False) -> dict[str, Any]:
    if missing_ok and not path.exists():
        return {}
    with path.open("rb") as f:
        data = tomllib.load(f)
    if not isinstance(data, dict):
        raise SyncError(f"{path} must contain a TOML table")
    return data


def quote_toml_string(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int | float):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if value is None:
        return '""'
    return quote_toml_string(str(value))


def dump_lock_toml(data: dict[str, Any]) -> str:
    lines: list[str] = []
    lines.append(f"version = {int(data.get('version', 1))}")
    if data.get("updated_at"):
        lines.append(f"updated_at = {toml_value(data['updated_at'])}")
    lines.append("")

    for resource in data.get("resource", []):
        lines.append("[[resource]]")
        for key, value in resource.items():
            if isinstance(value, dict):
                continue
            lines.append(f"{key} = {toml_value(value)}")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=str(path.parent))
    tmp_path = Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
        tmp_path.replace(path)
    finally:
        if tmp_path.exists():
            tmp_path.unlink()


def run(cmd: list[str], *, cwd: Path | None = None, dry_run: bool = False) -> str:
    printable = " ".join(cmd)
    if dry_run:
        print(f"[dry-run] {printable}")
        return ""
    proc = subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else None,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if proc.returncode != 0:
        raise SyncError(f"Command failed ({proc.returncode}): {printable}\n{proc.stdout}")
    return proc.stdout.strip()


def resource_list(data: dict[str, Any]) -> list[dict[str, Any]]:
    resources = data.get("resource", [])
    if resources is None:
        return []
    if not isinstance(resources, list):
        raise SyncError("Expected [[resource]] entries in config")
    for item in resources:
        if not isinstance(item, dict):
            raise SyncError("Every [[resource]] entry must be a table")
    return resources


def lock_by_name(lock_data: dict[str, Any]) -> dict[str, dict[str, Any]]:
    result: dict[str, dict[str, Any]] = {}
    for item in resource_list(lock_data):
        name = item.get("name")
        if isinstance(name, str) and name:
            result[name] = item
    return result


def require_str(resource: dict[str, Any], key: str) -> str:
    value = resource.get(key)
    if not isinstance(value, str) or not value.strip():
        name = resource.get("name", "<unnamed>")
        raise SyncError(f"resource {name}: missing required string field '{key}'")
    return value.strip()


def resolve_path(config_dir: Path, raw_path: str) -> Path:
    path = Path(raw_path).expanduser()
    if not path.is_absolute():
        path = config_dir / path
    return path.resolve()


def file_digest(path: Path) -> str:
    h = hashlib.sha256()
    h.update(path.name.encode("utf-8", "surrogateescape"))
    h.update(b"\0")
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def directory_hash(path: Path) -> str:
    if not path.exists():
        raise SyncError(f"Directory does not exist: {path}")
    if not path.is_dir():
        raise SyncError(f"Expected a directory: {path}")

    h = hashlib.sha256()
    for root, dirs, files in os.walk(path):
        dirs[:] = sorted(d for d in dirs if d not in DEFAULT_EXCLUDES)
        rel_root = Path(root).relative_to(path)
        for filename in sorted(files):
            if filename in DEFAULT_EXCLUDES:
                continue
            file_path = Path(root) / filename
            rel_path = rel_root / filename
            h.update(str(rel_path.as_posix()).encode("utf-8", "surrogateescape"))
            h.update(b"\0")
            h.update(file_digest(file_path).encode("ascii"))
            h.update(b"\0")
    return "sha256:" + h.hexdigest()


def git_checkout(resource: dict[str, Any], temp_root: Path, *, dry_run: bool) -> Path:
    name = require_str(resource, "name")
    repo = require_str(resource, "repo")
    commit = resource.get("commit")
    ref = resource.get("ref")
    if not commit and not ref:
        raise SyncError(f"resource {name}: git resource needs 'commit' or 'ref'")

    checkout_dir = temp_root / name
    run(["git", "clone", "--no-checkout", repo, str(checkout_dir)], dry_run=dry_run)
    target = str(commit or ref)
    run(["git", "checkout", "--detach", target], cwd=checkout_dir, dry_run=dry_run)

    sub_path = resource.get("path", ".")
    if not isinstance(sub_path, str):
        raise SyncError(f"resource {name}: 'path' must be a string")
    local_path = (checkout_dir / sub_path).resolve()
    if not dry_run and not local_path.exists():
        raise SyncError(f"resource {name}: checked-out path does not exist: {local_path}")
    return local_path


def expected_state(resource: dict[str, Any], config_dir: Path, *, dry_run: bool) -> dict[str, Any]:
    name = require_str(resource, "name")
    resource_type = require_str(resource, "type")
    to_uri = require_str(resource, "to")

    base: dict[str, Any] = {
        "name": name,
        "type": resource_type,
        "to": to_uri,
    }

    if resource_type == "git":
        base["repo"] = require_str(resource, "repo")
        if resource.get("ref"):
            base["ref"] = str(resource["ref"])
        if resource.get("commit"):
            base["commit"] = str(resource["commit"])
        else:
            base["ref"] = str(resource["ref"])
        if resource.get("path"):
            base["path"] = str(resource["path"])
        return base

    if resource_type == "directory":
        raw_path = require_str(resource, "path")
        local_path = resolve_path(config_dir, raw_path)
        if not dry_run and not local_path.exists():
            raise SyncError(f"resource {name}: directory path does not exist: {local_path}")
        base["path"] = raw_path
        if resource.get("fingerprint"):
            base["fingerprint"] = str(resource["fingerprint"])
        if resource.get("timestamp"):
            base["timestamp"] = str(resource["timestamp"])
        if resource.get("content_hash"):
            base["content_hash"] = str(resource["content_hash"])
        elif not resource.get("fingerprint") and not resource.get("timestamp") and not dry_run:
            base["content_hash"] = directory_hash(local_path)
        return base

    raise SyncError(f"resource {name}: unsupported type '{resource_type}'")


def prepare_local_path(
    resource: dict[str, Any], config_dir: Path, temp_root: Path, *, dry_run: bool
) -> Path:
    resource_type = require_str(resource, "type")
    if resource_type == "git":
        return git_checkout(resource, temp_root, dry_run=dry_run)
    if resource_type == "directory":
        return resolve_path(config_dir, require_str(resource, "path"))
    raise SyncError(f"resource {resource.get('name', '<unnamed>')}: unsupported type '{resource_type}'")


def state_matches(expected: dict[str, Any], locked: dict[str, Any] | None) -> bool:
    if not locked or locked.get("status") != "success":
        return False
    keys = [
        "name",
        "type",
        "to",
        "repo",
        "ref",
        "commit",
        "path",
        "fingerprint",
        "timestamp",
        "content_hash",
    ]
    return all(locked.get(key) == expected.get(key) for key in keys if key in expected)


def ov_add_resource(
    ov_bin: str,
    local_path: Path,
    to_uri: str,
    resource: dict[str, Any],
    *,
    dry_run: bool,
) -> None:
    def option_value(value: Any) -> str:
        if isinstance(value, list):
            return ",".join(str(item) for item in value)
        return str(value)

    cmd = [ov_bin, "add-resource", str(local_path), "--to", to_uri, "--wait"]
    if resource.get("reason"):
        cmd += ["--reason", str(resource["reason"])]
    if resource.get("instruction"):
        cmd += ["--instruction", str(resource["instruction"])]
    if resource.get("ignore_dirs"):
        cmd += ["--ignore-dirs", option_value(resource["ignore_dirs"])]
    if resource.get("include"):
        cmd += ["--include", option_value(resource["include"])]
    if resource.get("exclude"):
        cmd += ["--exclude", option_value(resource["exclude"])]
    run(cmd, dry_run=dry_run)


def sync(args: argparse.Namespace) -> int:
    config_path = Path(args.config).resolve()
    lock_path = Path(args.lock).resolve()
    config_dir = config_path.parent

    config = load_toml(config_path)
    lock = load_toml(lock_path, missing_ok=True)
    locked = lock_by_name(lock)
    resources = resource_list(config)

    new_lock_resources: list[dict[str, Any]] = []
    changed_count = 0

    with tempfile.TemporaryDirectory(prefix="ov-resource-sync-") as tmp:
        temp_root = Path(tmp)
        for resource in resources:
            expected = expected_state(resource, config_dir, dry_run=args.dry_run)
            name = expected["name"]

            if state_matches(expected, locked.get(name)):
                print(f"[skip] {name}: already synced")
                new_lock_resources.append(locked[name])
                continue

            changed_count += 1
            local_path = prepare_local_path(resource, config_dir, temp_root, dry_run=args.dry_run)
            print(f"[sync] {name}: {local_path} -> {expected['to']}")
            ov_add_resource(args.ov_bin, local_path, expected["to"], resource, dry_run=args.dry_run)
            new_lock_resources.append(
                {
                    **expected,
                    "status": "success",
                    "synced_at": utc_now(),
                }
            )

    if args.dry_run:
        print(f"[dry-run] {changed_count} resource(s) would be synced")
        return 0

    new_lock = {
        "version": int(config.get("version", 1)),
        "updated_at": utc_now(),
        "resource": new_lock_resources,
    }
    atomic_write(lock_path, dump_lock_toml(new_lock))
    print(f"[done] synced={changed_count}, lock={lock_path}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Sync .resources.toml declarations into OpenViking and update .resources.lock."
    )
    parser.add_argument("--config", default=DEFAULT_CONFIG, help="resource config TOML path")
    parser.add_argument("--lock", default=DEFAULT_LOCK, help="resource lock TOML path")
    parser.add_argument("--ov-bin", default=shutil.which("ov") or "ov", help="ov CLI executable")
    parser.add_argument("--dry-run", action="store_true", help="print actions without syncing")
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        return sync(args)
    except SyncError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
