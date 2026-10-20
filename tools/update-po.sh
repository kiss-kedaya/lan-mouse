#!/bin/sh
# Adds new translatable texts to po/zh_CN.po and reports the ones that still
# need a translation. Existing translations are kept, matched by their English
# text. Run from anywhere, needs a Rust toolchain.
set -eu

cd "$(dirname "$0")/.."

# extracted tools are built on the machine, like cargo-bundle or cargo-edit
export CARGO_TARGET_DIR=target/i18n-extract
report=$(cargo run --quiet --release --manifest-path tools/i18n-extract/Cargo.toml -- .)
echo "$report"
if ! echo "$report" | head -1 | grep -q ' 0 without a translation$'; then
	echo "po/zh_CN.po: translate the texts above (msgstr stays empty for them)" >&2
	exit 1
fi
