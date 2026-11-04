#!/bin/sh
# Turns a directory of built packages for one architecture into eave's pacman repository: signs
# each package and writes the signed database that pacman reads (D-135). The key must already be
# in gpg's keyring. Run by .github/workflows/repo.yml.
#
#   sh dist/pacman/repo.sh <dir> <key fingerprint>
set -eu
dir=$1
key=$2
cd "$dir"
rm -f eave.db* eave.files*
for pkg in *.pkg.tar.zst; do
	gpg --batch --yes --local-user "$key" --detach-sign --no-armor "$pkg"
done
repo-add --sign --key "$key" eave.db.tar.zst ./*.pkg.tar.zst
# repo-add makes eave.db a link to the archive. A release asset can't be a link.
for db in eave.db eave.files; do
	rm "$db" "$db.sig"
	cp "$db.tar.zst" "$db"
	cp "$db.tar.zst.sig" "$db.sig"
done
