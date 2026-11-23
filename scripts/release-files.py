"""Writes a release's configurations and manifest into <out-dir>, for .github/workflows/release-build.yaml.

Each configuration under config/release/, production's, staging's and any other deployment's, is
written as its own name, followed by config/benchmark-publishers.toml and config/production-genesis.toml.
The manifest, release.toml, is the format-2 manifest `engine verify-attestation` reads: the image's
launch digest, policy and guest features from its measurement.json, the production configuration's
[[tee.cpu_models]], and the SHA-256 of each configuration it writes.

It refuses a measurement of an image other than the production variant, or one whose guest features
lack SNPActive and SecureTSC, and configurations whose [[tee.cpu_models]] differ, since a verifier and a
peer enclave must take the same models at the same floors. With --final, for a tag, it also refuses a
configuration that still carries an owner or replace marker; without it, it prints each as a warning.
crates/node/tests/release_files.rs runs it and loads what it writes. It needs Python 3.11 or later, for
tomllib.

Usage: python3 -I scripts/release-files.py [--final] [--repo <dir>] <name> <measurement.json> <out-dir>
"""

import argparse
import hashlib
import json
import re
import sys
import tomllib
from pathlib import Path

APPENDED = ("config/benchmark-publishers.toml", "config/production-genesis.toml")
MARKERS = ("# OWNER", "# REPLACE")
# SNPActive (bit 0) and SecureTSC (bit 9), which a verifier requires.
REQUIRED_FEATURES = 0x201


def fail(message):
    sys.exit(f"release-files: {message}")


def hex_field(measurement, key, digits=None):
    value = measurement.get(key)
    if not isinstance(value, str) or not re.fullmatch(r"(0x)?[0-9a-fA-F]+", value):
        fail(f"measurement.json's {key} is not hex: {value!r}")
    if digits is not None and len(value.removeprefix("0x")) != digits:
        fail(f"measurement.json's {key} is not {digits // 2} bytes: {value!r}")
    return value


def tcb_value(value):
    return "0" if value == 0 else f"0x{value:02X}"


def cpu_model(model):
    if not re.fullmatch(r"[A-Za-z0-9-]+", model["product_name"]):
        fail(f"an unexpected product name: {model['product_name']!r}")
    tcb = model["minimum_tcb"]
    floor = ", ".join(f"{part} = {tcb_value(tcb[part])}" for part in ("boot_loader", "tee", "snp", "microcode"))
    return (
        "[[cpu_models]]\n"
        f'product_name = "{model["product_name"]}"\n'
        f"family = 0x{model['family']:02X}\n"
        f"model = 0x{model['model']:02X}\n"
        f"stepping = 0x{model['stepping']:02X}\n"
        f"minimum_tcb = {{ {floor} }}\n"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--final", action="store_true", help="refuse any marker left in a configuration")
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("name")
    parser.add_argument("measurement", type=Path)
    parser.add_argument("out", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9._-]+", args.name):
        fail(f"a release name is letters, digits, dots, dashes and underscores: {args.name!r}")

    measurement = json.loads(args.measurement.read_text())
    if measurement.get("variant") != "production":
        fail(f"measurement.json is of the {measurement.get('variant')!r} image, not the production one")
    digest = hex_field(measurement, "launch_digest", 96)
    policy = hex_field(measurement, "policy")
    features = hex_field(measurement, "guest_features")
    if int(features, 16) & REQUIRED_FEATURES != REQUIRED_FEATURES:
        fail(f"guest features {features} lack SNPActive and SecureTSC (0x201)")

    environments = sorted(path.stem for path in (args.repo / "config/release").glob("*.toml"))
    if "production" not in environments:
        fail("config/release/ holds no production.toml, whose [[tee.cpu_models]] the manifest takes")
    args.out.mkdir(parents=True, exist_ok=True)
    hashes, models, markers = {}, {}, []
    for environment in environments:
        parts = [f"config/release/{environment}.toml", *APPENDED]
        text = b"".join((args.repo / part).read_bytes() for part in parts)
        file = f"{environment}.toml"
        (args.out / file).write_bytes(text)
        hashes[file] = hashlib.sha256(text).hexdigest()
        models[environment] = tomllib.loads(text.decode())["tee"]["cpu_models"]
        for number, line in enumerate(text.decode().splitlines(), 1):
            if any(marker in line for marker in MARKERS):
                markers.append(f"{file}:{number}: {line.strip()}")

    for environment, listed in models.items():
        if listed != models["production"]:
            fail(f"{environment}.toml's [[tee.cpu_models]] differ from production.toml's")
    if markers:
        if args.final:
            fail("a release names no configuration with a marker left:\n" + "\n".join(markers))
        # One workflow annotation for the run's summary, then each marker in the log.
        print(f"::warning::{len(markers)} markers are left in the configurations; a tag would be refused")
        for marker in markers:
            print(f"warning: a marker is left in {marker}", file=sys.stderr)

    manifest = (
        "format = 2\n"
        f'name = "{args.name}"\n'
        f'launch_digest = "0x{digest.removeprefix("0x")}"\n'
        f'policy = "{policy}"\n'
        f'guest_features = "{features}"\n'
        + "".join("\n" + cpu_model(model) for model in models["production"])
        + "\n[config_hashes]\n"
        + "".join(f'"{file}" = "0x{sha256}"\n' for file, sha256 in hashes.items())
    )
    (args.out / "release.toml").write_text(manifest)
    print(manifest, end="")


main()
