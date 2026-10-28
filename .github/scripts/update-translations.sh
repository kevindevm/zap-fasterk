#!/usr/bin/env bash
# Updates the translation template and catalogs with fastframe-i18n's script,
# from the fastframe checkout Cargo already has. Pass --check to change nothing
# and fail if the template is out of date. Requires GNU gettext tools with Rust
# support; normal Cargo builds do not.
set -euo pipefail
cd "$(dirname "$0")/../.."
# Git Bash may see Scoop shims without their GNU gettext / Rust binaries.
# Prefer the configured Scoop/Cargo roots, then existing standard installations.
if command -v cygpath >/dev/null 2>&1; then
    for tool_dir in "${SCOOP:-$HOME/scoop}/apps/gettext/current/bin" \
        "${CARGO_HOME:-$HOME/.cargo}/bin" \
        /h/Packages/Scoop/apps/gettext/current/bin \
        /h/Packages/Scoop/persist/rustup/.cargo/bin; do
        tool_dir=$(cygpath -u "$tool_dir")
        if [[ -d "$tool_dir" ]]; then PATH="$tool_dir:$PATH"; fi
    done
    export PATH
fi
crate=$(cargo metadata --format-version 1 --locked | python3 -c 'import json,sys; from pathlib import Path; d=json.load(sys.stdin); print(next(str(Path(p["manifest_path"]).parent) for p in d["packages"] if p["name"] == "fastframe-i18n"))')
if command -v cygpath >/dev/null 2>&1; then
    crate=$(cygpath -u "$crate")
fi
exec "$crate/scripts/update-translations.sh" --package "Zap Fasterk" --domain zapfasterk \
    --bugs 'https://github.com/kevindevm/zap-fasterk/issues/new?template=translation.yml' \
    --keyword translated:2 --fuzzy-matching "$@"
