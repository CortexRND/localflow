# Releasing LocalFlow v0.2.1

The macOS installer defaults to the native Rust app, built for the current Mac.
It provisions stable Rust with rustup (minimal profile) and waits up to 30 minutes
for Apple's Command Line Tools dialog to finish. Missing CMake uses Homebrew;
without Homebrew install CMake from https://cmake.org/download/ first. Rust does
not create a Python environment or download Python speech models. Its setup
wizard downloads the selected models. Optional Ollama installation uses Homebrew.

To install the Python fallback on macOS, run
`LOCALFLOW_RUNTIME=python bash install.sh`. Linux and Windows retain Python by
default. Existing configuration, history and notes remain compatible.

## How default installs are verified

Without `LOCALFLOW_REF`, `install.sh` always installs the release's
`localflow-<ver>.tar.gz`, whether or not git is available, and refuses to unpack
it unless it matches the release's `SHA256SUMS`. Fresh installs, earlier archive
installs and earlier managed git checkouts (from installers before v0.2.1) are
all handled the same way: the existing directory is replaced only after the
download verifies, its `.venv` is kept, and a failed download or checksum leaves
it untouched. A replaced checkout whose `.git` is a directory is never
deleted, even when it is clean: it is renamed intact to a sibling `<name>.git-backup-<YYYYmmdd-HHMMSS>-<pid>`
directory (for example `~/.localflow/app.git-backup-20261120-101500-4242`),
outside the temporary download directory, and the installer prints the exact
path. Uncommitted, staged and untracked files, unpushed commits, branches,
stashes, hooks and other git metadata therefore survive; only `.venv` moves to
the new install for reuse. The backup is a plain rename, so it needs no git. If
the swap fails, the previous directory and its `.venv` are restored and no
backup is left behind. A destination whose `.git` is a file (a linked git
worktree or submodule) is refused before anything is downloaded or moved:
renaming it would break the bookkeeping its parent repository keeps about it,
so its files, index, `.venv` and git metadata are left exactly as they were.
A destination that is not recognisably a LocalFlow install is
refused, never overwritten. Running `install.sh` from inside a developer
checkout installs that checkout as is; `LOCALFLOW_REF` remains an explicitly
unverified git/codeload override. With a usable git, a custom ref always uses
git, even over an existing archive install: it is cloned from
`LOCALFLOW_REPO_URL`, fetched and checked out in a sibling staging directory,
and swapped in (keeping `.venv`) only after every git step succeeds; a git
failure leaves the existing install untouched and never falls back to an
archive. Fresh and existing git installs alike always run
`git fetch origin <ref>` and `git checkout --detach FETCH_HEAD`, so the
requested ref is never satisfied by a stale local branch; `origin/<name>` and
`refs/remotes/origin/<name>` are fetched as origin's branch `<name>`, while
branch names, `refs/heads/<name>`, tags and full commit SHAs are fetched as
given. Codeload is used only when no usable git is found, with a warning.

The installer intentionally pins no git commit. The release tag is placed on
the merge commit of the release branch, and that commit contains `install.sh`;
a file cannot contain the hash of a commit that includes it, so the commit is
unknowable while the installer is written. The published v0.2.0 installer did
not verify Git checkouts; PR #59 subsequently pinned its already-published
commit. That post-release scheme cannot prepare a future release. A
fetched tag is never treated as the expected source either. The trust root is
the checksummed release archive.

## README installer checksums

The README one-liners download `install.sh` and `install.ps1` from the release
tag and compare them with SHA-256 digests written in the README. Those digests
must be the SHA-256 of the installer files on the release branch, because those
are the exact bytes the tag will publish. Regenerate them after **every** change
to either installer, before tagging:

```sh
shasum -a 256 install.sh                    # lowercase digest for the install.sh lines
shasum -a 256 install.ps1 | tr a-f A-F      # uppercase digest for the install.ps1 line
```

`tests/test_release_default.py` hashes the working-tree installers and fails if
any README one-liner carries a different digest or points at a tag other than
the current version.

## Version bumps and the standalone desktop lock

`rust/lf-desktop` is a standalone Cargo workspace with its own
`rust/lf-desktop/Cargo.lock`. It depends on `lf-core`, `lf-ui`, `lf-providers`
and `lf-audio` (and transitively other local crates) by path, so bumping
`[workspace.package] version` in `rust/Cargo.toml`, or any local crate's own
version, leaves that lock stale. After every such bump, refresh it without
upgrading third-party crates:

```sh
cargo metadata --manifest-path rust/lf-desktop/Cargo.toml --format-version 1 >/dev/null
git diff rust/lf-desktop/Cargo.lock   # expect only local path package versions to change
```

`lf-desktop` keeps its own `version` in `rust/lf-desktop/Cargo.toml`; it is not
inherited from the root workspace. `tests/test_release_default.py` fails if any
source-less (path) package in the desktop lock differs from its manifest version.

## Tag-triggered assets

After the release branch is merged, a maintainer can tag its commit and push:

```sh
git tag v0.2.1
git push origin v0.2.1
```

The existing **Mac installer** workflow (`.github/workflows/mac-installer.yml`)
runs on `v*` tag pushes. It builds and validates the DMG on `macos-latest`, runs
`scripts/release-assets.sh` against the tag, creates the GitHub release if needed,
and uploads all five assets:

- `Install.LocalFlow.cmd` (the repository's `packaging/windows/Install LocalFlow.cmd`;
  GitHub replaces spaces in asset names with dots, so the workflow copies it to
  the dotted name and checksums that exact name)
- `localflow-0.2.1.tar.gz`
- `localflow-0.2.1.zip`
- `LocalFlow-Installer.dmg`
- `SHA256SUMS` (covers all four downloadable files, by their uploaded names)

Before uploading, the workflow runs `shasum -a 256 -c SHA256SUMS` and refuses to
publish if a payload is missing, a digest does not match, or any staged asset is
not listed under its exact upload name.

The source archives are reproducible and the tag version must match
`pyproject.toml`. No tag or release is published just by pushing a branch.
Before announcing the release, confirm that the published
`raw.githubusercontent.com/CortexRND/localflow/v0.2.1/install.sh` and
`install.ps1` hash to the README digests.

For a manual build on a Mac, check out the tag in a clean checkout, then:

```sh
git checkout v0.2.1
bash scripts/build_mac_installer.sh
bash scripts/release-assets.sh v0.2.1
cp dist/LocalFlow-Installer.dmg dist/release/LocalFlow-Installer.dmg
cp "packaging/windows/Install LocalFlow.cmd" dist/release/Install.LocalFlow.cmd
(cd dist/release && shasum -a 256 LocalFlow-Installer.dmg Install.LocalFlow.cmd >> SHA256SUMS)
(cd dist/release && shasum -a 256 -c SHA256SUMS)
gh release create v0.2.1 dist/release/* --verify-tag --generate-notes
```
