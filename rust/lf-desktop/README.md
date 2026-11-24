# lf-desktop: native LocalFlow desktop app (GPUI preview)

A native desktop window for LocalFlow, drawn with [GPUI](https://crates.io/crates/gpui)
(pinned `=0.2.2`). It reads and writes the stores LocalFlow already uses (config TOML,
history SQLite, `dictionary.json`, `hot_phrases.json`, keyring) and talks to the
existing local HTTP service for Ask, Meetings and Prompts.

**The migration is not complete.** This crate covers steps 1 and 2 of a staged
migration: a parallel GPUI front end connected to the real stores, plus the
runtime/setup parity work (setup wizard, model storage, key import, Test
connection, settings assistant, runtime status, single-instance routing and an
opt-in in-window engine). It isn't the default launcher and it doesn't replace
`lf hub`, `lf-hub` (the macOS AppKit Hub), `lf setup` or the Python runtime.
Step 3 adds a separate, opt-in **LocalFlow Preview** package and installer
([Preview package](#preview-package-step-3)); `install.sh`, the existing
installers and every launcher default are unchanged. Step 4a lets
`lf hub` / `lf phrases` / `lf setup` open it when asked
([Opening it from `lf`](#opening-it-from-lf-step-4a)); the default stays the
legacy window. By default this window
does not run dictation; a `--features runtime` build can, after `--engine` and an
explicit **Start here**.

## Migration sequence and acceptance gates

Each step is its own reviewable PR. A step starts only when the previous step's
gate holds. The full feature matrix is in
[`docs/gpui-migration.md`](../../docs/gpui-migration.md).

| Step | PR scope | Acceptance gate before the next step |
|---|---|---|
| **1. Parallel front end** (done) | Native app beside the existing ones. Home, History, Dictionary, Snippets, Style, Settings and Models & keys are connected to the real stores and config. Ask, Meetings and Prompts use the existing local HTTP service. Isolated `--sandbox`. Dedicated `gpui.yml` CI | Linux and macOS CI green (`gpui.yml`). Legacy runtime, launchers and installers unchanged. Existing CI changed only by the Windows `whisper-rs-sys` CMake-cache cleanup in `rust.yml`. UI testing for this PR was waived by the owner; native rendering on screen is unverified (later steps keep their validation gates) |
| **2. Runtime and global-input parity** (done) | Setup wizard (checks, mic level, model choice/download, cleanup, shortcut, trial, apply). Model storage. Settings assistant. Key import and Test connection. Runtime status from the runtime's own status file (protocol 2) instead of HTTP reachability. Linux X11 global hotkeys in the Rust engine. Single instance and page routing. Opt-in engine behind `--features runtime` + `--engine` | Every capability the Python Hub and `lf-hub` offer works here, or the owner waives it in writing. Dictation readiness is shown only from a runtime signal. Interactive and macOS runtime QA are still open (UI probing on the development VM was waived by the owner) |
| **3. Packaging** (done) | A dedicated preview package and installer ship a release `lf-desktop` (runtime feature) for Linux x86_64 and macOS arm64/x86_64, built natively in `gpui-package.yml`. System-library requirements documented per distribution. Separate ad-hoc signed `LocalFlow Preview.app`. Published from release tags with `SHA256SUMS-gpui-preview` | Clean install, upgrade and uninstall on each supported platform. Existing installs keep working. Interactive (GUI, permissions, Gatekeeper) QA is still open |
| **4a. Opt-in launch** (this PR) | `--gpui` / `--legacy` on `lf hub`, `lf phrases` and the `lf setup` window, or `LOCALFLOW_UI=gpui\|legacy` (no config key). Finds only the preview package or an absolute `LOCALFLOW_DESKTOP_BIN`, probes its `--help`, and falls back to the legacy window. `lf-hub` on Linux opens it. Default unchanged (legacy) | Owner decision to promote the default (4b) |
| 4b. Default-launch promotion | `lf hub`, `lf phrases` and `lf setup` prefer `lf-desktop` by default, with the 4a fallback | Not started. The data-safety hash check passes. Accessibility is accepted (GPUI has no accessibility tree today). Light and dark screenshots of every page are reviewed. The owner agrees |
| 5. Legacy retirement | Remove the Tk Hub and the AppKit Hub paths that are no longer launched, and controller code duplicated from `lf_ui::mac` | One release cycle on the new default without a rollback |

## Build and run

```bash
cargo build --manifest-path rust/lf-desktop/Cargo.toml          # -> rust/lf-desktop/target/debug/lf-desktop
scripts/run_gpui.sh --sandbox                                     # build + run with example data
scripts/run_gpui.sh --page history                                # build + run on your real data
LF_DESKTOP_RELEASE=1 scripts/run_gpui.sh                          # optimised build

cargo test   --manifest-path rust/lf-desktop/Cargo.toml --workspace
cargo fmt    --manifest-path rust/lf-desktop/Cargo.toml --all -- --check
cargo clippy --manifest-path rust/lf-desktop/Cargo.toml --workspace --all-targets -- -D warnings

# Runtime-enabled build: native speech engine (whisper.cpp / Parakeet ONNX),
# model inspection/download/delete, the setup trial and the opt-in engine.
cargo build  --manifest-path rust/lf-desktop/Cargo.toml --features runtime
cargo test   --manifest-path rust/lf-desktop/Cargo.toml --workspace --features runtime
rust/lf-desktop/target/debug/lf-desktop --engine     # then press "Start here" on Home
```

`lf-desktop --help` lists the options:

| Option | Effect |
|---|---|
| `--page <PAGE>` | `home`, `history`, `dictionary`, `snippets` (also `"Hot Phrases"`), `style`, `ask`, `meetings`, `prompts`, `models`, `settings`, `setup`. An unknown page opens Home with a notice |
| `--setup` | Opens the setup wizard (or hands it to the window already open) |
| `--engine` | Lets this window run dictation (Home → **Start here**). Needs a `--features runtime` build; refused with the rebuild command otherwise, and always refused with `--sandbox`. Nothing starts until you press Start |
| `--sandbox` | Uses isolated example data and in-process fake services (also `LOCALFLOW_SANDBOX=1`) |
| `--data-dir <DIR>` | Sandbox directory, only valid with `--sandbox` |
| `--sandbox-reset` | Re-seeds the sandbox directory (refused while that sandbox is open in another window) |
| `--sandbox-exercise` | Writes once to each sandbox store (Settings, a provider key, Hot Phrases, History, Prompts) through the page controllers, prints one line per store and exits without a window. Only valid with `--sandbox`; refused while that sandbox is open in another window; used by Gate 2 |

An unknown argument, or `--engine` where it can't run, exits with status 2. A first
launch with no setup marker in the config (and no `--page`) opens the wizard,
using the Hub's `hub_route::plan_initial_route` policy.

### System tray

On Linux and Windows, LocalFlow shows a tray item while the desktop window is
running; on macOS it is a menu bar item. Right-click it (on macOS, click it or
reach it with the keyboard) for runtime status, **Open LocalFlow**, the dictation
switch and **Quit LocalFlow**. The switch uses the active config (the isolated
config in sandbox mode). **Paste last transcript** is enabled only while this
window's opt-in dictation engine is running; it never sends commands to another
runtime. Closing the last window still quits the app.

On Linux the tray uses the session D-Bus and needs a StatusNotifierItem host,
such as KDE Plasma or GNOME with an AppIndicator extension. If the host is
unavailable, the window still runs.

On macOS the menu bar item uses a template icon (tinted for light and dark menu
bars) and is not shown in `--sandbox`, so sandbox automation leaves the menu bar
alone. **Paste last transcript** checks the target once, when the window
handles the request: if the app that was frontmost when you chose it is
LocalFlow, unknown, or no longer frontmost, nothing is pasted, no app is
activated and the clipboard is untouched; a toast in the LocalFlow window asks
you to switch to the target app and choose it again. Choosing it again before
LocalFlow has handled the first request does nothing. After the check passes,
the paste goes through the engine queue and a history lookup (which waits if
the history database is busy) before the paste keystroke, and goes to whichever
app is frontmost then, as with the paste shortcut. The item is not interactively
verified on macOS yet (UI QA waived); an earlier native probe exercised the
status item and its menu callbacks.

The crate is a **standalone Cargo workspace** with its own `Cargo.lock`, so GPUI's
dependency tree and system libraries stay out of the `rust/` workspace and its CI.
`.github/workflows/gpui.yml` is an additive workflow that only runs when this crate
or one of its path dependencies changes:

- **Linux:** fmt, then clippy, the headless tests (including the shim's) and the
  build, each for the default build and for `--features runtime`.
- **macOS:** the shim tests and a full type-check of both builds, including the
  menu bar item.
- **Windows:** a type-check of the desktop and its native tray.

The runtime feature adds `lf-stt` (whisper.cpp needs CMake and libclang) and
`lf-ui/runtime`; its tests use temp directories and fakes only (no audio, keyboard
hook, model download or keyring).

`vendor/xattr-shim` is a member of this workspace and contains no unsafe code. GPUI's
tar dependency requires `xattr 0.2`, whose last release reads `libc::ENOATTR`,
which the libc version LocalFlow's keyring backend needs no longer has. The shim
keeps the 0.2 package name and version and re-exports upstream **xattr 1.x**. The
reason and the removal condition are in `vendor/xattr-shim/src/lib.rs`.

## Platform support and prerequisites

| Platform | Status |
|---|---|
| Linux (X11, and Wayland through GPUI's Wayland backend) | Built and unit-tested here (Ubuntu, Mesa llvmpipe). Native rendering was not verified interactively: first paint was transparent under X11/VNC, and under Cage/Wayland redraws appeared only after a resize |
| macOS | Expected to build (GPUI's Metal backend; the code has no Linux-only parts). Not verified in this PR. `lf-hub` stays the macOS app |
| Windows | Preview package only (`windows-x86_64` zip, below): built, PE-checked and launched in `--sandbox` on `windows-latest` by `gpui-package.yml`. No installer; `install.ps1` and the Python runtime are unchanged. It includes the native tray and, with `--engine`, the engine's Windows global hotkeys. Windows CI exercises the `lf-input` keyboard hook, type-checks the tray and launches the package in `--sandbox`; none of this is verified interactively |

Linux build packages (Debian/Ubuntu names): `build-essential cmake clang pkg-config
libasound2-dev libfontconfig1-dev libfreetype-dev libwayland-dev libxkbcommon-dev
libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libxcb-shape0-dev
libxcb-xfixes0-dev libvulkan-dev libzstd-dev`.
To run it you need a Vulkan driver (Mesa works, llvmpipe included) and a graphical
session (`DISPLAY` or `WAYLAND_DISPLAY`). The toolchain used here is Rust 1.99.

**Fonts.** The UI uses the first installed family from: Figtree, Inter, Cantarell,
Ubuntu, Noto Sans, Liberation Sans, DejaVu Sans. Figtree isn't bundled yet. Mono
text (key names, config keys) uses JetBrains Mono when it's installed. **Icons** are
Lucide outline SVGs (ISC, `assets/icons/LICENSE-lucide.txt`), embedded in the
binary.

## Preview package (step 3)

`scripts/build_gpui_package.sh` builds a ready-to-run package of a release
`lf-desktop` **with** `--features runtime`, natively for the machine it runs on
(Linux x86_64, macOS arm64, macOS x86_64 or Windows x86_64 under Git Bash with
MSVC; `CARGO_BUILD_TARGET` is refused, as
in `scripts/build_rust_app.sh`). It builds in `rust/lf-desktop/target` unless
`CARGO_TARGET_DIR` is set, and writes to `dist/gpui-preview/` or the absolute
directory you pass:

| Asset | Contents |
|---|---|
| `LocalFlow-Preview-<ver>-linux-x86_64.tar.gz` | `localflow-preview/`: `bin/lf-desktop`, `lib/` (Sherpa-ONNX C and C++ API and ONNX Runtime `.so` files), `share/applications/localflow-preview.desktop`, `share/icons/localflow-preview.png`, `README.txt`, `MANIFEST.json` (version, measured ABI requirements, SHA-256 of every file) |
| `LocalFlow-Preview-<ver>-macos-arm64.tar.gz`, `LocalFlow-Preview-<ver>-macos-x86_64.tar.gz` | `LocalFlow Preview.app` |
| `LocalFlow-Preview-<ver>-windows-x86_64.zip` | `LocalFlow Preview\`: `lf-desktop.exe` (static CRT, no Visual C++ Redistributable), the Sherpa-ONNX C/C++ API and ONNX Runtime DLLs next to it, `README.txt`, `MANIFEST.json`. Unzip and run; there is no installer |

Each archive comes with `<asset>.sha256`. Nothing in the output directory is
replaced until `scripts/verify_gpui_package.sh` has extracted the archive into
a fresh directory and checked the extracted copy:

- **Every platform:** only regular files and directories under the one expected
  root; `lf-desktop --version` matches `rust/lf-desktop/Cargo.toml`; `--help`
  works, `--sandbox --engine` is refused, and the `--speech-helper` mode exits
  cleanly on stdin EOF and rejects extra arguments. These run with an empty
  environment and a throwaway `HOME`; nothing opens a window, a device, the
  keyring or the network.
- **Linux:** the binary's `RUNPATH` is exactly `$ORIGIN:$ORIGIN/../lib` (set at
  link time by `build.rs`, no `patchelf`), each bundled library's is `$ORIGIN`,
  every `DT_NEEDED` entry is bundled or on the system allowlist below, `ldd`
  resolves the bundled libraries inside the package and the rest from system
  library directories, the symbol-version floor below holds for the binary and
  every bundled library, the files match `MANIFEST.json`, and the desktop entry
  passes `desktop-file-validate` (from `desktop-file-utils`, needed to build).
- **macOS:** the same relocation as `lf-hub`'s bundle: all non-system dylibs
  in `Contents/Frameworks` with `@rpath` install names, build-machine search
  paths removed (only `@executable_path/../Frameworks` and `@loader_path`), no
  dependency outside `/usr/lib` and `/System/Library`, nested dylibs and then
  the bundle ad-hoc signed with the designated requirement
  `identifier "com.cortexrnd.localflow.desktop-preview"`, and
  `codesign --verify --deep --strict` on the extracted copy.
- **Windows:** one flat folder; `llvm-readobj --coff-imports` on the `.exe`
  and every DLL finds only bundled DLLs or DLLs in `System32` (and
  `api-ms-win-*` sets), never `VCRUNTIME*`/`MSVCP*`; the imports and files
  match `MANIFEST.json`. The CLI smoke checks run with only `System32` on
  `PATH`. Zips are written and read with `bsdtar` (`System32\tar.exe`).

`.github/workflows/gpui-package.yml` runs this on `ubuntu-22.04`,
`macos-latest` (arm64), `macos-15-intel` (x86_64) and `windows-latest`,
re-verifies a copy outside the checkout, runs the installer below twice and
then `--uninstall` in a temporary `HOME` (with `--no-launch`; Linux and macOS),
and uploads `gpui-preview-<platform>` artifacts. On Windows it instead unzips
the archive with `Expand-Archive`, starts `lf-desktop.exe --sandbox` and
requires a window that stays up for 5 s (screenshot in the
`windows-launch-screenshot` artifact), then stops it. It runs on pull requests and pushes to `main` that touch the
package inputs, and on manual dispatch; `mac-installer.yml` calls it only for
release tags and manual runs, so a pull request builds the native legs once.
On release tags `mac-installer.yml` publishes them (see `docs/releasing.md`).

### Installing the preview

`scripts/install_gpui_preview.sh` needs no Rust toolchain. It uses Bash, `tar`,
`sha256sum` or `shasum`, `curl` (downloads only) and, on macOS, the stock
`plutil`, `codesign` and `open`.

```bash
scripts/install_gpui_preview.sh                    # this release's package, checked against SHA256SUMS-gpui-preview
scripts/install_gpui_preview.sh --release v0.2.3   # another release
scripts/install_gpui_preview.sh --archive LocalFlow-Preview-0.2.2-linux-x86_64.tar.gz \
    --sha256 LocalFlow-Preview-0.2.2-linux-x86_64.tar.gz.sha256 --no-launch   # CI artifact or offline copy
scripts/install_gpui_preview.sh --uninstall
```

The `v0.2.2` release predates the package, so downloads work from the first
release tagged with this workflow; until then use a `gpui-preview-<platform>`
artifact with `--archive` and `--sha256` (a `.sha256` file, a
`SHA256SUMS-gpui-preview` file or the digest itself).

| Platform | Installed (and the only paths the script owns) |
|---|---|
| macOS | `~/Applications/LocalFlow Preview.app` (bundle id `com.cortexrnd.localflow.desktop-preview`) |
| Linux | `~/.local/share/localflow-preview/`, the `~/.local/bin/localflow-preview` symlink and `~/.local/share/applications/localflow-preview.desktop` |

- It refuses an archive whose checksum is missing or wrong, whose name is not
  for this platform, or that contains links, special files, paths outside its
  root or files that don't match its `MANIFEST.json` (Linux) or bundle
  identity and signature (macOS). It also checks that `lf-desktop --version`
  runs from the unpacked copy, which proves the bundled and listed system
  libraries load on this machine.
- It refuses to replace or remove anything at its paths that it didn't install
  (identified by the bundle id, `MANIFEST.json` and a marker line in the
  desktop entry). It unpacks beside the destination, swaps it in by rename and
  restores the previous install if any step fails.
- It never touches `install.sh`'s files: `lf`, `localflow`, `LocalFlow.app`
  (`com.cortexrnd.localflow.app`), its login agent, `localflow.desktop` or
  `localflow-setup.desktop`. It adds no login item and downloads no model.
- After installing it opens the app **with no arguments** (`--no-launch`
  skips this; Linux only launches when a display is present). The window is a
  front end; to let it dictate, start it with `--engine` and press **Start
  here**: `open -a "LocalFlow Preview" --args --engine` or
  `localflow-preview --engine`.
- In real mode the preview uses the same config, `~/.localflow` data and
  keyring as the main LocalFlow app, exactly as step 2's `lf-desktop` does; its
  single-instance lock is its own (`~/.localflow/desktop/`). Uninstalling the
  preview leaves that data alone.

### Opening it from `lf` (step 4a)

The default is unchanged: `lf hub`, `lf phrases` and `lf setup` open the legacy
windows. To use the preview instead:

```bash
lf hub --gpui                 # no page: the preview's own first-run wizard still applies
lf hub --gpui --page history  # sends --page History
lf phrases --gpui             # sends --page "Hot Phrases"
lf setup --gpui               # sends --setup
export LOCALFLOW_UI=gpui      # the same for every one of these commands
lf hub --legacy               # always the legacy window; nothing is probed
```

- **Selection:** `--gpui` / `--legacy` (not both), then `LOCALFLOW_UI=gpui|legacy`,
  then legacy. Any other `LOCALFLOW_UI` value prints a notice and uses legacy.
  There is no config key. The flags apply only to the window path: `lf setup
  --cli`, `--yes`, and the `install-app` / `uninstall-app` subcommands ignore
  them and behave as before. The direct dispatchers (`python -m
  localflow.hub_window`, `phrases_window`, `setup_window`), `LocalFlow.app`, the
  menu bar, the login agent and the `install-app` aliases always stay legacy.
- **Discovery** (only after a GPUI selection, Linux and macOS only):
  `LOCALFLOW_DESKTOP_BIN` if set (an absolute path to an executable; an invalid
  one falls back to legacy and never to another install), otherwise only the
  preview installer's own package: `~/.local/share/localflow-preview/bin/lf-desktop`
  with `MANIFEST.json` `name` = `localflow-preview` (Linux), or
  `~/Applications/LocalFlow Preview.app/Contents/MacOS/lf-desktop` with bundle id
  `com.cortexrnd.localflow.desktop-preview` (macOS). `PATH`, build trees and the
  `~/.local/bin/localflow-preview` link are never searched, and nothing is
  downloaded or built. Setting `LOCALFLOW_DESKTOP_BIN` alone doesn't opt in.
  Prebuilt packages exist for Linux x86_64 and macOS arm64/x86_64 only (no ARM
  Linux); Windows and other platforms always use legacy.
- **Probe:** the candidate must answer `--help` within 5 s as `lf-desktop` with
  `--page <PAGE>`, `--setup` and the line `EXIT STATUS (launch-status: 1):`.
  That line promises the exit status the launchers rely on: 0 only when a window
  opened (and later closed) or the request was handed to the open window, 1 when
  no window could open, 2 for bad arguments. A version number alone isn't
  trusted: early unpackaged 0.2.2 builds have no Setup, and Scope 3 preview
  packages (also 0.2.2, with `--page` and `--setup`) exit 0 even when no window
  opens. **Preview packages installed before this change must be updated**
  (re-run `scripts/install_gpui_preview.sh` with a package built from this
  source) to be opened from `lf` or Linux `lf-hub`; until then `lf … --gpui`
  falls back to the legacy window and `lf-hub` prints its install hint. They can
  still be started directly. A probe that hangs is killed; nothing else ever is.
- **Launch:** a fixed argv (no shell; never `--engine`, `--sandbox` or a speech
  helper), stdin closed, stdout discarded, stderr inherited, in its own session.
  Environment (`HOME`, `LOCALFLOW_CONFIG`, …) is inherited, so it opens the same
  data as the legacy window. `lf` watches the first 1.5 s: exit 0 is a hand-off to
  an already open preview window (no second window); a non-zero exit, a failed
  start, a missing/unrecognised install or a failed probe prints one short notice
  and opens the legacy window once. A crash after those 1.5 s isn't detected.
  With the preview, `lf phrases` and `lf setup` return once the window has
  started instead of when it closes.
- **Sandbox:** with `LOCALFLOW_SANDBOX=1` (or `true`) a GPUI selection is refused
  with an error and no fallback, so a sandbox request never opens real data. Run
  `lf-desktop --sandbox` (`localflow-preview --sandbox`, or
  `open -a "LocalFlow Preview" --args --sandbox`) directly. `--legacy` keeps its
  previous behaviour.
- **`lf-hub` on Linux** (previously always an error) opens the preview through
  the same discovery, ownership and probe rules and `exec`s it with `--page` /
  `--setup` from its own arguments. If nothing usable is found it prints an
  install hint and exits 1. macOS and Windows `lf-hub` are unchanged.
- **Screen readers:** the preview has no accessibility tree; use `--legacy`, or
  `lf-hub` on macOS.
- **Not verified:** none of this has been run against a real preview window (GUI
  probing was waived by the owner); it is covered by fake-executable tests.
  Interactive macOS permission (TCC) behaviour when `lf` opens the preview
  hasn't been checked.

### Linux runtime requirements

The verifier fails the build if the binary or any bundled library needs a
symbol version newer than glibc 2.35, `GLIBCXX_3.4.30` or `CXXABI_1.3.13`, so
the package needs a distribution at least as new as Ubuntu 22.04, Debian 12 or
Fedora 36. The measured requirement is recorded in `MANIFEST.json` (the first
build of this package on Ubuntu 22.04: `GLIBC_2.35`, `GLIBCXX_3.4.29`,
`CXXABI_1.3.9`).

System libraries the binary links directly (`DT_NEEDED`; the allowlist in
`scripts/verify_gpui_package.sh`). Sherpa-ONNX and ONNX Runtime are bundled.

| Library | Debian / Ubuntu | Fedora | Arch |
|---|---|---|---|
| `libc.so.6`, `libm.so.6`, `libdl.so.2`, `librt.so.1`, `libpthread.so.0`, `ld-linux-x86-64.so.2` | `libc6` | `glibc` | `glibc` |
| `libstdc++.so.6`, `libgcc_s.so.1` | `libstdc++6`, `libgcc-s1` | `libstdc++`, `libgcc` | `gcc-libs` |
| `libasound.so.2` | `libasound2` (`libasound2t64` on Ubuntu 24.04+) | `alsa-lib` | `alsa-lib` |
| `libxcb.so.1` | `libxcb1` | `libxcb` | `libxcb` |
| `libxkbcommon.so.0` | `libxkbcommon0` | `libxkbcommon` | `libxkbcommon` |
| `libxkbcommon-x11.so.0` | `libxkbcommon-x11-0` | `libxkbcommon-x11` | `libxkbcommon-x11` |

Loaded at run time and **not** proven by the package checks (a `DT_NEEDED`
closure says nothing about `dlopen` or GPU drivers): a Vulkan loader and
driver (`libvulkan1` + `mesa-vulkan-drivers`; `vulkan-loader` +
`mesa-vulkan-drivers`; `vulkan-icd-loader` + a `vulkan-*` driver), the Wayland
client library for Wayland sessions (`libwayland-client0`; `libwayland-client`;
`wayland`) and fontconfig/FreeType (`libfontconfig1`, `libfreetype6`;
`fontconfig`, `freetype`; `fontconfig`, `freetype2`), plus a graphical session.

### macOS notes

- **Signing:** ad-hoc only. It is not Developer ID signed or notarized and has
  no Gatekeeper approval. The installer downloads with `curl`, which doesn't
  quarantine the app; an archive downloaded in a browser may be quarantined and
  need **Open Anyway** in System Settings > Privacy & Security.
- **Minimum macOS: 13.0 or later** (higher if a bundled library needs it).
  `lf-desktop` strongly links ScreenCaptureKit (GPUI's screen capture and
  `lf-meetings`' system audio). The framework first shipped in macOS 12.3, but
  `lf-meetings`' `ScreenCaptureKitSource` unconditionally configures
  `SCStreamConfiguration` audio capture (`capturesAudio`,
  `excludesCurrentProcessAudio`, `sampleRate`, `channelCount`), which Apple
  introduced in macOS 13.0. The builder therefore sets
  `MACOSX_DEPLOYMENT_TARGET=13.0` (a higher explicit value is kept; a lower
  one, including 12.3, is refused), and the verifier refuses an executable that
  records an older minimum. `LSMinimumSystemVersion` is the highest of the
  minimum OS recorded in the executable and each bundled dylib
  (`LC_BUILD_VERSION`) and the introduction version of each strongly linked
  framework in the verifier's table (ScreenCaptureKit 12.3, MetalFX 13.0); an
  image that strongly links such a framework while recording an older minimum
  fails. `LC_BUILD_VERSION` alone does not encode framework or API
  availability, and the table is not a complete SDK availability database. The
  minimum is not a list of macOS versions the app was tested on.
- **Permissions:** the preview is its own TCC identity, so microphone,
  Accessibility and Input Monitoring grants for the main LocalFlow app don't
  carry over. The usage descriptions are the main app's. Permission prompts
  were not validated interactively.
- **Speech helper:** as with `lf-hub`, the engine runs the speech model in a
  child process of the same signed executable (`lf-desktop --speech-helper`,
  an internal mode that takes no other argument and is refused in the sandbox).
  Linux runs the model in-process.

## Real mode vs sandbox

**Real mode** (the default) resolves LocalFlow's own locations through `lf_core`:

- `LOCALFLOW_CONFIG`, or `$XDG_CONFIG_HOME/localflow/config.toml`
- `~/.localflow/` for history, dictionary and hot phrases
- the API token for the local service
- the OS keyring for provider keys

Every config write reloads the latest file, validates the change with
`apply_config_update` and saves it atomically, all under `lf_ui::config_write`. So
fields you didn't touch, and changes another LocalFlow process made in the
meantime, are kept. The running LocalFlow runtime reads the same file.

**Sandbox** (`--sandbox`) works like this:

- It uses `--data-dir`, or by default `$XDG_DATA_HOME/localflow-desktop-sandbox`.
  That directory is never `~/.localflow` and never your config directory.
- On the first run it's seeded with clearly labelled example history, words,
  snippets and meeting notes. It's marked with `LOCALFLOW-DESKTOP-SANDBOX.txt`.
- It's **kept between launches**, so edits can be checked after a relaunch.
- `--sandbox-reset` re-seeds it, and refuses to delete a directory that has no
  marker. A non-empty directory that isn't a sandbox is refused too.
- History **Retry** is a labelled simulation: the failed example has a synthetic
  clip, and Retry runs the real text pipeline over the sandbox dictionary and
  snippets with a fixed transcript ("Sandbox: simulated result. No speech model
  ran."). No model, cleanup provider or key is used. Sandboxes seeded before
  this existed have no clip on that row; run `--sandbox-reset` to re-seed.
- Keys live in memory only. The OS keyring is never read or written.
- Ask, Meetings and Prompts use the existing in-process fakes, and no socket is
  opened. Ask answers with a labelled keyword match over the example history.
- Setup, model storage, key import, Test connection and the runtime status are
  simulated: downloads tick to 100 % in about 3 s, the microphone meter plays a
  canned curve (the microphone is never opened), the trial returns a fixed
  transcript, key import scans a fixed example environment, Test connection
  answers OK after 400 ms without a socket. Each simulated result carries a
  "Simulated" token. Settings → Advanced has a sandbox-only selector that steps
  Home and the sidebar through every runtime state.
- The settings assistant is the real native agent over the sandbox files with a
  scripted offline chat client ("turn off sounds", "casual tone", "hold right
  control", "add Kubernetes").
- `--engine` is refused: no microphone, keyboard listener, engine or HTTP server
  is ever started in a sandbox.
- A permanent banner and the window title say "Sandbox".

## Single window and page routing

One window runs per mode and data folder. The lock is an OS advisory lock
(`lf_runtime::singleinstance`, held for the whole process, so a crash never
leaves a stale lock and nothing compares PIDs). A second launch writes its page
(or `Setup`) with `lf_ui::hub_route::RequestFile` (whitelisted page ids, atomic
tmp + rename, per-consumer claim, 20 s TTL) and exits 0; the open window raises
itself and navigates within 250 ms. A request that arrives while the setup
overlay or a dialog is open is applied when it closes. If the owner exits between
the lock attempt and the hand-off, the new launch takes the lock and opens the
window itself, so a launch is never lost. The request file has one slot: two
launches within the same 250 ms poll leave only the later page (last wins).

| Mode | Lock and request directory |
|---|---|
| Real | `~/.localflow/desktop/` (`main-app.lock`, `page-request.json`) — separate from the legacy Hub's `~/.localflow/main-app.lock`, so `lf-hub` and the Python Hub keep their own routing until cutover |
| Sandbox | `<sandbox dir>/desktop-instance/` — kept by `--sandbox-reset`, which is refused while that sandbox is open |

## Setup wizard

**Run setup** on Home's checklist, **Run setup again** in Settings → General,
`--setup`, `--page setup`, a routed `Setup` request or a first launch open a
full-window, seven-step wizard: Checkup, Microphone, Speech model, Cleanup,
Shortcut, Try it, Review. It is built on `lf_ui::setup_flow` (`initial_choices`,
`run_checks`, `available_stt_models`, `recommend`, `config_update`) and
`lf_ui::setup_backend::FlowModelBackend`.

- Nothing is written until **Apply and finish**. Apply reloads the config under
  `config_write::with_lock` and writes only the fields the user changed since
  setup opened, so other edits — including newer values of setup fields the
  user left alone, such as other shortcuts or the recording limit — are kept. A
  changed field that was also changed elsewhere is reported as a conflict and
  nothing is saved; applying again replaces it explicitly. A failure keeps every
  choice. Closing keeps the draft for the session (Home then offers **Resume
  setup**).
- The microphone opens only after **Test microphone** (a metering stream; no
  audio is kept) and is released on leaving the step, Back, closing, the window
  losing focus (also while the device is still opening — press Test again after
  a permission prompt), or quit. The trial records at most 5 s after the model
  loads, is cancelled by the same events, keeps "Try again" disabled until the
  cancelled worker has returned, never writes History and never pastes.
- The Linux checkup adds the global-shortcut session (X11 / Wayland) and the
  paste helper (`xdotool` / `wtype`).
- Cleanup: **With Ollama** (Ollama models, pulled on request) or **No
  cleanup**; when a remote provider is active the choice is **Keep ‹provider›**
  or **No cleanup** — switching provider stays in Models & keys.
- Start at login is never changed here (macOS: `lf agent install/uninstall`).
- Downloads, the trial and model deletes are refused while a runtime reports it
  is recording or transcribing.
- **Default build:** the microphone test works; speech inspection, download and
  the trial say they need `--features runtime` (choices are still saved).
  **Runtime build:** they run for real against the Hugging Face cache.

## History retry

- **Retry** (secondary to Copy) appears for dictations with saved audio. It
  transcribes the clip again through the shared `process_clip` pipeline with the
  current settings, this window's dictionary, snippets and commands, and the
  row's own app (for its style). A failing cleanup provider keeps the
  uncleaned text, as in live dictation.
- Only the installed configured model is used; Retry never downloads, never
  picks another model and never starts dictation, the microphone or the
  shortcut listener. On macOS it runs in the speech helper with a cache-only
  load request (a helper without that support refuses before loading).
- Success replaces that row's status, text and word count in place (an atomic
  conditional update: a row deleted, cleared and reused, or edited meanwhile is
  left alone) and shows a short toast. Nothing is pasted or copied, `last.txt`
  is untouched and no clip is pruned.
- One retry runs at a time per process; the button reads **Retrying…** and stays
  focusable, and repeat presses are ignored. A failure stays as a banner on that
  row with **Try again**. Closing waits briefly for a retry and says it can't be
  interrupted.
- **Default build:** Retry is disabled with the rebuild hint. **Sandbox:** a
  labelled simulation (see Sandbox above).

## Models & keys

- **Speech → Models on this computer:** each catalogue model for this machine
  shows Not downloaded / Downloading (progress, Cancel) / Failed (Retry) /
  Downloaded (Use) / In use. Opening the tab inspects local speech-model files
  and asks the configured Ollama for its model list (one HTTP request); no model
  file is downloaded until you click Download. Stored model folders (from
  `lf_stt::model_storage`) can be deleted after a confirmation; the selected
  model and models leased by a running LocalFlow (`stt_lease`) are protected,
  and protection is re-checked at delete time. Use on a model that isn't
  downloaded saves the choice and says it isn't downloaded. Default builds show
  why these actions need the runtime feature.
- **Providers → Import keys…:** scans the environment (on click) or a chosen /
  typed `.env` file (literal `NAME=value` lines only), lists one row per key with
  provider, source variable and "Replaces saved key", defaults from
  `models_layout::import_states`. Key values are never shown, logged or put in
  messages. Importing never changes the active provider. A provider whose key
  comes from `LOCALFLOW_PROVIDER_KEY_<ID>` can't be imported (the saved key would
  never be used).
- **Test connection** (Cleanup tab and provider rows): only on click, on a
  background thread, with a 10 s timeout. The provider in use is tested as
  configured: it is built by the runtime's own factory
  (`lf_providers::factory::build_llm`, so the configured endpoint, model and
  endpoint-bound key are used) and asked for its model list (`healthcheck`:
  Ollama `GET /api/tags`, OpenAI-compatible list models). Other provider rows
  are tested as their preset with the saved key through
  `ProviderKeys::test_llm` (the same model-list probe). No prompt or user text is
  sent. The result is one of the three fixed sentences from `lf_providers::keys`.
- A key save or provider switch closes its editor only if that same editor is
  still open and nothing new was typed into it; an editor reopened or edited
  while the change was saving keeps its draft and shows the result.

## Settings assistant

"Tell LocalFlow what to change" sits above the Settings tabs (collapsed by
default). It uses `hub_settings::SettingsPageState` and the native settings agent
on this window's explicit config, dictionary and undo files (loopback Ollama
only). Preview runs on a worker; **Apply** and **Undo** run through
`settings_worker::dispatch`. A preview made stale by any settings change can't be
applied; an apply the agent refuses keeps the preview and shows why; Undo skips
(and lists) settings that changed since. Secret-looking tokens in model output
are masked. The manual Settings rows are unchanged.

## Appearance

Settings → General → **Appearance** picks System, Light or Dark (default
System, which follows the OS light/dark setting live). The choice applies to
every open window at once and is stored on its own in `appearance.json` in the
window's instance directory (`<localflow dir>/desktop/`, or inside the sandbox
directory), never in `config.toml`. A missing or unreadable file means System;
if saving fails, the row says so and the choice still applies for the session.

## Runtime connection

The app reports what it finds:

- **Local service.** It checks `GET http://{[server] host}:{[server] port}/healthz`
  (default `127.0.0.1:8756`) every 5 s on a background thread. When the service
  answers, Ask (`/api/recall`), Meetings (`/meeting/*`) and Prompts (the prompt
  queue API) work through the existing `lf_ui::backends::http` adapters, with the
  `X-Localflow-Token` header. When it doesn't answer, those pages show "LocalFlow's
  local service isn't running at …" with **Try again**. They never show empty data
  as if it were real. Start the service with `localflow` (the desktop runtime) or
  `lf serve`.
- **Dictation.** Home and the sidebar read `~/.localflow/status.json` every second
  (and this window's engine, when it runs one). Protocol 2, written by the Rust
  runtime, keeps `state`/`since`/`pid` and adds `protocol`, `instance`,
  `heartbeat` (every 5 s), `listener` (`starting` → `ok` only after the platform
  accepted the keyboard listener; `denied` / `dead` / `unsupported` when it failed
  or stopped), `session`, `mic.configured` / `mic.observed` and `warnings`. States:
  Not running, Starting, **Shortcut listening** (protocol 2 + listener ok + fresh
  heartbeat + proof the file belongs to the *current* runtime: either this
  window's own engine, whose live `RuntimeHandle` reports the file's `instance`,
  or — for an external runtime — the file's PID actually holding
  `localflow.lock` (kernel `/proc/locks`, device + inode, without taking the
  lock) *and* the file's `lock` identity matching the lock file's current
  acquisition), Recording, Transcribing, Needs attention, Not responding (dead
  PID, a lock that is free or held by another process, a file from an earlier
  lock acquisition — same-process restart or recycled PID — or a heartbeat older
  than 15 s) and status limited (the Python runtime and older Rust builds
  still write protocol 1, which is never shown as ready). Without a usable file
  Home claims only what it can prove: a missing file is Not running unless
  its own proven runtime is starting or another process holds `localflow.lock`
  ("Another LocalFlow handles dictation"). Without ownership proof, an unreadable
  file is "LocalFlow's status can't be read"; a reported status with unverifiable
  ownership is "status limited". While this window's
  engine is starting, or runs but hasn't published its own instance yet, Home
  says Starting whatever an older file claims. Limitation: on macOS an
  *external* runtime's lock owner can't be read without taking the lock (flock
  locks report no owner PID through `F_GETLK`), so an external runtime there
  stays "status limited"; this window's own engine is verified by its
  instance. On btrfs subvolumes `stat` and `/proc/locks` report different
  devices, so an external runtime there shows as not responding (never ready).
  "Shortcut listening" is the strongest claim: the microphone and speech model are
  checked when dictation starts, and the configured and observed microphone are
  shown separately. `/healthz` is a separate "Local service" line and never
  changes the lead or the sidebar dot.
- **Engine** (`--features runtime` + `--engine`, not in the sandbox): Home shows
  **Start here** / **Stop**. It reuses `lf_ui::embedded::EmbeddedRuntime` on
  LocalFlow's own data, starts only on that click, only with dictation on and
  setup done, and never when another process is *proven* to hold
  `localflow.lock` (that runtime is left alone and never stopped from here). A
  PID recorded in `status.json` or the lock file alone, or a lock owner that
  can't be read (macOS), never blocks Start; the engine's own lock decides.
  Closing the window asks first, then stops only what this window started
  (engine, model downloads) on a background thread with bounded waits. While it
  stops, a "Stopping LocalFlow…" cover blocks clicks, keyboard shortcuts, page
  routing from a second launch and the setup wizard. Right before the window
  closes, running downloads, an in-flight model deletion and pending settings or
  key writes are checked again (not from the earlier snapshot); late work keeps
  the window open with the reason. If stopping isn't confirmed (still stopping,
  or the engine failed while stopping) the window stays open, keeps ownership,
  and says so: Cancel keeps it open (close again to retry), **Quit anyway** ends
  the app without waiting.
  If the shortcut listener dies while recording, held keys are treated as
  released: a hold recording ends normally and is transcribed, while a
  hands-free/toggle or command recording is discarded instead of left running.
- **Store-backed pages** (Home, History, Dictionary, Snippets, Style, Settings,
  Models) work without the service.

## Linux global shortcuts

The Rust runtime (`lf-input`) now listens on **X11** with listen-only XInput2 raw
key events through the pure-Rust `x11rb` connection: no grab, no subprocess, no
root, `input` group or `/dev/input` access. It feeds the existing `KeyboardHub`
(hold-to-talk including a bare modifier key-up, toggle and chords, left/right
modifiers, auto-repeat suppression), releases held keys when it stops, and
re-reads the keymap on `MappingNotify`. It requires the server to agree to
XInput 2.1 or newer (before 2.1, raw events stop during another client's grab,
which could lose a release); older servers get an explicit "unsupported" with
the reason. Key events are never logged and no text
is collected. **Wayland** sessions (`WAYLAND_DISPLAY`, even with XWayland) are
reported as unsupported with the reason — compositors don't let apps see keys
pressed in other apps — instead of claiming XWayland coverage. A session without
a display is unsupported too. Pasting still needs `xdotool` (X11) or `wtype`
(Wayland); Home and the setup checkup say when it's missing. macOS keeps its
CGEventTap and Input Monitoring checks; the listener is reported ready only
once the tap is installed with Input Monitoring granted (otherwise it stays
"starting" next to the permission warning).

## Keyboard

| Keys (Ctrl on Linux, Cmd on macOS) | Action |
|---|---|
| Ctrl+1 … Ctrl+9, Ctrl+0 | Go to sidebar destination 1–10 |
| Ctrl+K or Ctrl+F | Focus the page's search or filter field |
| Ctrl+N | New snippet, new word or new question |
| Ctrl+, | Settings |
| Tab / Shift+Tab | Move focus. Every control is a tab stop with a visible 2 px ring |
| Enter / Space | Activate the focused button, switch, segment, row or menu item |
| Up / Down | Move between sidebar pages (when the sidebar is focused), history rows, or menu items |
| Delete, Ctrl+C | Delete (with confirmation) or copy the selected History entry |
| Escape | Close the dialog, menu or toast, close the setup wizard (the draft is kept), or clear the History search |
| Ctrl+Q / Ctrl+W | Quit (waits for pending writes) / close the window |

Text fields handle Unicode graphemes, IME composition, mouse and keyboard selection,
word movement, clipboard and Ctrl+Z undo. Masked key fields have no undo history and
no copy. Multi-line fields soft-wrap and grow up to a row limit. Past the limit they
scroll: the view follows the caret while you type or move, the mouse wheel scrolls,
and clicking or dragging into scrolled text selects the right characters. The
editing state is GPUI-free (`ui/text_edit.rs`) and unit-tested with long
multi-line Unicode text.

Confirmation dialogs move focus to **Cancel**. Tab and Shift+Tab stay inside the
dialog, and Escape cancels without running the action. Navigation and the
search and new-item shortcuts are blocked while a dialog is open, and focus
returns to where it was when the dialog closes.

## Known limitations

These are tracked in `docs/gpui-migration.md`:

- GPUI 0.2.2 doesn't publish an accessibility tree, so screen readers can't read
  the app. Keyboard and visual focus work.
- None of the scope-2 UI was exercised interactively: owner-waived UI probing on
  the development VM; behaviour is covered by controller, sandbox and CLI
  subprocess tests. macOS runtime behaviour (tap readiness, engine, Metal
  whisper) is type-checked in CI only.
- The engine runs only on LocalFlow's own data folders and only in builds made
  with `--features runtime` (the preview package is one).
- Wayland global shortcuts are not possible (see above). On Windows only the
  preview package is built; its tray and global hotkeys are not interactively
  verified.
- The Python runtime still writes protocol-1 status, so Home shows it as
  "Running · status limited".
- `lf-hub` / the Python Hub and this window don't route to each other until the
  cutover (separate lock namespaces on purpose).
- The preview package is opt-in and separate; `lf … --gpui` (step 4a) is opt-in
  too. Default-launch promotion (4b) and legacy retirement are later steps.
  The macOS bundle is ad-hoc signed only (no Developer ID, notarisation or
  Gatekeeper approval) and its permissions,
  speech helper and rendering were not exercised interactively.
