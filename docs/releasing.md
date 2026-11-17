# Releasing LocalFlow v0.2.0

The macOS installer defaults to the native Rust app, built for the current Mac.
It provisions stable Rust with rustup (minimal profile) and waits up to 30 minutes
for Apple's Command Line Tools dialog to finish. Missing CMake uses Homebrew;
without Homebrew install CMake from https://cmake.org/download/ first. Rust does
not create a Python environment or download Python speech models. Its setup
wizard downloads the selected models. Optional Ollama installation uses Homebrew.

To install the Python fallback on macOS, run
`LOCALFLOW_RUNTIME=python bash install.sh`. Linux and Windows retain Python by
default. Existing configuration, history and notes remain compatible.

## Tag-triggered assets

After the release branch is merged, a maintainer can tag its commit and push:

```sh
git tag v0.2.0
git push origin v0.2.0
```

The existing **Mac installer** workflow (`.github/workflows/mac-installer.yml`)
runs on `v*` tag pushes. It builds and validates the DMG on `macos-latest`, runs
`scripts/release-assets.sh` against the tag, creates the GitHub release if needed,
and uploads all five assets:

- `Install LocalFlow.cmd`
- `localflow-0.2.0.tar.gz`
- `localflow-0.2.0.zip`
- `LocalFlow-Installer.dmg`
- `SHA256SUMS` (covers all four downloadable files)

The source archives are reproducible and the tag version must match
`pyproject.toml`. No tag or release is published just by pushing a branch.

For a manual build on a Mac, check out the tag in a clean checkout, then:

```sh
git checkout v0.2.0
bash scripts/build_mac_installer.sh
bash scripts/release-assets.sh v0.2.0
cp dist/LocalFlow-Installer.dmg dist/release/
cp "packaging/windows/Install LocalFlow.cmd" dist/release/
(cd dist/release && shasum -a 256 LocalFlow-Installer.dmg "Install LocalFlow.cmd" >> SHA256SUMS)
gh release create v0.2.0 dist/release/* --verify-tag --generate-notes
```
