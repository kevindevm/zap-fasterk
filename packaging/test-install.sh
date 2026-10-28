#!/usr/bin/env bash
# Usage: bash packaging/test-install.sh ubuntu:24.04 native-packages-output
# The compiler runs on the host so it cannot supply missing runtime libraries.
set -euo pipefail
image=${1:?Supply an Ubuntu, Debian or Fedora container image}
packages=$(realpath "${2:?Supply a native-packages output directory}")
case "$(uname -m)" in
  x86_64) target=linux-amd64 ;;
  aarch64) target=linux-arm64 ;;
  *) echo 'Unsupported test architecture' >&2; exit 1 ;;
esac
case "$image" in
  ubuntu:*|debian:*) format=deb ;;
  fedora:*) format=rpm ;;
  *) echo 'Unsupported test distribution' >&2; exit 1 ;;
esac
package_dir="$packages/packages/$target/$format"
test -d "$package_dir"
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
checks=$(mktemp -d)
trap 'rm -rf -- "$checks"' EXIT
cc -std=c99 -Wall -Wextra -Werror "$script_dir/check-runtime-libs.c" -ldl -o "$checks/check-runtime-libs"
docker run --rm \
  --volume "$package_dir:/packages:ro" \
  --volume "$checks:/checks:ro" \
  --env "FORMAT=$format" "$image" sh -ec '
    set -- /packages/*."$FORMAT"
    test "$#" -eq 1
    test -f "$1"
    mkdir -p /root/.config/zap-fasterk
    printf "%s\n" "preserve-existing-settings" > /root/.config/zap-fasterk/fixture
    if [ "$FORMAT" = deb ]; then
      apt-get update
      DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$1"
      dpkg-query -W zap-fasterk
    else
      dnf install -y --setopt=install_weak_deps=False "$1"
      rpm -q zap-fasterk
    fi
    zap-fasterk --version
    /checks/check-runtime-libs
    test -s /usr/share/applications/zap-fasterk.desktop
    test -s /usr/share/icons/hicolor/scalable/apps/zap-fasterk.svg
    grep -qx "Icon=zap-fasterk" /usr/share/applications/zap-fasterk.desktop
    grep -qx "StartupWMClass=zap-fasterk" /usr/share/applications/zap-fasterk.desktop
    test -s /usr/share/zap-fasterk/omarchy/zap-fasterk.json.tpl
    test -x /usr/share/zap-fasterk/omarchy/zap-fasterk-theme
    if [ "$FORMAT" = deb ]; then apt-get remove -y zap-fasterk; else dnf remove -y zap-fasterk; fi
    test ! -e /usr/bin/zap-fasterk
    test ! -e /usr/share/applications/zap-fasterk.desktop
    test ! -e /usr/share/icons/hicolor/scalable/apps/zap-fasterk.svg
    test ! -e /usr/share/zap-fasterk/omarchy/zap-fasterk.json.tpl
    test ! -e /usr/share/zap-fasterk/omarchy/zap-fasterk-theme
    test "$(cat /root/.config/zap-fasterk/fixture)" = preserve-existing-settings
  '
