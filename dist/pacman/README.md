# The pacman repository

eave's pacman repository serves the [AUR package](../aur/README.md), prebuilt for x86_64 and aarch64 and signed ([D-135](../../docs/decisions.md)). It holds `eave-build` only: the bar is still built per user with the plugins config.toml uses.

## Adding it

Trust the signing key once:

```sh
curl -fsSL https://raw.githubusercontent.com/eaveshell/eave/main/dist/pacman/eave.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key B498F572F57269382E01AE73E8A6AB6E761025F7
```

Add the repository to `/etc/pacman.conf`:

```ini
[eave]
Server = https://github.com/eaveshell/eave/releases/download/repo-$arch
```

Then `sudo pacman -Syu eave`, and the steps the package prints.

## Publishing

Pushing a `vX.Y.Z` tag runs [.github/workflows/repo.yml](../../.github/workflows/repo.yml). It builds the tag's `dist/aur/eave/PKGBUILD` on both architectures, signs the packages and the database ([repo.sh](repo.sh)), and replaces the assets of the `repo-x86_64` and `repo-aarch64` releases. The PKGBUILD's `pkgver` must be the tag's version, so bump it before tagging. To publish a tag that's already pushed, run the workflow from the Actions tab with that tag.

A packaging-only fix (a new `pkgrel`) needs a new tag too, since the workflow builds from tags. Don't move a pushed tag.

## The signing key

`eave.asc` is the public key, an ed25519 signing key that never expires. Its private half is the repository secret `PACMAN_SIGNING_KEY`, an armored export (`gpg --armor --export-secret-keys B498F572…`) without a passphrase. Only the workflow's publish job reads it.

To replace the key: generate a new one, set the secret, replace `eave.asc` and the fingerprint here, in the workflow's `SIGNING_KEY` and in D-135, and publish again. Everyone who added the repository has to import the new key.
