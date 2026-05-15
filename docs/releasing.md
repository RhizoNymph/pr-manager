# Releasing pr-manager

Releases publish three install paths from one `vX.Y.Z` tag:

- crates.io via `cargo publish`
- a `.deb` package via `cargo-deb`
- an apt repository served from GitHub Pages on the `apt-repo` branch

## One-time setup

### crates.io

Create a crates.io API token and add it to the GitHub repository as:

```text
CARGO_REGISTRY_TOKEN
```

The workflow runs `cargo publish --dry-run` before publishing.

### apt repository branch

Create a GPG key for the apt repository, then add the private key to GitHub
Actions as a base64-encoded secret named:

```text
GPG_PRIVATE_KEY
```

Example export:

```sh
gpg --armor --export-secret-keys '<key-id>' | base64 -w0
```

Create an orphan branch named `apt-repo` with this structure:

```text
conf/distributions
conf/options
gpg.key
```

`conf/distributions`:

```text
Origin: pr-manager
Label: pr-manager
Codename: stable
Architectures: amd64
Components: main
Description: pr-manager APT repository
SignWith: <gpg-key-fingerprint>
```

`conf/options`:

```text
verbose
basedir .
ask-passphrase
```

`gpg.key` should contain the public key:

```sh
gpg --armor --export '<key-id>' > gpg.key
```

Push the branch, then enable GitHub Pages for the repository using the
`apt-repo` branch as the Pages source. Users will install from:

```text
https://rhizonymph.github.io/pr-manager
```

## Publishing

Update `Cargo.toml` to the release version, then tag the matching version:

```sh
git tag v0.2.0
git push origin v0.2.0
```

The tag must match the Cargo package version exactly, without the leading `v`.
For example, `version = "0.2.0"` must be released as `v0.2.0`.

The release workflow will:

1. run tests
2. verify `cargo publish --dry-run`
3. publish to crates.io
4. build `pr-manager-<version>-x86_64-unknown-linux-gnu.tar.gz`
5. build `target/debian/pr-manager_<version>_amd64.deb`
6. update the `apt-repo` branch with `reprepro`
7. create a GitHub Release containing the binary tarball and `.deb`
