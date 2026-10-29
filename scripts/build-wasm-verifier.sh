#!/usr/bin/env bash
# Builds the browser verifier (crates/verify-wasm) into an npm package directory: the WebAssembly
# module, the JavaScript glue and TypeScript types wasm-bindgen generates for it, package.json, and
# SHA256SUMS over all of them.
#
# The release build uses the pinned Rust toolchain and wasm-bindgen CLI below and maps the build's
# paths to fixed names, so a commit builds to the same bytes on any x86_64 Linux machine, from any
# checkout path, spaces included; CI builds it twice on separate runners, the second from another
# path with a space in it, and fails if the two differ. A build on another host is not checked and
# may differ. It fails if the module and glue together exceed 1.5 MB gzipped. `--dev` builds with the dev profile and the active toolchain instead, for the
# dev scenario only. The out-dir must be new or empty.
#
# Needs rustup with the pinned toolchain and its wasm32-unknown-unknown target
# (`rustup toolchain install 1.99.1 --profile minimal --target wasm32-unknown-unknown`), and
# wasm-bindgen-cli at the pinned version (`cargo install wasm-bindgen-cli --version 0.2.129 --locked`).
#
# Usage: scripts/build-wasm-verifier.sh [--dev] <out-dir>
set -euo pipefail

rust_toolchain=1.99.1
# The wasm-bindgen crate's version in Cargo.toml; the CLI must match it exactly.
wasm_bindgen_version=0.2.129
max_gzip_bytes=1500000

root="$(cd "$(dirname "$0")/.." && pwd)"
dev=false
if [[ "${1:-}" == --dev ]]; then
  dev=true
  shift
fi
out="${1:?usage: build-wasm-verifier.sh [--dev] <out-dir>}"
if [[ -e "$out" && -n "$(ls -A "$out")" ]]; then
  echo "$out is not empty: the package goes into a new or empty directory" >&2
  exit 1
fi

if [[ "$(wasm-bindgen --version)" != "wasm-bindgen $wasm_bindgen_version" ]]; then
  echo "wasm-bindgen $wasm_bindgen_version is required: cargo install wasm-bindgen-cli --version $wasm_bindgen_version --locked" >&2
  exit 1
fi

target_dir="$root/target/wasm-verifier"
if $dev; then
  cargo=(cargo)
  profile=dev
  built="$target_dir/wasm32-unknown-unknown/debug/engine_verify_wasm.wasm"
else
  cargo=(cargo "+$rust_toolchain")
  profile=wasm
  built="$target_dir/wasm32-unknown-unknown/wasm/engine_verify_wasm.wasm"
  # Panic messages carry source paths; these names keep the checkout and registry locations out.
  # CARGO_ENCODED_RUSTFLAGS separates flags with 0x1f, so a path with a space stays one flag (RUSTFLAGS
  # splits on whitespace), and it takes precedence over any RUSTFLAGS in the caller's environment.
  unit_sep=$'\x1f'
  export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$root=/engine${unit_sep}--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
fi
"${cargo[@]}" build --manifest-path "$root/Cargo.toml" -p engine-verify-wasm --locked \
  --target wasm32-unknown-unknown --profile "$profile" --target-dir "$target_dir"

mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" --out-name engine_verify \
  --remove-name-section --remove-producers-section "$built"

version="$(sed -n '/^\[workspace.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' "$root/Cargo.toml")"
cat > "$out/package.json" <<JSON
{
  "name": "engine-verify",
  "version": "$version",
  "description": "The engine's verifier for browsers: attestations, signed responses, and signed stream frames",
  "license": "Apache-2.0",
  "private": true,
  "type": "module",
  "main": "engine_verify.js",
  "types": "engine_verify.d.ts",
  "files": ["engine_verify_bg.wasm", "engine_verify_bg.wasm.d.ts", "engine_verify.js", "engine_verify.d.ts"],
  "sideEffects": false
}
JSON

files=(engine_verify_bg.wasm engine_verify_bg.wasm.d.ts engine_verify.js engine_verify.d.ts package.json)
if command -v sha256sum >/dev/null; then
  (cd "$out" && sha256sum "${files[@]}" > SHA256SUMS)
else
  (cd "$out" && shasum -a 256 "${files[@]}" > SHA256SUMS)
fi

gzipped=$(( $(gzip -9 -n -c "$out/engine_verify_bg.wasm" | wc -c) + $(gzip -9 -n -c "$out/engine_verify.js" | wc -c) ))
echo "engine-verify $version ($profile): $gzipped bytes gzipped"
cat "$out/SHA256SUMS"
if ! $dev && (( gzipped > max_gzip_bytes )); then
  echo "the module and glue are $gzipped bytes gzipped, over the $max_gzip_bytes limit" >&2
  exit 1
fi
