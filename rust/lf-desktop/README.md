# lf-desktop: native LocalFlow desktop app (GPUI preview)

A native desktop window for LocalFlow, drawn with [GPUI](https://crates.io/crates/gpui)
(pinned `=0.2.2`). It reads and writes the stores LocalFlow already uses (config TOML,
history SQLite, `dictionary.json`, `hot_phrases.json`, keyring) and talks to the
existing local HTTP service for Ask, Meetings and Prompts.

**The migration is not complete.** This crate is step 1 of a staged migration: a
parallel GPUI front end whose library and config workflows are connected to the real
stores. It isn't the default launcher and it doesn't replace `lf hub`, `lf-hub` (the
macOS AppKit Hub), `lf setup` or the Python runtime. Installers and launch
behaviour are unchanged, and this window does not run dictation itself.

## Migration sequence and acceptance gates

Each step is its own reviewable PR. A step starts only when the previous step's
gate holds. The full feature matrix is in
[`docs/gpui-migration.md`](../../docs/gpui-migration.md).

| Step | PR scope | Acceptance gate before the next step |
|---|---|---|
| **1. Parallel front end** (this PR) | Native app beside the existing ones. Home, History, Dictionary, Snippets, Style, Settings and Models & keys are connected to the real stores and config. Ask, Meetings and Prompts use the existing local HTTP service. Isolated `--sandbox`. Dedicated `gpui.yml` CI | Linux and macOS CI green (`gpui.yml`). Legacy runtime, launchers and installers unchanged. Existing CI changed only by the Windows `whisper-rs-sys` CMake-cache cleanup in `rust.yml`. UI testing for this PR was waived by the owner; native rendering on screen is unverified (later steps keep their validation gates) |
| 2. Runtime and global-input parity | Setup wizard (model downloads, mic level, trial). Model storage. Settings assistant. Key import and Test LLM. Real runtime status (input device, listening, warnings) from the engine instead of HTTP reachability. Global hotkeys on Linux in the Rust engine. Single instance and `page-request.json` routing | Every capability the Python Hub and `lf-hub` offer works here, or the owner waives it in writing. Dictation readiness is shown only from a runtime signal |
| 3. Packaging | Installers build and ship a release `lf-desktop`. System-library requirements documented per distribution. macOS app bundle and signing | Clean install, upgrade and uninstall on each supported platform. Existing installs keep working |
| 4. Opt-in, then default-launch cutover | First an opt-in (`lf hub --gpui` or a config flag). After a soak period, `lf hub`, `lf phrases` and `lf setup` prefer `lf-desktop`, with a fallback to the current UIs | The data-safety hash check passes. Accessibility is accepted (GPUI has no accessibility tree today). Light and dark screenshots of every page are reviewed. The owner agrees |
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
```

`lf-desktop --help` lists the options:

| Option | Effect |
|---|---|
| `--page <PAGE>` | `home`, `history`, `dictionary`, `snippets` (also `"Hot Phrases"`), `style`, `ask`, `meetings`, `prompts`, `models`, `settings`. An unknown page opens Home with a notice |
| `--setup` | Opens Home with the setup checklist expanded |
| `--sandbox` | Uses isolated example data and in-process fake services (also `LOCALFLOW_SANDBOX=1`) |
| `--data-dir <DIR>` | Sandbox directory, only valid with `--sandbox` |
| `--sandbox-reset` | Re-seeds the sandbox directory |

An unknown argument exits with status 2.

The crate is a **standalone Cargo workspace** with its own `Cargo.lock`, so GPUI's
dependency tree and system libraries stay out of the `rust/` workspace and its CI.
`.github/workflows/gpui.yml` is an additive workflow that only runs when this crate
or one of its path dependencies changes:

- **Linux:** fmt, clippy, the headless tests (including the shim's) and the build.
- **macOS:** the shim tests and a full type-check.

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
| Windows | Not targeted |

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
- Keys live in memory only. The OS keyring is never read or written.
- Ask, Meetings and Prompts use the existing in-process fakes, and no socket is
  opened. Ask answers with a labelled keyword match over the example history.
- A permanent banner and the window title say "Sandbox".

## Runtime connection

The app doesn't run the speech engine. It reports what it finds:

- **Local service.** It checks `GET http://{[server] host}:{[server] port}/healthz`
  (default `127.0.0.1:8756`) every 5 s on a background thread. When the service
  answers, Ask (`/api/recall`), Meetings (`/meeting/*`) and Prompts (the prompt
  queue API) work through the existing `lf_ui::backends::http` adapters, with the
  `X-Localflow-Token` header. When it doesn't answer, those pages show "LocalFlow's
  local service isn't running at …" with **Try again**. They never show empty data
  as if it were real. Start the service with `localflow` (the desktop runtime) or
  `lf serve`.
- **Dictation.** The sidebar and the Home card report three things separately:
  - the config, meaning whether `[dictation] enabled` is set and which shortcut is configured;
  - whether the local service is reachable;
  - input capability, which is always shown as unverified.

  `/healthz` can be served by the API alone, so a reachable service never counts as
  proof that a runtime is listening to the microphone or the global shortcut. That
  holds on Linux and on macOS. Real runtime status arrives in step 2.
- **Store-backed pages** (Home, History, Dictionary, Snippets, Style, Settings,
  Models) work without the service.

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
| Escape | Close the dialog, menu or toast, or clear the History search |
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
- The setup wizard isn't included: no model downloads, mic test or trial.
- There's no in-window dictation and no tray item.
- The settings assistant isn't included.
- The app doesn't take part in single-instance or `page-request.json` routing.
- Speech-model storage management isn't included.
