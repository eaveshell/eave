# Plugins

Everything in the bar is a plugin, including the clock. A plugin is an ordinary Rust crate that provides one widget, an `eave_api::Widget`. It lives in a git repository, alone or with others, and gets compiled into the bar by `eave-build`. Nothing is loaded at runtime ([D-12](decisions.md)).

## Using plugins

`~/.config/eave/config.toml` is the only file you edit. It says what appears where, and the same `widget = "…"` says where each widget comes from ([D-30](decisions.md)):

```toml
[[left]]
widget = "workspaces"                 # built in

[[center]]
widget = "clock"
format = "%a %d %b  %H:%M"            # every key except `widget` goes to the widget

[[right]]
widget = "someone/weather"            # a plugin: github.com/someone/weather
units = "metric"
```

The zones are `left`, `center` and `right`, in order. Every key except `widget`, `width_group`, `collapsible`, `keys` and `parts` goes to the widget's `Config`.

Above the zones, `theme` is `"system"` (the default), `"light"` or `"dark"`. `system` follows the desktop's dark mode, as the settings portal tells it, and is dark when the desktop has no preference ([D-92](decisions.md)). It changes live.

`renderer = "cpu"` draws the bar on the CPU (tiny-skia) instead of the GPU (wgpu, the default). It's lighter on memory but less smooth in large popovers, and it takes effect when the bar restarts ([D-67](decisions.md)).

