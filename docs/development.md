# Development

## Requirements

- Rust (stable, edition 2024). Developed with 1.97.
- A running Hyprland session (the bar needs a Wayland compositor with layer-shell).
- System libraries: `wayland`, `libxkbcommon` (with its headers), and `pkg-config` to find it at build time. Arch and CachyOS have all of them with Hyprland; Debian-like systems need `libxkbcommon-dev pkg-config`. For the GPU renderer, the default, also a Vulkan loader plus drivers (Mesa on Asahi). Without them the bar falls back to the CPU renderer.

## Build and run

```fish
cargo run                          # debug, default binary (host + built-in plugins)
cargo run --release                # what you'd actually use
ICED_BACKEND=tiny-skia cargo run --release   # the CPU renderer, like renderer = "cpu"
cargo run --release --no-default-features --features builtin   # a CPU-only build
cargo test --workspace             # every crate
cargo install --path crates/eave-build   # then `eave-build`: install what config.toml uses

```

CI ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) runs the build, tests, clippy (`-D warnings`) and rustfmt on x86_64 and aarch64 for every push to main and every pull request, then runs `install.sh` from the checkout on x86_64.

To install it and start it with your session, see [running.md](running.md). To have the installed bar built from your checkout instead of GitHub, put `eave = "~/path/to/eave"` at the top of config.toml. Then `eave-build` builds your working tree as it is, uncommitted changes included.

Without `~/.config/eave/config.toml` the bar is empty. A minimal config:

```toml
[[center]]
widget = "clock"
```

To try a config without touching your own, point `XDG_CONFIG_HOME` at another directory. See [plugins.md](plugins.md) for plugins and the config format.

Stop with Ctrl+C. To check that the surface exists and where it is:

```fish
hyprctl layers | grep eave
```

To watch the Hyprland event stream that `eave-hyprland` reads:

```fish
socat -U - UNIX-CONNECT:$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock
```

## Measuring

"Lightweight" is checked with numbers ([vision.md](vision.md)). Before and after any change to dependencies, renderer, animation or polling:

```fish
cargo build --release
ls -lh target/release/eave                     # binary size
./target/release/eave &
sleep 5
ps -o rss,pcpu -p (pgrep -x eave)              # memory (KB) and CPU
kill (pgrep -x eave)
```

Current baseline (Asahi, aarch64, release, tiny-skia, default binary). Measured 2026-10-12 after the plugin foundation (0.5). Config hot reload (0.6) left RSS and idle CPU unchanged within noise, added one thread and 65 KB of binary (5.84 MiB) ([D-21](decisions.md)):

