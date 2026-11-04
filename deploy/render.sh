#!/bin/sh
# Render the Kanso deployment artefacts from a host-level, non-secret env file.
#
# The templates live next to this script and use __KANSO_*__ placeholders.
# The app's secret-bearing kanso.env is NOT read, written or accepted here.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
ENV_FILE="$SCRIPT_DIR/deploy.env"
OUT_DIR="$SCRIPT_DIR/out"
ENV_FILE_SET=0
OUT_DIR_SET=0

usage() {
    cat <<'EOF'
Usage: deploy/render.sh [--env FILE] [--out DIR] [FILE]

Render kanso.container, nginx.conf and kanso.env.example from a deployment env
file. The default env file is deploy/deploy.env; the default output directory
is deploy/out. A single positional FILE is accepted as the env file.

The env file holds host-level, non-secret values only. It is read as a plain
KEY=value list; unknown keys (including secrets) are rejected. Secrets belong
in the separate app kanso.env installed on the host. Set exactly one of
KANSO_TLS_INCLUDE (a host-managed include) or KANSO_TLS_SNIPPET (a snippet
embedded from a file; relative paths resolve from the repository root).

Options:
  --env FILE   deployment env file (default: deploy/deploy.env)
  --out DIR    output directory (default: deploy/out)
  -h, --help   show this help
EOF
}

die() {
    printf 'render: %s\n' "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --env)
            [ "$#" -ge 2 ] || die "--env needs a file"
            [ "$ENV_FILE_SET" -eq 0 ] || die "env file given twice"
            ENV_FILE=$2
            ENV_FILE_SET=1
            shift 2
            ;;
        --out)
            [ "$#" -ge 2 ] || die "--out needs a directory"
            [ "$OUT_DIR_SET" -eq 0 ] || die "output directory given twice"
            OUT_DIR=$2
            OUT_DIR_SET=1
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        -*)
            die "unknown option: $1"
            ;;
        *)
            [ "$ENV_FILE_SET" -eq 0 ] || die "env file given twice"
            ENV_FILE=$1
            ENV_FILE_SET=1
            shift
            ;;
    esac
done

if [ "$#" -gt 0 ]; then
    [ "$ENV_FILE_SET" -eq 0 ] || die "env file given twice"
    [ "$#" -eq 1 ] || die "unexpected argument: $2"
    ENV_FILE=$1
fi
[ -f "$ENV_FILE" ] || die "missing env file: $ENV_FILE (copy deploy/deploy.env.example)"
[ -r "$ENV_FILE" ] || die "cannot read env file: $ENV_FILE"

KANSO_DOMAIN=
KANSO_USER=
KANSO_HOME=
KANSO_PORT=
KANSO_IMAGE=
KANSO_CERT_NAME=
KANSO_TLS_INCLUDE=
KANSO_TLS_SNIPPET=

# Read only the known host-level keys. Never execute the file.
load_env() {
    file=$1
    while IFS= read -r line || [ -n "$line" ]; do
        line=$(printf '%s' "$line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')
        case "$line" in
            ''|'#'*) continue ;;
        esac
        case "$line" in
            *=*) ;;
            *) die "invalid line in $file (expected KEY=value)" ;;
        esac
        key=${line%%=*}
        value=${line#*=}
        # Trim surrounding blanks, then reject anything that would silently
        # change the rendered syntax.
        value=$(printf '%s' "$value" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')
        case "$value" in
            *[[:cntrl:]]*) die "$key contains a control character" ;;
            *[[:space:]]*) die "$key contains whitespace" ;;
            *'#'*) die "$key contains '#': inline comments are not supported" ;;
        esac
        case "$key" in
            KANSO_DOMAIN|KANSO_USER|KANSO_HOME|KANSO_PORT|KANSO_IMAGE|KANSO_CERT_NAME|KANSO_TLS_INCLUDE|KANSO_TLS_SNIPPET) ;;
            *) die "unknown key in $file: $key" ;;
        esac
        case "$key" in
            KANSO_DOMAIN) KANSO_DOMAIN=$value ;;
            KANSO_USER) KANSO_USER=$value ;;
            KANSO_HOME) KANSO_HOME=$value ;;
            KANSO_PORT) KANSO_PORT=$value ;;
            KANSO_IMAGE) KANSO_IMAGE=$value ;;
            KANSO_CERT_NAME) KANSO_CERT_NAME=$value ;;
            KANSO_TLS_INCLUDE) KANSO_TLS_INCLUDE=$value ;;
            KANSO_TLS_SNIPPET) KANSO_TLS_SNIPPET=$value ;;
        esac
    done < "$file"
}

load_env "$ENV_FILE"

require() {
    name=$1
    eval "value=\${$name-}"
    [ -n "$value" ] || die "missing required variable: $name"
    case "$name" in
        KANSO_CERT_NAME)
            case "$value" in
                example.com|example.com-*) die "$name still holds an example.com placeholder: $value" ;;
            esac
            ;;
        *)
            case "$value" in
                *example.com*) die "$name still holds an example.com placeholder: $value" ;;
            esac
            ;;
    esac
    case "$value" in
        *'|'*|*'&'*|*'\'*) die "$name contains an unsupported character (|, &, or backslash)" ;;
    esac
}

for name in KANSO_DOMAIN KANSO_USER KANSO_HOME KANSO_PORT KANSO_IMAGE KANSO_CERT_NAME; do
    require "$name"
done

case "$KANSO_PORT" in
    *[!0-9]*|'') die "KANSO_PORT must be a number" ;;
