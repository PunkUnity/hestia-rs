#!/usr/bin/env python3
"""Build or check a Hestia RS palette pack using Python's ZIP support."""

import argparse
import json
from pathlib import Path
import re
import sys
from zipfile import BadZipFile, ZIP_DEFLATED, ZIP_STORED, ZipFile, ZipInfo
from zlib import error as CompressionError


MANIFEST = "manifest.json"
MANIFEST_LIMIT = 32 * 1024
ZIP_LIMIT = 64 * 1024
ROLES = frozenset((
    "bg", "surface", "card", "card-hover", "border", "text", "muted", "faint",
    "accent", "accent-hover", "accent-pressed", "on-accent", "glow", "ok",
    "warn", "danger", "info", "scrim",
))
ALPHA_ROLES = {"glow", "scrim"}
PACK_ID = re.compile(r"[a-z0-9]+(?:[.-][a-z0-9]+)+")
RELEASE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
RGB = re.compile(r"#[0-9a-fA-F]{6}")
RGBA = re.compile(r"#[0-9a-fA-F]{8}")


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError(f"Duplicate JSON key {key!r}.")
        obj[key] = value
    return obj


def invalid_constant(value):
    raise ValueError(f"{value} is not valid JSON.")


def read_manifest(source):
    data = source.read(MANIFEST_LIMIT + 1)
    if len(data) > MANIFEST_LIMIT:
        raise ValueError("The manifest must be at most 32 KiB.")
    manifest = json.loads(
        data.decode("utf-8"), object_pairs_hook=unique_object, parse_constant=invalid_constant
    )
    if not isinstance(manifest, dict):
        raise ValueError("The manifest must be a JSON object.")
    version = manifest.get("format_version")
    if type(version) is not int or version != 1:
        raise ValueError("Set format_version to 1.")

    pack_id = manifest.get("id")
    if not isinstance(pack_id, str) or len(pack_id) > 64 or not PACK_ID.fullmatch(pack_id):
        raise ValueError("Use a lowercase pack ID such as your-name.harbor, up to 64 characters.")
    name = manifest.get("name")
    if not isinstance(name, str) or not name.strip() or len(name) > 80:
        raise ValueError("Give the theme a display name up to 80 characters.")
    release = manifest.get("version")
    if not isinstance(release, str) or not RELEASE.fullmatch(release):
        raise ValueError("Use a pack version such as 1.0.0.")
    if manifest.get("base") not in ("hearth", "daylight"):
        raise ValueError("Choose hearth or daylight as the base palette.")

    components = manifest.get("components")
    if not isinstance(components, dict) or components.keys() != {"palette"}:
        raise ValueError("Add one palette under components.")
    palette = components["palette"]
    if not isinstance(palette, dict) or not palette:
        raise ValueError("Set at least one color in the palette.")
    for role, color in palette.items():
        if role not in ROLES:
            raise ValueError(f"Unknown color role {role!r}.")
        if not isinstance(color, str) or not (
            RGB.fullmatch(color) or (role in ALPHA_ROLES and RGBA.fullmatch(color))
        ):
            raise ValueError(f"Invalid color for {role!r}. Use a hex color such as #127f86.")
    return manifest


def build(source, output):
    with source.open("rb") as contents:
        manifest = read_manifest(contents)
    output = output or Path(f"{manifest['id']}.zip")
    data = json.dumps(manifest, ensure_ascii=False, indent=2).encode("utf-8") + b"\n"
    if len(data) > MANIFEST_LIMIT:
        raise ValueError("The formatted manifest must be at most 32 KiB.")
    with ZipFile(output, "x") as archive:
        archive.writestr(ZipInfo(MANIFEST), data, compress_type=ZIP_DEFLATED)
    print(f"Created {output}")


def check(pack):
    with pack.open("rb") as source:
        if source.seek(0, 2) > ZIP_LIMIT:
            raise ValueError("A palette-only pack must be at most 64 KiB.")
        with ZipFile(source) as archive:
            entries = archive.infolist()
            if len(entries) != 1 or entries[0].filename != MANIFEST:
                raise ValueError("The ZIP must contain one manifest.json at its root.")
            entry = entries[0]
            if entry.flag_bits & 1 or entry.compress_type not in (ZIP_STORED, ZIP_DEFLATED):
                raise ValueError("Use standard ZIP compression without a password.")
            if entry.file_size > MANIFEST_LIMIT:
                raise ValueError("The manifest must be at most 32 KiB.")
            with archive.open(entry) as contents:
                manifest = read_manifest(contents)
    print(f"{manifest['name']} {manifest['version']} ({manifest['id']})")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build_parser = commands.add_parser("build", help="Build a ZIP from a manifest")
    build_parser.add_argument("manifest", type=Path)
    build_parser.add_argument("--output", "-o", type=Path, help="Output ZIP path")
    check_parser = commands.add_parser("check", help="Check a ZIP before sharing")
    check_parser.add_argument("pack", type=Path)
    args = parser.parse_args()

    try:
        if args.command == "build":
            build(args.manifest, args.output)
        else:
            check(args.pack)
    except (OSError, ValueError, BadZipFile, RuntimeError, CompressionError) as error:
        print(f"Theme pack: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
