# The AUR package

`eave/` is the [AUR package `eave`](https://aur.archlinux.org/packages/eave): it ships `eave-build` only, built from a release tarball ([D-54](../../docs/decisions.md)). CI builds it from every commit, and the [pacman repository](../pacman/README.md) serves it prebuilt from every tag (D-135).

## Releasing

1. Bump `version` in `crates/*/Cargo.toml` and `plugins/*/Cargo.toml` where it changed. In `dist/aur/eave/`, set `pkgver=X.Y.Z` and `pkgrel=1` and run `makepkg --printsrcinfo > .SRCINFO`. Merge to `main`. The tag builds the [pacman repository](../pacman/README.md) from this PKGBUILD, and it refuses one of another version.
2. Tag and push: `git tag -a vX.Y.Z -m "eave X.Y.Z" && git push origin vX.Y.Z`. A packaged eave-build builds `eaveshell/eave@vX.Y.Z`, so the tag must exist before the package does. Pushing it publishes the pacman repository.
3. In `dist/aur/eave/`: run `updpkgsums` (from `pacman-contrib`), which checksums the tag's tarball, then `makepkg --printsrcinfo > .SRCINFO`. Commit.
4. Publish:

   ```sh
   git clone ssh://aur@aur.archlinux.org/eave.git aur-eave
   cp dist/aur/eave/{PKGBUILD,eave.install,.SRCINFO} aur-eave/
   cd aur-eave && git commit -am "eave X.Y.Z" && git push
   ```

A packaging-only fix bumps `pkgrel` instead of `pkgver`.