esac
[ "$KANSO_PORT" -ge 1 ] || die "KANSO_PORT must be between 1 and 65535"
[ "$KANSO_PORT" -le 65535 ] || die "KANSO_PORT must be between 1 and 65535"
case "$KANSO_HOME" in
    /*) ;;
    *) die "KANSO_HOME must be an absolute path" ;;
esac
case "$KANSO_HOME" in
    *:*|*,*) die "KANSO_HOME must not contain ':' or ','" ;;
esac
case "$KANSO_DOMAIN" in
    *[!A-Za-z0-9.-]*) die "KANSO_DOMAIN contains unsupported characters" ;;
esac
case "$KANSO_USER" in
    *[!A-Za-z0-9_.-]*) die "KANSO_USER contains unsupported characters" ;;
esac
case "$KANSO_CERT_NAME" in
    *[!A-Za-z0-9_.-]*) die "KANSO_CERT_NAME contains unsupported characters" ;;
esac

if [ -n "$KANSO_TLS_INCLUDE" ] && [ -n "$KANSO_TLS_SNIPPET" ]; then
    die "set only one of KANSO_TLS_INCLUDE and KANSO_TLS_SNIPPET"
fi
case "$KANSO_TLS_INCLUDE$KANSO_TLS_SNIPPET" in
    *';'*|*'{'*|*'}'*) die "TLS path contains an unsupported character (;, {, })" ;;
esac
TLS_SNIPPET_PATH=
if [ -n "$KANSO_TLS_SNIPPET" ]; then
    case "$KANSO_TLS_SNIPPET" in
        /*) TLS_SNIPPET_PATH=$KANSO_TLS_SNIPPET ;;
        *) TLS_SNIPPET_PATH="$REPO_ROOT/$KANSO_TLS_SNIPPET" ;;
    esac
    [ -f "$TLS_SNIPPET_PATH" ] || die "missing TLS snippet: $TLS_SNIPPET_PATH (relative paths resolve from the repository root)"
    [ -r "$TLS_SNIPPET_PATH" ] || die "cannot read TLS snippet: $TLS_SNIPPET_PATH"
fi
if [ -z "$KANSO_TLS_INCLUDE" ] && [ -z "$TLS_SNIPPET_PATH" ]; then
    printf 'render: warning: no TLS configured; set KANSO_TLS_INCLUDE or KANSO_TLS_SNIPPET in %s\n' "$ENV_FILE" >&2
fi

[ -e "$OUT_DIR" ] && [ ! -d "$OUT_DIR" ] && die "output path is not a directory: $OUT_DIR"
mkdir -p "$OUT_DIR"

tls_block() {
    if [ -n "$KANSO_TLS_INCLUDE" ]; then
        printf '    # TLS from KANSO_TLS_INCLUDE in the deployment env.\n'
        printf '    include %s;\n' "$KANSO_TLS_INCLUDE"
    elif [ -n "$TLS_SNIPPET_PATH" ]; then
        printf '    # TLS snippet embedded from %s.\n' "$KANSO_TLS_SNIPPET"
        sed -e 's/^/    /' "$TLS_SNIPPET_PATH"
    else
        printf '    # WARNING: no TLS configured. Set KANSO_TLS_INCLUDE or KANSO_TLS_SNIPPET\n'
        printf '    # in deploy/deploy.env before reloading nginx, or add TLS by hand.\n'
        printf '    # See deploy/tls.conf.example for the TLSRef 6.0 modern reference.\n'
    fi
}

render() {
    src=$1
    dst=$2
    tmp="${dst}.tmp.$$"
    tls_file="${OUT_DIR}/.tls.$$"
    [ -f "$src" ] || die "missing template: $src"
    trap 'rm -f "$tmp" "$tls_file"' 0 HUP INT TERM
    tls_block > "$tls_file"
    {
        printf '# Generated by deploy/render.sh from %s.\n' "$(basename "$src")"
        printf '# Values come from the deployment env file; do not edit this output.\n'
        sed \
            -e "s|__KANSO_DOMAIN__|$KANSO_DOMAIN|g" \
            -e "s|__KANSO_USER__|$KANSO_USER|g" \
            -e "s|__KANSO_HOME__|$KANSO_HOME|g" \
            -e "s|__KANSO_PORT__|$KANSO_PORT|g" \
            -e "s|__KANSO_IMAGE__|$KANSO_IMAGE|g" \
            -e "s|__KANSO_CERT_NAME__|$KANSO_CERT_NAME|g" \
            -e "s|__KANSO_PUBLIC_URL__|https://$KANSO_DOMAIN|g" \
            "$src" \
            | awk -v tlsfile="$tls_file" '
                /__KANSO_TLS__/ {
                    while ((getline line < tlsfile) > 0) print line
                    close(tlsfile)
                    next
                }
                { print }
            '
    } > "$tmp"

    if grep -q '__KANSO_[A-Z_]*__' "$tmp"; then
        rm -f "$tmp"
        die "unrendered placeholder in $tmp"
    fi
    mv "$tmp" "$dst"
    rm -f "$tls_file"
    trap - 0 HUP INT TERM
    printf 'wrote %s\n' "$dst"
}

render "$SCRIPT_DIR/kanso.container.in" "$OUT_DIR/kanso.container"
render "$SCRIPT_DIR/nginx.conf.in" "$OUT_DIR/nginx.conf"
render "$SCRIPT_DIR/kanso.env.example.in" "$OUT_DIR/kanso.env.example"
