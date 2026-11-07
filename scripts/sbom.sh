#!/bin/sh
# Generate the CycloneDX 1.5 SBOMs that CI publishes and releases attach.
#
# The backend document covers every crate in Cargo.lock for the musl release
# target. The frontend document covers web/package.json (marked is vendored in
# web/vendor) and records the vendored file's SHA-256. Pass an output directory
# as the first argument; the default is dist/sbom.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out=${1:-"$root/dist/sbom"}
mkdir -p "$out"
out=$(CDPATH= cd -- "$out" && pwd)

version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
    echo "sbom: cannot read the package version from Cargo.toml" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1 || ! cargo cyclonedx --version >/dev/null 2>&1; then
    echo "sbom: cargo-cyclonedx is required: cargo install cargo-cyclonedx --version 0.5.10 --locked" >&2
    exit 1
fi
cyclonedx_version=$(cargo cyclonedx --version | awk '{print $NF}')
if [ "$cyclonedx_version" != "0.5.10" ]; then
    echo "sbom: cargo-cyclonedx 0.5.10 is required, found $cyclonedx_version" >&2
    exit 1
fi
if ! command -v npm >/dev/null 2>&1 || ! command -v node >/dev/null 2>&1; then
    echo "sbom: Node.js and npm are required for the frontend document" >&2
    exit 1
fi

frontend_version=$(node -e 'process.stdout.write(require(process.argv[1]).version)' "$root/web/package.json")
if [ "$frontend_version" != "$version" ]; then
    echo "sbom: web/package.json is $frontend_version but Cargo.toml is $version; keep them in sync" >&2
    exit 1
fi

echo "sbom: backend kanso $version (CycloneDX 1.5, x86_64-unknown-linux-musl)"
(
    cd "$root"
    rm -f kanso-backend.json
    cargo cyclonedx --format json --spec-version 1.5 \
        --target x86_64-unknown-linux-musl \
        --override-filename kanso-backend
    mv kanso-backend.json "$out/kanso-backend.cdx.json"
)

node -e '
const { readFileSync } = require("node:fs");
const bom = JSON.parse(readFileSync(process.argv[1], "utf8"));
const root = bom.metadata && bom.metadata.component;
if (bom.bomFormat !== "CycloneDX" || !root || root.name !== "kanso" ||
    !Array.isArray(bom.components) || bom.components.length === 0) {
  console.error("sbom: invalid backend CycloneDX document");
  process.exit(1);
}
' "$out/kanso-backend.cdx.json"

echo "sbom: frontend kanso-web $frontend_version (CycloneDX 1.5)"
raw="$out/kanso-frontend.raw.json"
if (
    cd "$root/web"
    npm sbom --sbom-format cyclonedx --package-lock-only --omit dev --sbom-type application > "$raw"
); then
    node "$root/scripts/sbom-frontend.mjs" "$root/web/vendor/marked.min.js" \
        < "$raw" > "$out/kanso-frontend.cdx.json"
    rm -f "$raw"
else
    status=$?
    rm -f "$raw"
    exit "$status"
fi

echo "sbom: wrote $out/kanso-backend.cdx.json and $out/kanso-frontend.cdx.json"
