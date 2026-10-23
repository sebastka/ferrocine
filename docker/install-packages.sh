#!/bin/sh
# Install Debian packages into the hardened runtime image, which has no
# package manager, with the apt and dpkg of the matching -dev image mounted
# at /x. Nothing of apt or dpkg stays behind; the package database
# (/var/lib/dpkg) is updated, so scanners and SBOMs see what was installed.
#
#   install-packages.sh PACKAGE[=VERSION]…
#
# Sources: Debian trixie and the Docker Hardened Images repository (the -dev
# image's), and Jellyfin's repository for jellyfin-ffmpeg, signed by the key
# next to this script (fingerprint 4918 AABC 486C A052 358D 778D 4902 3CD0 1DE2 1A7B).
set -eu

here=$(dirname "$0")
arch=$(/x/usr/bin/uname -m)
export PATH="/x/usr/sbin:/x/usr/bin:/x/sbin:/x/bin:$PATH"
export LD_LIBRARY_PATH="/x/usr/lib/$arch-linux-gnu:/x/lib/$arch-linux-gnu"
export DEBIAN_FRONTEND=noninteractive
t=/tmp/apt

mkdir -p "$t/etc/sources.list.d" "$t/etc/preferences.d" "$t/etc/apt.conf.d" "$t/state/lists/partial" \
  "$t/cache/archives/partial" /var/lib/dpkg/updates /var/lib/dpkg/triggers /var/lib/dpkg/alternatives \
  /var/lib/dpkg/info /etc/alternatives
touch /var/lib/dpkg/available

# dpkg's data tables and configuration, for this run only.
links=""
for d in /usr/share/dpkg /etc/dpkg; do
  if [ ! -e "$d" ]; then
    ln -s "/x$d" "$d"
    links="$links $d"
  fi
done

# The -dev image's sources, with keys read from /x; plus Jellyfin's.
for f in /x/etc/apt/sources.list.d/*.sources; do
  sed -e 's|Signed-By: /usr/|Signed-By: /x/usr/|' "$f" > "$t/etc/sources.list.d/${f##*/}"
done
cp "$here/jellyfin.asc" "$t/jellyfin.asc"
cat > "$t/etc/sources.list.d/jellyfin.sources" <<EOF
Types: deb
URIs: https://repo.jellyfin.org/debian
Suites: trixie
Components: main
Signed-By: $t/jellyfin.asc
EOF

apt() {
  apt-get -q \
    -o Dir::Etc="$t/etc" -o Dir::State="$t/state" -o Dir::Cache="$t/cache" -o Dir::Log="$t" \
    -o Dir::Bin::dpkg=/x/usr/bin/dpkg -o Dir::Bin::Methods=/x/usr/lib/apt/methods \
    -o DPkg::Path="$PATH" -o 'DPkg::Pre-Install-Pkgs::=' \
    -o APT::Sandbox::User=root -o APT::Install-Recommends=false -o Debug::NoLocking=true "$@"
}

apt update > "$t/update.log" 2>&1 || { cat "$t/update.log"; exit 1; }
# perl-base, debconf and init-system-helpers are "essential" in Debian, so
# packages assume them in their scripts; the hardened base does not have them.
if ! apt install -y perl-base debconf init-system-helpers "$@" > "$t/install.log" 2>&1; then
  grep -B2 -A6 -iE "error processing|not found|unable to|unmet" "$t/install.log" | head -60
  exit 1
fi
echo "installed: $(grep -c '^Setting up ' "$t/install.log") packages"

for d in $links; do
  rm -f "$d"
done
rm -rf "$t"