| Config | Binary | RSS | Idle CPU |
|---|---|---|---|
| empty bar (no config) | 5.8 MiB | ~9.7 MB | 0% |
| two clocks | 5.8 MiB | ~11.8 MB | 0% |
| two clocks, popover open | | ~14.5 MB | |
| workspaces, window, clock, keyboard (after D-16/D-17) | 6.1 MiB | ~12.2 MB | 0.1% |
| + network, battery (after D-18, zbus) | 7.0 MiB | ~13.5 MB | 0.1% |
| + volume (after D-19) | 7.3 MiB | ~13.9 MB | 0.1% |
| same seven, battery and network from the kernel (after D-23) | 7.0 MiB | ~14.5 MB (unchanged: 14.6 before it, in the same session) | unchanged within noise |
| workspaces, clock, volume, network, battery, morphing islands (after D-27/D-28) | 7.1 MiB | ~14.1 MB; ~22.4 MB once a popover has opened (kept, see D-28) | 0% |
| same five, app icons on workspaces (after D-38) | 7.9 MiB | ~15.7 MB (from ~14.3 in the same session) | unchanged within noise |
| the user's config, real popovers and Wi-Fi through iwd (after D-40–D-42) | 9.0 MiB | ~16.5 MB (from ~15.7 in the same session), no popover open | unchanged within noise |
| the user's config + notifications (after D-47/D-48) | 9.7 MiB | ~17.8 MB (from ~17.1 in the same session; ~17.2 without the notifications widget) | unchanged within noise |
| workspaces, volume, network, battery, clock + media (after D-56/D-57; 9.1 MiB before) | 11.2 MiB | ~18.1 MB with no player (~16.9 on main in the same session), ~18.8–19.1 MB with a cover shown | unchanged within noise |
| the user's config, Markdown, text area, scroll (after D-62) | 11.6 MiB | ~19.3 MB (~18.8 on main in the same session) | unchanged within noise |
| the user's config, notification cards (after D-85; both renderers built in, run on tiny-skia, alone on a session bus of its own) | 18.40 → 18.53 MiB | ~23.4 MB idle, ~27.7 MB with a notification shown (main in the same session: ~23.6 and ~27.3) | unchanged within noise |
| two clocks, + `essential` and `update` under `[[dormer]]` (after D-96–D-98; CachyOS x86_64, both renderers built in, run on tiny-skia) | 20.77 → 21.98 MiB | ~18.2 MB for two clocks (~17.9 on main in the same session), ~18.4 MB with both plugins | 0% |
| a clock, + `bluetooth` with BlueZ running and a paired device (after D-103; Asahi, both renderers built in, run on tiny-skia) | 19.15 → 19.84 MiB | ~18.8 MB for the clock alone, ~19.1–19.5 MB with bluetooth, same session | 0% (one tick in 20 s either way) |
| clock, volume, network, bluetooth (after D-132/D-133: drained D-Bus signals, iwd always watched; Asahi, both renderers built in, run on tiny-skia) | 21.59 → 21.72 MB | ~22.5 → ~22.8 MB, same session | unchanged: 1–4 ticks in 20 s either way |
| two clocks, + `power` (after D-131; Asahi, both renderers built in, run on tiny-skia) | 20.09 → 20.59 MiB | ~19.1 MB for two clocks on main, ~19.4 MB on this build, ~19.6 MB with power, same session | 0%: nothing runs until the popover opens or an action is picked |
| clock, battery with ebb recording (after D-134; Asahi, both renderers built in, run on tiny-skia) | 20.71 → 23.17 MiB (SQLite and ebb's models) | ~19.9 → ~23.0 MB, same session | unchanged: 15 vs 16 ticks in 120 s |

Before the plugin split (skeleton, clock + date hard-coded): binary 5.6 MiB, RSS ~11.5 MB. The +0.2 MiB of binary and +0.3 MB of RSS are the price of the registry, config parsing (`toml`) and popover surfaces ([D-12](decisions.md), [D-14](decisions.md)).

The wgpu renderer measured 9.5 MB binary, ~72 MB RSS and ~1% idle CPU at the skeleton stage ([D-4](decisions.md)). It's the default since [D-67](decisions.md): with the user's config it measured 18.2 MB binary, 77.6 MB RSS (most of it shared driver libraries) and 0.1% idle CPU, against 14.0 MB, 21.4 MB and 0.1% on tiny-skia. The table above is tiny-skia.

Update this table when the baseline moves for a good reason, and record that reason in [decisions.md](decisions.md).

For smoothness: watch animations with Hyprland's debug overlay (`hyprctl keyword debug:overlay true`) and check that the bar's CPU only rises *while* something is animating. To measure smoothness, time the frames: how many refreshes apart they start (one at 120 Hz is 8.3 ms), not their average rate. D-128 did it with timing patched into scratch copies of iced_layershell and iced_wgpu, and the user's config on a second bar.

## Code rules

- **Events over polling.** If the data source can notify us, subscribe. Polling needs a comment explaining why no event source exists.
- **`update` does no I/O.** It mutates state and returns `Task`s. Blocking calls belong in subscriptions or tasks.
- **`view` is pure.** No side effects, no expensive computation. Precompute in `update` if needed.
- **One place for visual constants:** colors, sizes and fonts all live in `crates/eave/src/theme.rs`. Plugins never see them ([D-13](decisions.md)).
- **Dependencies:** `default-features = false` unless there's a reason. Every new crate gets one line in the PR or commit explaining why. Non-obvious ones get a `decisions.md` entry.
- **Tolerant parsing** of anything external (Hyprland events, D-Bus properties). Unknown is ignored and logged at debug level, never a panic.
- **Both arches build.** If you touch anything platform-dependent, note whether it was tested on x86_64 too.
- **config.toml may be a symlink** (a dotfiles manager's). Write through it to its target, by renaming a temporary file in the target's directory, never over the link.
- Match the surrounding code style. `cargo fmt` and `cargo clippy` should be clean.

## Versions, publishing and releases

Three things get versions, each by hand and only when the user asks for it:

- **The crates on crates.io:** `eave-api` and the services (`eave-dbus`, `eave-hyprland`, `eave-iwd`, `eave-bluez`, `eave-mpris`, `eave-net`, `eave-pulse`), so plugins in other repositories build and test on their own ([D-50](decisions.md)). The host, `eave-build` and the built-in plugins aren't published.
- **eave's release tags:** `vX.Y.Z` on `main`. The AUR package builds `eave-build` from a tag, and a packaged eave-build builds the bar from that same tag ([D-54](decisions.md)).
- **Plugin repositories** (`eaveshell/eave-claude`, `eaveshell/eave-notifications`, …): they depend on the published crates, and their CI builds with `--locked`.

### Publishing a crate

A plugin repository can only use what the published crates have. When a change in eave adds API a plugin needs (a new `ui` primitive, a new service call), publish before pushing the plugin, or its CI fails:

1. Bump `version` in the crate's `Cargo.toml` **and** its entry in the root `[workspace.dependencies]`. While we're on `0.x`, an addition is a patch bump (`0.1.0 → 0.1.1`). A breaking change bumps the minor (`0.1 → 0.2`), and every crate that depends on it needs a release too.
2. Commit the bump on `main`, with the tree clean. `cargo publish` refuses a dirty tree, and the published crate should match a commit.
3. `cargo publish -p <crate> --dry-run`, then `cargo publish -p <crate>`. A published version can't be replaced, only yanked, so dry-run first.
4. Push `main`.

Then, in the plugin repository: `cargo update -p <crate>`, raise its requirement to the version that has the feature (`eave-api = "0.1.1"`, not `"0.1"`), and run what its CI runs before pushing: `cargo build --workspace --locked --all-targets`, `cargo test --workspace --locked`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo fmt --all --check`. Inside a bar this doesn't matter, since `eave-build` patches the crates to the eave checkout it builds against. But the plugin's own CI only sees crates.io.

### Releasing eave

Bump the versions, tag `main` with an annotated `vX.Y.Z` (`git tag -a vX.Y.Z -m "eave X.Y.Z"`), and push the tag. Then update the AUR package: [dist/aur/README.md](../dist/aur/README.md) has the steps. Never move or delete a pushed tag, since the AUR package's checksum is of its tarball. A fix is a new tag.

### Merging and pushing

- Merge into `main` and push only when the user asks. Before pushing `main`, run `cargo test --workspace` on the merged tree.
- Branches that each add a decision often pick the same `D-` number. Keep the number of the branch merged first, renumber the other, and update every link to it (`grep -rn 'D-<n>'`: docs, `AGENTS.md`, code comments, CI, `dist/`).
- When several repositories change together, push in dependency order: eave (and any crate it publishes) first, then the plugins that need it.

## Docs

`AGENTS.md` is the entry point. `docs/` holds the reasoning. When behavior or a decision changes, update the relevant doc in the same change.
