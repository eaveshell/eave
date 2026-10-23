# Running eave

How to install the bar, start it with your session, update it, and what each kind of edit needs. The default is a systemd user service ([D-20](decisions.md)). Starting it from the Hyprland config works too.

## Install

### From the AUR (Arch, CachyOS)

```sh
paru -S eave
```

The package holds only `eave-build`, in `/usr/bin`, and pulls in `rust`, `git` and `pkgconf` to build the bar with ([D-54](decisions.md)). Then, as your user, the same steps as below from step 2:

```sh
eave-build init
eave-build
systemctl --user enable --now eave-rebuild.path eave.service
```

### With install.sh

You need Rust (`cargo`) and `git`:

```sh
curl -fsSL https://raw.githubusercontent.com/eaveshell/eave/main/install.sh | sh
```

[`install.sh`](../install.sh) does these steps, and you can do them by hand too:

1. `cargo install --locked --git https://github.com/eaveshell/eave eave-build`
2. `eave-build init`: writes a starter `~/.config/eave/config.toml` (an existing one is kept) and the systemd user units into `~/.config/systemd/user/`, then reloads systemd.
3. `eave-build`: clones eave into `~/.local/share/eave/src`, builds the widgets config.toml uses, and installs `~/.local/bin/eave`. The first build takes a few minutes.
4. `systemctl --user enable --now eave-rebuild.path`, and `eave.service` when `graphical-session.target` is active (see [below](#as-a-systemd-user-service-recommended)). Otherwise it tells you what to add instead.

Running it again repairs an install and never updates one. `EAVE_REPO=<git URL or checkout>` installs from a fork or a local checkout.

## What an edit needs

| You edit | What happens |
|---|---|
| `~/.config/eave/config.toml`, moving widgets or changing options | Nothing to do. The bar reloads it as soon as you save ([D-21](decisions.md)). Widgets whose entry didn't change keep their state. A file that doesn't parse is reported on stderr and the current bar stays. |
| `~/.config/eave/config.toml`, `renderer` | Used from the bar's next start: `systemctl --user restart eave.service` ([D-67](decisions.md)). |
| `~/.config/eave/config.toml`, `notch`, `screen` or `islands` | Nothing to do: the bar reloads it ([D-71](decisions.md), [D-73](decisions.md)). |
| `~/.config/eave/config.toml`, using a widget that isn't compiled in yet | A rebuild: `eave-build`, run for you by the [path unit](#rebuild-automatically-when-configtoml-changes) ([D-22](decisions.md), [D-30](decisions.md)). |

`eave-build` builds, and if the new binary differs from `~/.local/bin/eave`, installs it and restarts a running bar on it. It uses `systemctl --user restart eave.service` when the service runs the bar. Otherwise it stops the bar and relaunches it through Hyprland. A failed build installs nothing and leaves the running bar alone. If no bar is running, it doesn't start one.

## Update

```sh
eave-build update
```

Nothing changes version until you run this ([D-31](decisions.md)). It fetches eave and every plugin, moves each to the newest commit of its `@rev` (or its default branch), prints what moved, and rebuilds as above. When eave itself moved, it first reinstalls `eave-build` from the new checkout and lets the new one do the rest, so the bar is always built by the matching `eave-build`. It also runs `init`, which rewrites the systemd units (config.toml is kept), and reloads systemd ([D-93](decisions.md)). To stay on a version, pin it: `eave = "eaveshell/eave@<tag>"`, `widget = "someone/weather@v0.2"`.

With the AUR package, eave itself is pinned to the package's release: a config.toml without `eave = …` builds `eaveshell/eave@v<version>`. `paru -Syu` moves eave and `eave-build` together, and the next `eave-build` moves the bar to the new release. Run `eave-build init` after an upgrade to refresh the units. `eave-build update` still updates your plugins, and never reinstalls `eave-build` over the package ([D-54](decisions.md)).

To see what an update would move first:

```sh
eave-build check
```

It fetches eave and every plugin like `update`, and prints what would move and by how many commits, but checks nothing out and builds nothing ([D-97](decisions.md)).

From the dormer, the `update` widget does both: Check for updates and Update eave ([D-98](decisions.md)). They run in two systemd user units that `eave-build init` writes, `eave-update-check.service` and `eave-update.service`, not in the bar: an update restarts the bar, and its unit carries on. With `--summary` there, eave-build leaves what moved, or why it failed, in `~/.cache/eave/check.toml` or `update.toml`. The widget tells it in a notification and removes the file, and a check that found updates offers Update on its notification. Their output is in `journalctl --user -u eave-update` (or `-u eave-update-check`). Nothing is checked or updated unless you pick it.

## Uninstall

```sh
systemctl --user disable --now eave.service eave-rebuild.path
rm ~/.config/systemd/user/eave{,-rebuild,-update,-update-check}.* ~/.local/bin/eave
rm -r ~/.local/share/eave ~/.cache/eave     # sources and build cache
cargo uninstall eave-build                  # or, from the AUR: pacman -R eave
```

`~/.config/eave` is yours to keep or remove.

## As a systemd user service (recommended)

The release build aborts on a panic ([D-10](decisions.md)). Under systemd, `Restart=on-failure` brings the bar back, logs go to the journal (`journalctl --user -u eave`), and `systemctl --user status eave` shows its memory.

```fish
systemctl --user enable --now eave.service   # eave-build init put the unit in place
```

The unit starts with `graphical-session.target` and needs the session's environment. Two things must be true:

1. **systemd knows `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE`.** Without them the service can't find the compositor. The unit has `ConditionEnvironment=WAYLAND_DISPLAY`, so it's skipped instead of crash-looping. Check with `systemctl --user show-environment`.
2. **`graphical-session.target` is active.** Check with `systemctl --user is-active graphical-session.target`.

**With [uwsm](https://wiki.hyprland.org/Useful-Utilities/Systemd-start/)** (Hyprland's recommended launcher) both are taken care of. Nothing else to do.

**Without uwsm**, nothing starts `graphical-session.target`, and it can't be started by hand. Add a target that pulls it in, `~/.config/systemd/user/hyprland-session.target`:

```ini
[Unit]
Description=Hyprland session
BindsTo=graphical-session.target
Wants=graphical-session-pre.target
After=graphical-session-pre.target
```

Then import the environment and start the target from the Hyprland config. Use one command, so the import finishes before the target starts:

```lua
-- hyprland.lua (Hyprland 0.56+)
hl.on("hyprland.start", function()
	hl.exec_cmd("dbus-update-activation-environment --systemd --all && systemctl --user start hyprland-session.target")
end)
```

```ini
# hyprland.conf (older Hyprland)
exec-once = dbus-update-activation-environment --systemd --all && systemctl --user start hyprland-session.target
```

Remove any line that starts eave directly, or you'll get two bars.

Without uwsm, nothing stops the target when Hyprland exits. That only matters if you log out and back in without rebooting: the old `eave.service` keeps the old environment. `systemctl --user stop hyprland-session.target` before exiting fixes it.

## From the Hyprland config

No systemd involved. The catch is that nothing restarts the bar if it crashes.

```lua
-- hyprland.lua (Hyprland 0.56+)
hl.on("hyprland.start", function()
	hl.exec_cmd("~/.local/bin/eave")
end)
```

```ini
# hyprland.conf (older Hyprland)
exec-once = ~/.local/bin/eave
```

`eave-build` restarts the bar in this setup too. It finds the bar by its binary (`~/.local/bin/eave`), stops it, and relaunches it through Hyprland's request socket with `eval hl.exec_cmd(…)`, falling back to `dispatch exec` on older Hyprland. It doesn't need `hyprctl` or `kill` installed. That way the new bar gets Hyprland's environment and doesn't belong to your terminal. A dev build running from `target/` is left alone.

## Rebuild automatically when config.toml changes

A systemd path unit runs `eave-build` whenever `config.toml` changes. The installer enables it. By hand:

```fish
systemctl --user enable --now eave-rebuild.path   # eave-build init put the units in place
```

- A save that doesn't change which widgets are used finds nothing to compile, installs nothing and doesn't restart the bar. It costs a cargo freshness check at idle priority, about a second.
- The rebuild runs at idle CPU and I/O priority, so it never makes the desktop stutter.
- A failed build sends a notification (`notify-send`, or Hyprland's own `hyprctl notify` when no notification daemon is running). The old bar keeps running. Details are in `journalctl --user -u eave-rebuild`.
- Saving while a build runs doesn't start a second build, and that save isn't queued either. If you started using another widget mid-build, save again once the build is done.
- It works whether the bar runs as the service or from the Hyprland config. In the second case it relaunches the bar through Hyprland's socket. If systemd's environment has no `HYPRLAND_INSTANCE_SIGNATURE`, it uses the one Hyprland that's running, and it gives up (leaving the old bar running) only when there are several.
- `eave-build init` writes the real path into `PathChanged=` when `XDG_CONFIG_HOME` isn't `~/.config`.
- `eave-build init` writes its own full path into `ExecStart=`. systemd looks up a bare command name only in `/usr/bin` and `/usr/local/bin`, not in `~/.cargo/bin`.

## Hyprland's layer animation

Turn off Hyprland's `layersIn` animation, or the bar squeezes and stretches whenever a popover opens or closes ([D-123](decisions.md)):

```lua
-- hyprland.lua (Hyprland 0.56+)
hl.animation({ leaf = "layersIn", enabled = false })
```

```ini
# hyprland.conf (older Hyprland)
animation = layersIn, 0
```

The bar's surface grows to make room for a popover and shrinks back when it closes. Besides opening layers, `layersIn` animates their box on every resize. Hyprland then draws the bar into a box still sliding to its new height, so opening a popover flattens the whole bar, notch included, for a moment, and closing one stretches it. It's on by default: `layersIn` inherits from `layers`, and that from `global`. Turning off `layers` isn't enough if your config sets `layersIn` itself. Layers still fade in, which is `fadeLayersIn`; only a `slide` or `popin` style on other layers, like a launcher's, goes. `eave doctor` warns while it's on.

## Calibrate the screen

`eave calibrate` measures how your screen's corners are rounded, and its notch, if it has one, so the bar fits them exactly ([D-71](decisions.md)). It covers the focused output with a model of what the screen shows: a one-pixel red line just inside its edge, and blue over what it says is hidden. Change the model until you can see every red pixel and no blue one. Look closely at the corners and around the notch.

| Key | Does |
|---|---|
| ↑ ↓, `k` `j`, Tab, Shift+Tab | pick a row |
| ← →, `h` `l` | change it by a pixel (smoothing by 0.05; 0 is a circle's corner, more a squircle's) |
| Shift with ← → | ten pixels (0.2 smoothing) |
| Space | turn the notch on or off, or move it between hanging from the top and a cut-out below it |
| `?` | fold the panel away, and back |
| Enter, `s` | save to config.toml and quit |
| Esc, `q` | quit without saving |

It starts from what config.toml says for this monitor, or from the notch the hardware reports. Saving writes `notch` and `screen` into the monitor's `[monitor."…"]` table ([D-94](decisions.md)): the one that already matches it, or a new one named after its make and model, so the same config.toml fits every screen you use. It keeps the notch's and the screen's `grow` as they were, the top-level keys, the rest of the file and its comments, and writes through a symlink. Run it with the binary the bar runs, `~/.local/bin/eave calibrate`, or `cargo run --release -- calibrate` in a checkout. The `essential` widget's Calibrate screen, in the dormer, runs it for you. What it saves is in screen pixels, so it holds at any scale: change the monitor's scale and the bar fits the panel again on its own ([D-115](decisions.md)).

## Check the setup

```sh
eave doctor
```

It checks what the bar depends on and says what's wrong, and what to do about it ([D-96](decisions.md)):

- config.toml is there and reads, and what the bar would warn about in it: unknown keys, odd values.
- Every widget it names is in this build and takes its options.
- No two actions in the dormer want the same key.
- Hyprland answers. The monitor the bar opens on has its notch measured, if the kernel shows one.
- Hyprland loads hypr-eave, and the one eave-build installed last, not an older copy ([D-113](decisions.md)). It's optional: a warning only when it's built and not loaded, or the glass theme needs it.
- Hyprland doesn't animate the bar's size: its `layersIn` animation is off (see [Hyprland's layer animation](#hyprlands-layer-animation)).
- `eave.service` and `eave-rebuild.path` are enabled, systemd knows `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE`, and the update units are installed. Asked over D-Bus, and skipped without a systemd user manager.
- With a battery widget on a machine with a battery: ebb's collector (`ebb-collector.service`) is enabled, so the time left is learned rather than the kernel's guess. A warning, with how to install it, when it isn't ([D-134](decisions.md)).

It doesn't need the bar, so it helps most when the bar won't start. It exits with 1 when it found a problem. Run it with the binary the bar runs, `~/.local/bin/eave doctor`: it checks the widgets that binary has. `--report <path>` also writes what it found as TOML. The `essential` widget's Check health, in the dormer, runs it that way and shows the report in a dialog.

## The dormer

The dormer is a search dialog for what your widgets can do: mute, skip a track, switch to a window, or launch an app with the `apps` widget ([D-84](decisions.md)). Bind a key to it:

```lua
-- hyprland.lua (Hyprland 0.56+)
hl.bind("SUPER + SPACE", hl.dsp.event("eave-dormer"))
```

```ini
# hyprland.conf (older Hyprland)
bind = SUPER, SPACE, event, eave-dormer
```

`eave dormer` does the same from a script or another tool. Type to narrow the list, ↑ ↓ (or Tab) to pick, Enter to run, Esc to close. To launch apps from it, add the `apps` widget in a `[[dormer]]` entry of config.toml:

```toml
[[dormer]]
widget = "apps"
terminal = "foot"   # optional: for apps that run in a terminal
```

## Shortcuts

Some popovers and actions have keys of their own, shown as keycaps on their rows: in eave-claude's popover, `1` to `9` and `0` open the first ten chats. They work only while that popover, or the dormer, is open. Give one another key, or none, in its widget's entry of config.toml, by the name its README gives it ([D-91](decisions.md)):

```toml
[[right]]
widget = "fran/eave-claude"
keys = { chat-1 = "alt+1", chat-10 = false }

[[right]]
widget = "volume"
keys = { mute = "ctrl+m" }   # in the dormer, Ctrl+M mutes
```

A key is modifiers (`ctrl`, `alt`, `shift`, `super`) and one key (a character, `enter`, `space`, `tab`, `up`, `down`, `left`, `right`), joined with `+`. In the dormer a key needs Ctrl, Alt or Super, since plain keys type into its search. When two widgets want the same key there, your keys win over the widgets' own, and then the widget earlier in config.toml.

eave takes no key outside its popovers. To open a popover or run an action from anywhere, bind a key in Hyprland, like the dormer's:

```lua
-- hyprland.lua (Hyprland 0.56+)
hl.bind("SUPER + C", hl.dsp.event("eave-popover claude"))       -- opens Claude's popover, or closes it
hl.bind("SUPER + M", hl.dsp.event("eave-action volume mute"))   -- mutes, or unmutes
```

```ini
# hyprland.conf (older Hyprland)
bind = SUPER, C, event, eave-popover claude
bind = SUPER, M, event, eave-action volume mute
```

`eave popover <widget>` and `eave action <widget> <id>` do the same from a script or any other shortcut tool. `<widget>` is the name in `widget = "…"`, without its owner (`claude` for `fran/eave-claude`). The built-in widgets' action ids are listed in [plugins.md](plugins.md#action-ids).

## Rebooting into another system

The `power` widget reboots into the boot loader's other entries with no setup when logind knows them, as it does with systemd-boot. The rest need root to pick the next boot, so eave ships small system units that do only that, and a polkit rule that lets the user at the machine start them and nothing else ([D-131](decisions.md)):

| Target | config.toml | Unit | Runs |
|---|---|---|---|
| macOS on Apple Silicon | nothing: found when the unit is installed | `eave-boot-macos.service` | `asahi-bless --next --set-boot-macos` |
| a GRUB entry | `boot = [{ name = "Windows", grub = "Windows Boot Manager (on /dev/nvme0n1p1)" }]` | `eave-boot-grub@.service` | `grub-reboot <entry>` |
| a firmware boot entry | `boot = [{ name = "Windows", efi = "Windows Boot Manager" }]` | `eave-boot-efi@.service` | `efibootmgr --bootnext <####>` |

The AUR package installs them. With install.sh, install them from a checkout of eave:

```sh
sudo install -Dm644 dist/power/*.service -t /usr/lib/systemd/system
sudo install -Dm644 dist/power/50-eave-boot.rules -t /usr/share/polkit-1/rules.d
```

Then open the power popover again: it looks for them each time it opens. A target whose unit isn't installed doesn't show, and the bar's log says so.

- **GRUB** picks a one-time entry only with `GRUB_DEFAULT=saved` in `/etc/default/grub`, and `grub-mkconfig -o /boot/grub/grub.cfg` run since. An entry is its menu title (`sudo grep menuentry /boot/grub/grub.cfg`) or its id, and a submenu's with `>`: `"Advanced options>Arch Linux, with linux-lts"`.
- **Firmware entries** are named by their description, as `efibootmgr` lists them. They work under any boot loader, Limine included, on machines whose firmware keeps its variables (not U-Boot on Apple Silicon).
- If the helper fails, nothing reboots, and the row says why.

## Glass

`theme = "glass"` makes the islands liquid glass: what's behind them shows through, lightly frosted and magnified, and bent at their edges ([D-112](decisions.md)). That takes `hypr-eave`, a Hyprland plugin in this repository. `eave-build` builds it against the Hyprland you have whenever Hyprland's headers are installed (`pkg-config hyprland`; on Arch they come with the `hyprland` package). It installs it as `~/.local/share/eave/hypr-eave.so`. Nothing loads it for you: add it to Hyprland's config. While Hyprland doesn't load it, every `eave-build` prints this line, and `eave doctor` warns:

```lua
-- hyprland.lua (Hyprland 0.56+)
hl.plugin.load(os.getenv("HOME") .. "/.local/share/eave/hypr-eave.so")
```

Without it, the glass theme looks like the dark one. In every theme, it also puts the pointer under the notch and the grown edge, as under a physical notch ([D-113](decisions.md)). `plugin:hypreave:cursor_under = false` turns that off. A plugin only loads into the Hyprland it was built for. After updating Hyprland, run `eave-build`, then restart Hyprland. A rebuilt plugin also takes effect only when Hyprland restarts: loading the same path again gets the code already in memory.

Over something bright, an island's text turns dark so it stays readable. Popovers, the dormer and dialogs are glass too; the dormer, and a dialog that drops from the notch, once they let go of it. The notch, the grown edge, islands attached to an edge and a solid bar stay black: they stand for the screen's border. Tune it in config.toml:

```toml
theme = "glass"

[glass]
band = 24          # how far in from the edge the bevel curves down, logical px
thickness = 32     # how thick the glass is: thicker bends more, and folds near the rim
gap = 24           # how high it floats over what's behind: wider turns more of it over
chroma = 0.2       # red and blue bending apart
rim = 1            # the edge's reflection and its lit side
frost = 2          # 0 is clear glass
depth = 2          # 1 to 6: how far the frost reaches
tint = 0.35        # 0 to 1: how much it takes the color behind it
ink = "readable"   # "own", "invert" or "readable": grey text and symbols over glass
contrast = 7       # with "readable": the contrast ratio it reaches
ink-blur = 12      # how wide an area the ink's color is chosen over
```

To turn it off for every client at once: `plugin:hypreave:enabled = false` in Hyprland's config. `hyprctl hypreave` shows what the plugin knows: the surfaces with glass, their regions and look, its reports and buffers, and which monitors draw the pointer in software for the notch.

## Notifications

With `widget = "eaveshell/eave-notifications"` in config.toml, eave is your notification daemon ([D-48](decisions.md), [D-50](decisions.md)). Only one program can be. If mako, dunst or swaync is already running, eave waits in line and takes over as soon as it quits. Until then, the bell is dimmed and the popover says which daemon has them. To switch, stop starting the other daemon (remove its `exec-once`, or `systemctl --user disable --now mako`), then quit it.

Notifications sent while the bar restarts, after a rebuild for example, are lost, and the list starts empty after a restart.
