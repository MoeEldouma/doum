#!/bin/sh
# Install doum for the current user: the binary in ~/.local/bin and (optionally) the systemd user service.
#   curl -fsSL https://raw.githubusercontent.com/MoeEldouma/doum/main/install.sh | sh
#   DOUM_VERSION=v0.1.0 sh install.sh        # a specific release
#   DOUM_NO_SERVICE=1 sh install.sh          # binary only
set -eu

repo="MoeEldouma/doum"
bin_dir="${HOME}/.local/bin"
version="${DOUM_VERSION:-latest}"

case "$(uname -s)" in
  Linux) ;;
  *) echo "doum: only Linux is supported for now" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) echo "doum: unsupported CPU: $(uname -m)" >&2; exit 1 ;;
esac

if [ "$version" = latest ]; then
  version=$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$version" ] || { echo "doum: could not find the latest release" >&2; exit 1; }
fi

name="doum-${version}-${arch}-linux"
url="https://github.com/${repo}/releases/download/${version}/${name}.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "doum: downloading ${name}"
curl -fsSL "$url" -o "$tmp/${name}.tar.gz"
curl -fsSL "${url}.sha256" -o "$tmp/${name}.tar.gz.sha256"
(cd "$tmp" && sha256sum -c "${name}.tar.gz.sha256" >/dev/null) || { echo "doum: checksum mismatch, not installing" >&2; exit 1; }
tar -C "$tmp" -xzf "$tmp/${name}.tar.gz"

mkdir -p "$bin_dir"
install -m 0755 "$tmp/${name}/doum" "$bin_dir/doum"
echo "doum: installed ${bin_dir}/doum ($("$bin_dir/doum" --version 2>/dev/null || echo "$version"))"

if [ -z "${DOUM_NO_SERVICE:-}" ] && command -v systemctl >/dev/null 2>&1; then
  unit_dir="${HOME}/.config/systemd/user"
  mkdir -p "$unit_dir"
  install -m 0644 "$tmp/${name}/deploy/systemd/doum.service" "$unit_dir/doum.service"
  systemctl --user daemon-reload
  systemctl --user enable --now doum.service
  echo "doum: daemon running as a user service (systemctl --user status doum)"
fi

case ":${PATH}:" in
  *":${bin_dir}:"*) ;;
  *) echo "doum: add ${bin_dir} to your PATH" ;;
esac
echo "doum: try 'doum status', then 'doum run -s 8 -- your-gpu-command'"
