#!/bin/sh
# Install ax — an autonomous agent that pursues a goal by writing and running code.
#
#   curl -fsSL https://raw.githubusercontent.com/cloudycotton/ax/main/install.sh | sh
#
# Honours:
#   AX_VERSION      install a specific version instead of the latest (e.g. 0.2.0)
#   AX_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   GITHUB_TOKEN    required only while the repository is private
#
# POSIX sh on purpose: this has to run under dash and busybox ash, not just bash.

set -eu

REPO="${AX_REPO:-cloudycotton/ax}"
INSTALL_DIR="${AX_INSTALL_DIR:-$HOME/.local/bin}"

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
dim()  { printf '\033[2m%s\033[0m\n' "$1"; }
die()  { printf '\033[31merror\033[0m %s\n' "$1" >&2; exit 1; }

# --- what are we running on? -------------------------------------------------

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux)  os_part="unknown-linux-gnu" ;;
    *) die "unsupported operating system: $os. Build from source: cargo install --git https://github.com/$REPO" ;;
  esac
  case "$arch" in
    arm64|aarch64) arch_part="aarch64" ;;
    x86_64|amd64)  arch_part="x86_64" ;;
    *) die "unsupported architecture: $arch" ;;
  esac
  echo "${arch_part}-${os_part}"
}

# --- talking to GitHub -------------------------------------------------------

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"; }

auth_header() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    echo "Authorization: Bearer $GITHUB_TOKEN"
  elif [ -n "${GH_TOKEN:-}" ]; then
    echo "Authorization: Bearer $GH_TOKEN"
  else
    echo ""
  fi
}

# fetch <url> <accept> <output-path>
fetch() {
  url="$1"; accept="$2"; out="$3"
  auth="$(auth_header)"
  if [ -n "$auth" ]; then
    curl -fsSL -H "$auth" -H "Accept: $accept" -o "$out" "$url" && return 0
  else
    curl -fsSL -H "Accept: $accept" -o "$out" "$url" && return 0
  fi
  return 1
}

main() {
  need curl
  need tar
  target="$(detect_target)"

  bold "Installing ax ($target)"

  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT INT TERM

  # Resolve the release. The API works for private repositories given a token,
  # which the plain download URLs do not.
  if [ -n "${AX_VERSION:-}" ]; then
    api="https://api.github.com/repos/$REPO/releases/tags/v${AX_VERSION#v}"
  else
    api="https://api.github.com/repos/$REPO/releases/latest"
  fi

  if ! fetch "$api" "application/vnd.github+json" "$tmp/release.json"; then
    if [ -z "${GITHUB_TOKEN:-}${GH_TOKEN:-}" ]; then
      die "could not read releases for $REPO.
  If the repository is private, export GITHUB_TOKEN with \`repo\` scope and re-run:
    curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | GITHUB_TOKEN=… sh"
    fi
    die "could not read releases for $REPO (is the token valid?)"
  fi

  version="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp/release.json" | head -1)"
  [ -n "$version" ] || die "no published release found for $REPO"

  asset="ax-${version}-${target}.tar.gz"

  # Look the asset up by name and use its API url: that is the form that
  # supports token auth, which browser_download_url does not.
  asset_id="$(tr '{' '\n' < "$tmp/release.json" \
    | grep -F "\"name\":\"$asset\"" \
    | sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -1)"
  [ -n "$asset_id" ] || die "release v$version has no build for $target"

  sums_id="$(tr '{' '\n' < "$tmp/release.json" \
    | grep -F '"name":"checksums.txt"' \
    | sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -1)"

  dim "downloading $asset"
  fetch "https://api.github.com/repos/$REPO/releases/assets/$asset_id" \
        "application/octet-stream" "$tmp/$asset" \
    || die "download failed"

  # Verify before unpacking: this binary will later run with your API key and
  # your browser session.
  if [ -n "$sums_id" ]; then
    fetch "https://api.github.com/repos/$REPO/releases/assets/$sums_id" \
          "application/octet-stream" "$tmp/checksums.txt" \
      || die "could not download checksums.txt"

    expected="$(grep -F " $asset" "$tmp/checksums.txt" | awk '{print $1}' | head -1)"
    [ -n "$expected" ] || die "$asset is not listed in checksums.txt"

    if command -v shasum >/dev/null 2>&1; then
      actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
    elif command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
    else
      die "neither shasum nor sha256sum is available to verify the download"
    fi

    [ "$actual" = "$expected" ] || die "checksum mismatch for $asset
  expected $expected
  got      $actual"
    dim "sha256 verified"
  else
    die "release v$version publishes no checksums.txt; refusing to install unverified"
  fi

  tar -xzf "$tmp/$asset" -C "$tmp"
  binary="$(find "$tmp" -type f -name ax -perm -u+x 2>/dev/null | head -1)"
  [ -n "$binary" ] || binary="$(find "$tmp" -type f -name ax | head -1)"
  [ -n "$binary" ] || die "the archive did not contain an ax binary"

  mkdir -p "$INSTALL_DIR" || die "could not create $INSTALL_DIR"
  # Atomic swap, so an ax that is currently running is never half-replaced.
  install_tmp="$INSTALL_DIR/.ax.new.$$"
  cp "$binary" "$install_tmp" || die "could not write to $INSTALL_DIR"
  chmod 755 "$install_tmp"
  mv -f "$install_tmp" "$INSTALL_DIR/ax" || die "could not install into $INSTALL_DIR"

  printf '\033[32m✓\033[0m installed ax %s to %s\n\n' "$version" "$INSTALL_DIR/ax"

  # Is it actually reachable?
  case ":$PATH:" in
    *":$INSTALL_DIR:"*)
      bold "Next: run  ax"
      dim  "It will ask for your API endpoint, key, and model."
      ;;
    *)
      bold "Add it to your PATH, then run  ax"
      case "${SHELL:-}" in
        */fish) echo "  fish_add_path $INSTALL_DIR" ;;
        */zsh)  echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc && exec zsh" ;;
        *)      echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc && exec bash" ;;
      esac
      ;;
  esac
}

main "$@"