Two more keys there describe the screen. `eave calibrate` measures them for you and writes them ([running.md](running.md#calibrate-the-screen), [D-71](decisions.md)):

```toml
notch = { width = 370, height = 66, radius = 20, smoothing = 0.6, shoulder = 8, top = 0, grow = [4, 4] }
screen = { corners = [20, 20, 0, 0], smoothing = 0.6, inset = [0, 0, 0, 0], grow = 4 }
```

- `notch = "auto"` (the default) finds a MacBook's on Asahi, and `notch = false` ignores it. The bar splits the center zone around it ([D-69](decisions.md)).
- Everything is in physical pixels.
- A notch's `radius` rounds its lower corners. `shoulder` rounds it outwards where it meets the top edge, and islands attached to the top then flare the same way ([D-72](decisions.md)). `top` above 0 makes it a cut-out that far below the edge.
- The bar draws the notch black. `grow = [sides, below]` draws it bigger than it is, by `sides` on each side and `below` at the bottom, and the bar goes around it at that size ([D-75](decisions.md)). It's `[0, 0]` by default.
- With a notch, the islands end where it does, grown: their height plus the gap above them is the notch's bottom ([D-76](decisions.md)).
- `smoothing` 0 is a circle's corner, and up to 1 a squircle's.
- A screen's `corners` go top left, top right, bottom right, bottom left. `inset` goes top, right, bottom, left: rows and columns the bezel hides, which the bar keeps off. Either takes one number for all four.
- A screen's `grow` draws its top edge that much thicker, in black, rounded into the sides by its top `corners`. Islands, their shoulders and a notch hanging from the top all hang from the grown edge, so the notch keeps its depth and the islands their height ([D-81](decisions.md)). It's `0` by default. `eave calibrate` keeps it, and the notch's `grow`, as they were.

The islands take their corners from these ([D-73](decisions.md)):
- The top corner of the first island on the left, and of the last on the right, follows the screen's corner at an even distance (its radius less the bar's 6 px gap), and never less than a 4 px squircle, which a nearly square screen gets. Against a corner rounder than the island can follow, the island moves in until the gap is whole ([D-95](decisions.md)).
- A center island next to the notch is rounded like the notch on that side: its top corner like the notch's shoulder, its bottom corner like the notch's lower corner.
- Every other corner follows `islands`, also in physical pixels:

```toml
islands = { radius = "auto", smoothing = "auto" }   # or radius = 24, smoothing = 0.6
```

`"auto"`, the default, takes the notch's `radius` and `smoothing`, then the screen's top corners at an even distance (at least 4 px). Without a `screen` or a measured notch, the islands keep the bar's own 14 px squircle corners.

One config.toml can describe several monitors: a `[monitor."…"]` table gives `notch`, `screen` and `islands` for the monitors it names, and the bar uses the table of the monitor it opens on ([D-94](decisions.md)). `eave calibrate` saves into one of these.

```toml
notch = false   # the top level: every monitor without a table of its own

[monitor."eDP-1"]   # by connector
notch = { width = 370, height = 66, radius = 20 }
screen = { corners = [20, 20, 0, 0] }

[monitor."ViewSonic Corporation XG2431"]   # by what Hyprland's description starts with
screen = { corners = 12 }
```

- A table matches a monitor named after its connector (`hyprctl monitors` shows the `name`), or one whose `description` (make, model, serial) starts with the table's name. Leave the serial out to cover every monitor of that model.
- A table named after the connector wins. Otherwise the longest matching description wins. With none, the top-level keys apply.
- A key the table leaves out comes from the top level, whole: a table with only `screen` gets the top-level `notch`.
- The bar picks the table when it starts and on every reload, for the monitor it opened on. It doesn't move to another monitor while it runs.

Two more say when the bar and the notifications show ([D-88](decisions.md)):

```toml
bar = { show = "always", fullscreen = "hide", peek = true }
notifications = { fullscreen = "critical" }
```

| Key | Values | Default | |
|---|---|---|---|
| `bar.show` | `"always"`, `"hover"` | `"always"` | `hover`: the bar hides above the screen and slides out when the pointer touches the top edge. It keeps no room for itself, so windows reach the top. |
| `bar.fullscreen` | `"hide"`, `"hover"`, `"show"` | `"hide"` | On a workspace with a fullscreen window (a maximized one doesn't count). `show` and `hover` put the bar over the window. |
| `bar.peek` | `true`, `false` | `true` | A bar shown on hover slides out for two seconds when you switch workspace. |
| `bar.style` | `"islands"`, `"solid"` | `"islands"` | `solid` draws the whole bar black, down to the islands' bottom. The islands still work as islands inside it. A popover, the notifications and the dormer flow out of the bar's bottom, with shoulders as round as the islands' corners ([D-89](decisions.md)). |
| `bar.corners` | `"round"`, `"square"` | `"round"` | Where a solid bar's bottom meets the screen's sides: curving into them like the screen's top corners, or square. |
| `notifications.fullscreen` | `"show"`, `"critical"`, `"hold"` | `"critical"` | In fullscreen, `critical` shows only critical notifications and `hold` none. The others wait, without running out, and show when fullscreen ends. |

Whatever these say, a notification that shows brings the bar out with it, until it's gone. So does the dormer. A widget's nudge (a volume change, say) peeks a bar shown on hover, the way it brings out a collapsed item. A bar hidden over a fullscreen window stays hidden for those.

`keys = { id = "ctrl+k", other = false }` gives the widget's shortcuts other keys, or none, by the names its README lists. It's the host's, like `width_group` ([D-91](decisions.md), [running.md](running.md#shortcuts)).

`parts = [...]` puts other widgets inside this one's popover, in the places it leaves for them, its **bays** ([D-109](decisions.md)). Each part is a widget reference, or a table with the widget's options, and `keys` for its own shortcuts:

```toml
[[right]]
widget = "media"
parts = ["someone/lyrics", { widget = "someone/spotify", client_id = "…" }]
```

A part shows only in its owner's popover, never on the bar, and only where it has something to say: a Spotify part under Spotify's controls, not Firefox's. Its actions are in the dormer as usual. A widget that can be a part can usually sit on its own too: put the same entry in a zone instead, and it gets its own item and popover. Which bays a widget has is in its README; the built-in ones are under [Bays](#bays).

`width_group = "name"` lines items up: items in the same island with the same group all get a slot as wide as the widest of them, each centered in its slot ([D-34](decisions.md)). For example, put `volume` and `network` in one group, so the Wi-Fi icon gets the same room as the volume item, which needs space for its percent.

A zone is one island, or several side by side. Entries without a widget arrange them ([D-55](decisions.md)):

```toml
[[right]]
widget = "volume"
collapsible = true     # hidden until you open the island

[[right]]
divider = true         # a thin line before the next item

[[right]]
widget = "network"

[[right]]
island = true          # what follows is another island, beside this one

[[right]]
widget = "battery"
```

- `island = true` starts another island in the same zone. Add `live = true` in the same entry to make it a live island, snug around its items, with corners like any island where it sits ([D-60](decisions.md), [D-78](decisions.md)). A marker before the zone's first item sets the first island.
- `attach = ["top", "left"]` in an island's entry makes it flush with those screen edges, as if it grew out of them ([D-70](decisions.md)). Only the first island on the left can attach left, and only the last on the right can attach right. A center island next to the notch that attaches to the top is fused to it: it reaches under the notch and down as far as it does, so the two read as one shape. Without `attach`, it sits a gap away from the notch ([D-73](decisions.md)).
- `divider = true` puts a line before the next item. It shows only between two items that show.
- `collapsible = true` hides the item while its island is collapsed, which it is at first. Hover over the island and a chevron appears on the side facing the middle of the screen. Press it to show the hidden items in their places, one after another from the chevron, and again to hide them, the furthest first. A collapsed item whose widget nudges (the volume going up, say) comes out by itself for two seconds after the last nudge, then tucks back.

Notifications open from the right zone's last island, the one at the screen's edge.

Unknown widgets and invalid options are reported on stderr and skipped. They never crash the bar. Changes apply as soon as you save, with no restart. Widgets whose entry didn't change keep their state, and a file that doesn't parse leaves the bar as it was ([D-21](decisions.md)).

### Widget references

```
<source>[@rev][#widget]
```

| You write | Comes from | Widget |
|---|---|---|
| `clock` (no `/`) | built in: the `clock` plugin of eave's own collection | `clock` |
| `someone/weather` | `https://github.com/someone/weather` | `weather` |
| `someone/eave-weather@v0.2` | the same, at tag `v0.2` (or any commit or branch) | `weather`: a leading `eave-` is dropped |
| `codeberg.org/x/eave-mail`, `https://…`, `git@host:x/y.git` | any git repository | `mail` |
| `~/code/eave-weather`, `./weather`, `/abs/path` | a local directory, for working on a plugin | `weather` |
| `someone/tools#cpu` | `github.com/someone/tools` | `cpu`: picks one plugin of a repository that has several |

Built-in widgets come with eave, so they take no `@rev`. The same plugin can appear many times, but always at one revision. An optional top-level `eave = "<source>[@rev]"` says where eave itself comes from. It defaults to `eaveshell/eave`, and `eave = "~/code/eave"` builds from a local checkout.

### Compiling plugins in

The binary in `~/.local/bin/eave` holds exactly the widgets your config.toml uses. When the config starts using another one, `eave-build` compiles it in:

```fish
eave-build           # build what config.toml uses; install and restart the bar if it changed
eave-build update    # fetch newer eave and plugins first, then the same
```

The installer sets up `eave-rebuild.path`, which runs `eave-build` on every save ([running.md](running.md#rebuild-automatically-when-configtoml-changes)). A save that only moves widgets or changes options builds nothing new and doesn't restart the bar. Until a new plugin is compiled in, the bar reports it as not compiled in and skips it.

**Nothing updates by itself** ([D-31](decisions.md)). Git sources are cloned once, to `~/.local/share/eave/src`, and stay at that commit. `eave-build update` fetches every source, moves unpinned ones to their default branch and pinned ones to the newest commit of their `@rev`, and prints what moved. Changing a pin in config.toml checks out that revision on the next build, fetching it if needed. Removing a pin leaves the checkout where it is until the next update.

The generated crate builds in `~/.cache/eave/build`, so rebuilds are incremental. The plain `cargo run --release` in this repo builds the default binary, which has every built-in plugin and ignores the config's sources.

## Built-in plugins

Use them by name. The default binary (`cargo run --release`) has all of them, and an installed bar only those config.toml uses.

| Widget | Shows | Options |
|---|---|---|
| `clock` | time, the date while hovered, calendar in the popover | `format` (strftime, default `%H:%M`), `hover_format` (strftime, shown on hover, default `%b %-d`: `Oct 21`) |
| `workspaces` | Hyprland workspaces that have windows, plus the active one. Each shows its name, and with `icons` its apps' icons (two at most, then `+n` for the other windows), and one icon per window while the pointer rests on it. Workspaces you aren't on are dimmed. Click to go, right click to list its windows (click one to focus it), scroll to move | `persistent` (workspaces 1..N always shown, even empty, default 0), `icons` (default false) |
| `window` | focused window title, hidden when nothing has focus; in the dormer, close the focused window or toggle it fullscreen or floating | `max_chars` (default 60) |
| `battery` | battery icon filled to the charge (green, its charge rippling and gulping while charging, and the hovered percent gulping too), warning/critical tones, the percent while hovered; time left and power draw in the popover. With [ebb](https://github.com/francescomengarelli/ebb) recording, the time left is learned from how the machine is used and comes with a range, and the popover also shows what you seem to be doing and, live, which programs draw the power ([D-134](decisions.md)). `eave doctor` says how to install ebb; a notification when the charger goes in (for 4 s, [D-102](decisions.md)), when it gets critical (taken away once plugged in) and when it's done charging, sent to whichever notification daemon runs; hidden without a battery | `warning` (default 20), `critical` (default 10), `notify` (default true), `charging` (default true: the notification when the charger goes in) |
| `bluetooth` | the Bluetooth rune, dotted on both sides while a device is connected, dimmed while off, with nothing on hover; power, paired devices to connect and disconnect, and devices nearby to pair (confirming or typing a code) in the popover, which looks for devices only while open; a device's battery, address and Forget on its page; in the dormer, Bluetooth on and off, connect and disconnect paired devices. Needs BlueZ ([D-103](decisions.md)) | `show` (`"always"` (default), `"on"`: while Bluetooth is on, `"connected"`: while a device is connected) |
| `network` | Wi-Fi icon with as many bars as the signal, wired icon, struck-through Wi-Fi when offline; name, signal, band, address, gateway in the popover; in the dormer, Wi-Fi on and off, restart Wi-Fi, leave the network, join saved and open ones in range ([D-132](decisions.md)) | `show_name` (default false): the Wi-Fi name next to the icon |
| `volume` | speaker icon with 0–3 waves for the volume, struck through when muted; the percent while hovered and for 1.5 s after a change, with a nudge; muting nudges down, unmuting nudges up and shows the percent ([D-101](decisions.md)); scroll to change, right click to mute; −/+, mute, output list in the popover; a device that arrives (Bluetooth, USB, a jack) becomes the default ([D-133](decisions.md)) | `step` (default 5), `max` (default 100), `follow_new` (default true) |
| `brightness` | a sun whose eight rays light up with the screen's brightness; the percent while hovered and for 1.5 s after any change, with a nudge (brightness keys too); scroll to change; sliders for the display and, when there is one, the keyboard backlight in the popover; hidden without a backlight. Percents follow a perceptual curve ([D-80](decisions.md)) | `step` (default 5), `min` (default 1; 0 turns some panels off), `keyboard` (default true), `device` (a name in `/sys/class/backlight`, default the first) |
| `power` | the power symbol; a popover with Lock, Suspend and Hibernate (when logind can), Log out, Reboot and Shut down, then the other systems to reboot into. Log out, Reboot, Shut down and each system ask for a second press within 4 s; in the dormer, picking one is enough. Lock asks logind to lock the session, so something must answer it (hypridle's `lock_cmd`). Systems are the boot loader's entries logind knows (systemd-boot), macOS on Apple Silicon, and those named in `boot`. macOS, GRUB and firmware entries need the boot helpers installed as root ([running.md](running.md#rebooting-into-another-system), [D-131](decisions.md)) | `show` (which of `"lock"`, `"logout"`, `"suspend"`, `"hibernate"`, `"reboot"`, `"poweroff"`, `"boot"`; all by default), `boot` (`[{ name = "Windows", efi = "Windows Boot Manager" }]`, with one of `efi` (a firmware boot entry's description), `grub` (a GRUB entry's title or id) or `loader` (a systemd-boot entry's id, to name it)) |
| `keyboard` | active layout, click for the next one; hidden with a single layout | `aliases` (`{ "English (US)" = "us" }`) |
| `appearance` | a sun or a moon: the desktop's light or dark mode. Click to switch, right click for a popover with both. It sets GNOME's `color-scheme` through dconf, which apps and the bar (with `theme = "system"`) follow; hidden without dconf ([D-92](decisions.md)) | none |
| `apps` | nothing in the bar: put it under `[[dormer]]`. Every installed app (its desktop entry) is an action in the dormer that launches it ([D-84](decisions.md)) | `terminal` (what runs apps that want a terminal, like `"foot"` or `"alacritty -e"`; without it they're left out) |
| `essential` | nothing in the bar: put it under `[[dormer]]`, as the starter config does. The bar's own actions in the dormer: Calibrate screen (runs `eave calibrate`), Restart bar (only when the bar runs as `eave.service`, which it restarts) and Check health (runs `eave doctor` and shows what it found in a dialog, problems first) ([D-98](decisions.md)) | none |
| `update` | nothing in the bar: put it under `[[dormer]]`, as the starter config does. Check for updates and Update eave in the dormer: each starts one of eave-build's systemd units (`eave-build check`, `eave-build update`), and a notification says what it found or moved when it's done. One that finds updates offers Update. Lists nothing without the units or systemd ([D-97](decisions.md), [D-98](decisions.md)) | none |
| `media` | what an MPRIS player is playing: the track's cover (the player's icon without one), then title · artist, dimmed while paused; a pause or play sign in place of the cover for 1.5 s after pausing or playing, from anywhere ([D-101](decisions.md)); right click to play or pause, scroll to skip tracks, with a nudge; cover, track, seek bar, controls and a player list in the popover; hidden when nothing plays or is paused ([D-56](decisions.md)) | `show` (`"title"` (default), `"hover"`: the text only while hovered, `"icon"`: the cover alone), `max_chars` (default 40) |

```toml
[[left]]
widget = "workspaces"
persistent = 5

[[left]]
widget = "window"

[[center]]
widget = "media"
show = "hover"

[[right]]
widget = "keyboard"
aliases = { "English (US)" = "us", "Italian" = "it" }
```

### Action ids

The built-in widgets' actions that are always there have ids, for `eave action <widget> <id>` and for `keys` in config.toml ([running.md](running.md#shortcuts)). They come with no keys of their own.

| Widget | Action ids |
|---|---|
| `volume` | `mute`, `up`, `down`, `mic-mute` |
| `media` | `play-pause`, `next`, `previous` |
| `brightness` | `up`, `down` |
| `workspaces` | `previous`, `next` |
| `keyboard` | `next-layout` |
| `appearance` | `light`, `dark` |
| `network` | `wifi-on`, `wifi-off`, `wifi-restart` |
| `bluetooth` | `bluetooth-on`, `bluetooth-off` |
| `power` | `lock`, `logout`, `suspend`, `hibernate`, `reboot`, `poweroff`, and `boot-<name>` for each system (`boot-macos`, `boot-windows`); each only where the machine can |
| `essential` | `calibrate`, `restart` (only under `eave.service`), `health` |
| `update` | `check`, `update` |

### Bays

The places the built-in widgets leave for parts ([D-109](decisions.md)), with what each is about:

| Widget | Bay | Subject |
|---|---|---|
| `media` | `player`, under the controls | the player's MPRIS bus name, `org.mpris.MediaPlayer2.spotify` |
| `volume` | `output`, `input`, under each page's slider | the device's PulseAudio name, `bluez_output.AC_12_34_56_78_9A.1` |

Notifications come from a plugin of their own, eave's notification server, in [eaveshell/eave-notifications](https://github.com/eaveshell/eave-notifications): `widget = "eaveshell/eave-notifications"` ([D-50](decisions.md)).

## Writing a plugin

```toml
# Cargo.toml
[dependencies]
eave-api = "0.1"
serde = { version = "1", features = ["derive"] }
```

`eave-api` and the service crates are on crates.io, so a plugin builds and tests on its own ([D-50](decisions.md)). When it's compiled into a bar, `eave-build` patches them to the eave checkout it builds against, so the host and every plugin share one copy.

```toml
# eave-plugin.toml, beside the crate's Cargo.toml
[plugin]
name = "weather"         # the one widget it registers
description = "Current weather, forecast in the popover"
api = "0.1"              # eave-api version you target (cargo caret rules)
```

A plugin is one crate with one widget, registered under the manifest's `name` ([D-46](decisions.md)). `eave-build` takes the crate's name from its `Cargo.toml` and calls its `register`.

**One plugin per repository:** put `eave-plugin.toml` and `Cargo.toml` at the root. Users write `widget = "you/<repo>"`, which asks for the plugin named like the repository, without a leading `eave-`. So a repository `eave-weather` should set `name = "weather"`. When the names differ, `eave-build` says to write `you/<repo>#<name>`.

**Several plugins in one repository:** put an `eave-collection.toml` at the root that lists their directories, each holding its own `eave-plugin.toml` and `Cargo.toml`. Users pick one with `#name`. eave's own built-ins are such a collection.

```toml
# eave-collection.toml, at the repository root
[collection]
description = "System monitors"
plugins = ["cpu", "mem"]   # directories, relative to this file
```

```toml
[[right]]
widget = "you/eave-monitors#cpu"
[[right]]
widget = "you/eave-monitors#mem"
```

A repository has one of the two files at its root, never both. Plugins in a collection can share code through a library crate of their own, and only the ones a config uses are compiled in. A name that isn't there fails the build with a list of the ones that are.

```rust
use eave_api::ui::{self, Item, Panel, Tone};
use eave_api::{Effect, NoConfig, Registry, Task, Widget};

pub fn register(registry: &mut Registry) {
    registry.widget::<Counter>("counter");
}

pub struct Counter(u32);

#[derive(Debug, Clone)]
pub enum Message {
    Reset,
}

impl Widget for Counter {
    type Config = NoConfig;      // or your own #[derive(Deserialize)] struct
    type Message = Message;

    fn new(_: NoConfig) -> (Self, Task<Message>) {
        (Self(0), Task::none())
    }

    fn update(&mut self, message: Message) -> Effect<Message> {
        match message {
            Message::Reset => self.0 = 0,
        }
        Effect::none()
    }

    fn item(&self) -> Item<Message> {
        Item::new(ui::label(self.0.to_string()))
    }

    fn popover(&self) -> Option<Panel<Message>> {
        Some(Panel::new(ui::column([
            ui::heading("Counter"),
            ui::button(ui::label("Reset"), Message::Reset),
        ])))
    }
}
```

### Services

Widgets that read the same source share it through a service crate, instead of each opening its own connection. For Hyprland, depend on `eave-hyprland` (patched by `eave-build` like `eave-api`):

```rust
fn subscription(&self) -> Subscription<Message> {
    // A snapshot first, then every event after it. Re-snapshots by itself if events were missed.
    Subscription::run(|| eave_hyprland::watch(snapshot)).map(Message::Hyprland)
}

async fn snapshot() -> std::io::Result<eave_hyprland::types::Window> {
    eave_hyprland::query("activewindow").await
}
```

`eave_hyprland::dispatch("hl.dsp.focus({ workspace = 3 })", "workspace 3")` runs a dispatcher. Give it in both the Lua syntax of Hyprland 0.56+ and the older one, and it uses whichever this Hyprland speaks. `request("…")` sends any other command.

For D-Bus, depend on `eave-dbus`: `system()` and `session()` are the shared bus connections. `eave_dbus::watch(service, snapshot)` yields a snapshot of the service's state, then a new one after each `PropertiesChanged` on the objects the snapshot depends on. Unchanged snapshots are skipped. `properties(...)` reads every property of an interface in one call.

For the network, depend on `eave-net`: `watch()` yields the primary connection (interface, Wi-Fi SSID and signal, address, gateway) after every change. It reads the kernel directly, so it works whichever network manager is installed.

For Bluetooth, depend on `eave-bluez`: `watch()` yields the adapter and its devices (paired, connected, battery, signal while looking) whenever BlueZ signals, and `set_powered`, `start_discovery`, `connect`, `disconnect`, `pair` and `forget` send commands. While `pair` runs, `requests()` yields what BlueZ asks (a code to confirm or type), and `answer` replies.

For media players, depend on `eave-mpris`: `watch()` yields every MPRIS player (status, track, cover URL, position, what it can do) whenever one comes, goes or changes, and `play_pause`, `next`, `previous` and `set_position` send commands. `Player::position_at(now)` is where a playing track is now, since players don't signal it as it moves. A player's `bus_name` is what `media`'s bay is about.

For audio, depend on `eave-pulse`: `watch()` yields the outputs (sinks, default, volume, mute) after every change, and `set_volume`, `set_mute` and `set_default` send commands. Run them in a `Task`. A device's `name` is the server's for it, which stays when it comes back (a Bluetooth one has its address in it), and is what `volume`'s bays are about.

To send a notification, call `Notify` on `org.freedesktop.Notifications` over the session bus (`zbus::Connection::session()`; `eave-dbus` re-exports zbus), like any app ([D-52](decisions.md)). Whatever daemon the user runs shows it: eave's own, `eave-notify` in [eaveshell/eave-notifications](https://github.com/eaveshell/eave-notifications) ([D-50](decisions.md)), or mako, dunst or swaync. Put anything the spec lacks in `x-eave-<name>` hints, which other daemons ignore. eave's takes `x-eave-markdown`, `x-eave-thread` and `x-eave-thread-name`, `x-eave-icon` (a file for the app's icon), `x-eave-choices` (`a(sssb)`: key, label, description, recommended) with `x-eave-multiple` and `x-eave-step` (`(uu)`), and lists them in `GetCapabilities`: ask before sending them ([D-85](decisions.md)).

### The rules

- **Views are pure and cheap.** `item()` runs on every redraw, and `popover()` runs after every message to your widget. Precompute in `update`.
- **`update` never blocks.** I/O goes in a `Task` (`Task::perform`) or a `Subscription`.
- **Events, not polling.** Use `eave_api::time::every` only when there's no event source (see [development.md](development.md#code-rules)). To show something for a moment, return `Task::future(eave_api::time::sleep(d))` mapped to a message, numbered so that only the latest one counts.
- **You describe, the host draws.** The vocabulary is `label`, `heading`, `icon`, `app_icon`, `image`, `symbol`, `sprite`, `cover`, `counter`, `row`, `column`, `grid`, `button`, `switcher`, `alternate`, `hover`, `expand`, `gulp`, `separator`, `empty`, `bay`, plus `.tone(Tone::…)` (`Normal`, `Dim`, `Accent`, `Warning`, `Critical`, `Good`). `icon(Icon::…)` is a status symbol (volume, Wi-Fi, wired, battery, mic, bell, brightness, appearance: a sun or a moon, Bluetooth: dotted when connected) that says how full it is or whether it's off, or a plain symbol (lock, info, back, forward, check, refresh, close, reveal, bolt, plug, speaker, headphones, display, keyboard light, media, play, pause, previous, next), and the host draws it in the text's tone. `switcher(options, current)` is a row of choices with one marked current, and the host slides the mark when it changes. `counter(value, range, unit)` is a number the host counts up or down through every value in between when it changes. It keeps room for the widest number in `range`, so it holds its width. `alternate(options, current)` shows one option at a time, as big as the biggest one, and fades between them, e.g. an icon that turns into a `counter` for a moment. `.on_hover(index)` on an alternate shows that option while the pointer is over the item, and `hover(normal, hovered)` is the short form for two: an icon that shows its percent, a time that shows the date. `app_icon(app_id)` is an application's icon, named by window class or app id. The host finds it in the user's icon theme and draws it in its own colors, dimmed by `Tone::Dim`. `image(include_bytes!("../assets/logo.svg"))` is an SVG or PNG file you ship, drawn the same way, and `symbol(..)` draws only its shape, in the text's tone like an `icon` ([D-53](decisions.md)). Use them for what no `Icon` covers, such as a product's logo. `sprite(&FRAMES, &ORDER)` is a little animation you ship as frames like those, all one size: the host keeps their shape, picks the height and scales pixels up whole, so ship pixel art at its own size. They play in `ORDER` (indices into `FRAMES`), a frame every quarter second: the host plays it a couple of times when it shows up or changes, then rests on its first frame, and `.looping()` keeps it going while it shows. Every frame redraws the bar, so loop only what wants attention, or in a popover ([D-64](decisions.md)). `cover(url, fallback)` is a picture a program hands over at runtime by URL, like a track's cover art: the host loads `file://` and `https://` PNGs and JPEGs off the UI thread, crops them square, rounds them, and shows `fallback` until the picture is there or if it can't be had. `.large()` makes it a popover's main picture ([D-57](decisions.md)). `expand(collapsed, expanded)` shows `expanded` once the pointer rests on it, fading like `hover`. Unlike `hover`, it grows to fit, with the island springing along, and only the pointer over it counts, not the whole item. `button(...).on_secondary(message)` sends a message on a right click on that button. `button(...).shortcut(id, key)` gives a button in a popover or dialog a key, like a row's: drawn as keycaps on the button after what it says, rebindable by `id` ([D-100](decisions.md)). `gulp(content)` keeps swelling its content gently, one gulp after another, like a bottle being poured, for something filling up. `counted(content, count)` puts `count` in a small red badge on its content's top right corner, like unread notifications on a bell: 0 shows none, past 99 it shows 99, and the host grows it in and out and counts through the numbers in between ([D-82](decisions.md)). Popovers have their own pieces ([D-40](decisions.md)), laid out at one width the host picks: `header(title)` (with `.on_back(message)` for a sub-page, `.leading(node)` for a small picture before the title, `.subtitle(text)` under it, `.trailing(node)` at its end), `section(title)` (a small caption, with `.trailing`), `list_item(title)` (with `.subtitle`, `.leading`, `.trailing`, `.selected(bool)`, `.on_press`, and `.shortcut("chat-1", shortcut('1'))`, a key that presses it while the popover is open, named so the user can give it another in config.toml: the host draws its keycaps at the row's end; one with no Ctrl, Alt or Super gives way to a focused text field, and rows inside `tabs` get none, [D-91](decisions.md)), `keys(shortcut('k').ctrl())` (a shortcut drawn as keycaps, `[Ctrl] [K]`, only to show it), `list(rows)` (scrolls past a few rows), `detail(name, value)`, `fill()` (pushes what follows to a row's end), `center(node)` (centers it across the panel; text in it keeps to one line and is cut if it's too long), `slider(value, range, on_change)` (with `.step`, `.on_release`), `toggle(on, message)`, `meter(fraction)` (with `.limit(at)`), `text_input(placeholder, value, on_input)` (with `.on_submit`, `.secret(bool)`, `.trailing`; it takes the focus, and keys stay in the popover while it has it, [D-41](decisions.md)), `text_area(placeholder, value, on_input)` (several lines: grows to six, Enter sends `.on_submit`, Shift+Enter breaks the line; with `.trailing`), `markdown(text)` (Markdown set in the theme's type: emphasis, code, lists, quotes, code blocks colored for the language their fence names, tables; links don't open), `literal(text)` (text as typed: a path, a command; one line, monospace), `divider(text)` (a separator with a caption, like a time), `timeline(steps)` of `step(mark, content)` (steps strung on a line, each marked `Mark::You`, `Note`, `Running`, `Done`, `Failed` or `Waiting`; `.leading(node)` puts a small picture there instead), `diff(title, lines)` (a change to a file, each line a `Change::Added`, `Removed`, `Context` or `Hunk`; the host counts and colors it, [D-65](decisions.md)), `scroll(node)` (scrolls once taller than the host allows; `.bottom()` keeps the end in view, for a conversation), `display(node)` (large text), `mark(node)` (ringed: today in a calendar), `badge(text)`, `tabs([(name, page), …])` (pages behind a strip of names; the host shows the first when the popover opens and switches when one is clicked, so you keep no state for it, [D-43](decisions.md)), `page(order, node)` (one page of a sequence you move through, like a list and a row's details, or a calendar's months: when `order` changes, the host slides the old page away and the new one in, from the right for a higher order and from the left for a lower one; while it stays the same, the content just changes, [D-107](decisions.md)), and `button(...).prominent()` for a page's main action. A `button` holding only an `icon`, `image` or `symbol` is drawn as a square icon button, or a filled disc when prominent. A `heading` in a popover keeps to one line, cut at the edge if it's too long. `Panel::new(..).on_open(message).on_close(message)` tells you when the popover opens and starts closing, and `.wide()` asks for a wider popover for pages that are mostly reading: subscribe to chatty services only while it's open, and forget anything typed when it closes. If you need something it can't express, propose a new primitive in eave rather than working around it ([D-13](decisions.md)).
- **Notifications go over the bus, never through `notices()`.** Send them with `Notify`, as above. `notices()` is only for a notification engine: the plugin that serves `org.freedesktop.Notifications` lists there what it received, for the host to draw ([D-52](decisions.md)). It lists the notifications to show on screen now, newest first, each a `Notice::new(key, summary)` with `.app(name)`, `.app_icon(id)` (an app id, or a `file://` URL), `.icon(Icon::…)` (a status symbol instead, for what's about the device: a battery, a network; without either, or when the app's icon can't be found, the host shows a bell, [D-87](decisions.md)), `.image(url)` (its own picture, loaded like a `cover`: beside the text, or under it, full width, if it's wider than 4:3), `.pixels(Pixels::new(w, h, rgba))` (the same, from bytes), `.time(when)`, `.thread(key)` and `.thread_name(name)` (the conversation: notices of one thread that come one after another stack on one card), `.progress(fraction)`, `.body(text)`, `.markdown()` (the body is Markdown), `.urgency(Urgency::…)`, `.action(label, message)` (up to three show), `.choices(Choices::one()` or `Choices::many().on_send(..)` with `.option(label, description, picked, on_pick)` and `.recommended())`, `.step(index, count, on_back)` (one of several questions, sliding to the next), `.on_press(message)`, `.on_dismiss(message)` (gives it a close button, under the pointer), `.expires(after, message)` and `.reply(Reply::new(placeholder, value, on_input).on_submit(..).on_cancel(..).sent(bool))`: a field to type an answer into, shown with the notice, which says Sent while `sent`. The bar takes the keyboard only once the field is clicked, and nothing expires meanwhile; Esc gives it back ([D-49](decisions.md), [D-62](decisions.md)). While there are any, the host turns the right island into a panel of them, like a popover, keyed by `key`: a new key comes in, a missing one leaves. It counts `expires` only while the panel is on screen and not pointed at, and only for the three cards it shows, then sends the message. Drop the notice from your list when it's dismissed or expires ([D-47](decisions.md), [D-85](decisions.md)).
- **Actions put you in the dormer.** `actions()` lists what the widget can do, for the dormer: the search dialog a keybind opens ([D-84](decisions.md)). Each is `ui::action(title, message)`, with `.subtitle(text)`, `.keywords([..])` (more words to be found by, not shown) and `.leading(node)` (an `icon`, `app_icon`, `image` or `symbol`). Picking one sends its message to you, like a click. `.id("mute")` names an action that's always there, so `eave action <widget> mute` runs it from a Hyprland bind and config.toml can give it a key; `.shortcut(shortcut('m').ctrl())` is that key in the dormer by default, drawn on its row. It needs Ctrl, Alt or Super, and when two widgets want the same key the one earlier in config.toml keeps it ([D-91](decisions.md)). List your ids and keys in your README. The host asks only while the dormer is open, and again after each of your messages, so titles can follow your state ("Unmute" while muted). Leave out what can't be done right now. Your popover, if you have one, gets a row of its own, so don't add an action that only opens it. A widget that is only there for its actions, like `apps`, goes under `[[dormer]]` in config.toml and returns `Item::hidden()`.

- **A dialog for what needs room.** `dialog()` returns a `Panel` like `popover()` does, shown in the middle of the screen, sized to fit it, wider than a wide popover, with taller scrolls: for a conversation or a long read ([D-90](decisions.md)). Nothing opens it but you: return `Effect::none().open_dialog()`, and `close_dialog()` to close it. Returning `None` while it's open closes it too. The host picks where it comes from: it comes out of what was just acted on (your popover, your item's island, a notification, or the dormer), and your island stays on the bar, and without one it drops out of the top of the screen. It closes on Esc and on a click elsewhere, like the dormer. `on_open`/`on_close` on its panel say when it opens and starts going back. Keep returning its content while it goes back, so it fades out with it.

- **Bays and parts.** A widget can show inside another's popover or dialog ([D-109](decisions.md)). As the owner, put `ui::bay(name, subject)` where others may add something: `name` says where, and your README lists it; `subject` says what it's about, as the service names it. A bay nothing fills takes no room. As a part, implement `part(&self, bay: &Bay) -> Option<Part<Message>>`: look at `bay.name()` and `bay.subject()`, and return `None` when you have nothing for them. `Part::new(node)` takes `.on_open` and `.on_close`, like a `Panel`. You don't choose where you go: the user does, putting your entry in a zone or in another's `parts`. In a zone, the host uses your `item()` and `popover()`; as a part, only `part()`. The rest works the same, and effects land where you show: as a part, `open_popover()` opens your owner's popover, and a nudge nudges its island. To work in both places, share what you show:

  ```rust
  fn popover(&self) -> Option<Panel<Message>> {
      Some(Panel::new(ui::column([ui::header("Spotify"), self.body()])).on_open(Message::Opened))
  }

  fn part(&self, bay: &Bay) -> Option<Part<Message>> {
      // In media's player bay, and only for Spotify: media already shows the track.
      let ours = bay.name() == "player" && bay.subject().starts_with("org.mpris.MediaPlayer2.spotify");
      ours.then(|| Part::new(self.body()).on_open(Message::Opened))
  }
  ```

  Or return `Some(self.popover()?.into())` to show the whole popover. On its own, a widget finds its subject itself, from the service it watches.

### Clicks and popovers

| Item has `on_press` | `popover()` returns `Some` | Primary click |
|---|---|---|
| no | no | nothing (no pointer cursor) |
| no | yes | toggles the popover |
| yes | either | sends your message (you can still return `Effect::none().open_popover()`) |

A right click sends `on_secondary`, if set. `Item::on_scroll(up, down)` sends one message per scroll step. The host turns touchpad scrolling into steps. Buttons inside the item take their own clicks first. Return `Item::hidden()` to take no space at all (a battery widget on a desktop).

### Nudges

Return `Effect::none().nudge(Nudge::…)` from `update` when something your item shows just went one way, and the host gives your island a nudge that way. You say what happened, and the host picks the motion ([D-32](decisions.md)):

| Nudge | Use it when | The island |
|---|---|---|
| `Up`, `Down` | a level rose or fell (volume, brightness) | its top edge bumps up above your item, or its bottom edge dips below it, then springs flat |
| `Left`, `Right` | you moved to the previous or next thing (workspace, track) | the whole island is pushed that way and springs back, your item a little further |

If the change you're following plays out on screen over a known time, like the compositor sliding a workspace in, return `Effect::none().nudge_lasting(nudge, duration)` instead: the host plays the same motion, just as far, stretched to settle over about that long. `workspaces` does this with Hyprland's `workspacesIn` animation.

Nudge on the change itself, wherever it came from, and once per change. `volume` nudges on the server's readings, so a scroll step and a media key look the same. A nudge is dropped while the island is held or has a popover open. A nudge also brings a collapsed item out for a moment, so the change is seen ([D-58](decisions.md)).
