//! `eave doctor` (D-96): checks what the bar depends on and says what's wrong, and what to do
//! about it. config.toml, the widgets it names, their keys, the screen, hypr-eave, Hyprland's
//! layer animation, the systemd units, and ebb for the battery widget.
//! Runs without a bar, so it helps most when the bar won't start. The `essential` plugin's Check
//! health runs it with `--report` and shows what it wrote.

pub mod report;

use std::path::{Path, PathBuf};
use std::time::Duration;

use eave_api::Registry;
use eave_api::erased::ErasedWidget;
use eave_api::ui::Action;
use eave_hyprland::types::Monitor;

use crate::config::{self, Config, Entry, Notch, Screens, Theme};
use crate::{notch, shortcut};
use report::Check;

/// Runs the checks, prints them, and exits: 1 if any found a problem.
pub fn run(registry: &Registry) -> ! {
    let words: Vec<String> = std::env::args().skip(2).collect();
    let report = match words.as_slice() {
        [] => None,
        [flag, path] if flag == "--report" => Some(PathBuf::from(path)),
        _ => {
            eprintln!("eave doctor: takes only --report <path> (try --help)");
            std::process::exit(2);
        }
    };
    let checks = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(checks(registry)),
        Err(error) => {
            eprintln!("eave doctor: {error}");
            std::process::exit(1);
        }
    };
    print!("{}", report::text(&checks));
    if let Some(path) = report
        && let Err(error) = report::save(&checks, &path)
    {
        eprintln!("eave doctor: can't write {}: {error}", path.display());
        std::process::exit(1);
    }
    let failed = checks
        .iter()
        .any(|check| check.level == report::Level::Problem);
    std::process::exit(i32::from(failed));
}

async fn checks(registry: &Registry) -> Vec<Check> {
    let mut checks = Vec::new();
    let path = config::dir().map(|dir| dir.join(config::FILE));
    let config = config_checks(path.as_deref(), &mut checks);
    let mut battery = false;
    if let Some(config) = &config {
        let widgets = widget_checks(config, registry, &mut checks);
        checks.extend(key_checks(&widgets));
        battery = widgets
            .iter()
            .any(|(entry, _)| config::widget_name(&entry.widget) == "battery");
    }
    let screens = config.as_ref().map(Config::screens).unwrap_or_default();
    let monitor = notch::focused().await;
    let hardware = tokio::task::spawn_blocking(notch::hardware)
        .await
        .ok()
        .flatten();
    checks.extend(screen_checks(monitor.as_ref(), hardware, &screens));
    // Without Hyprland there's nothing to ask, and the screen's check already says so.
    if monitor.is_some() {
        let glass = config
            .as_ref()
            .is_some_and(|config| config.theme == Theme::Glass);
        checks.push(plugin_check(hypr_eave().await, glass));
        if let Ok(lasting) = eave_hyprland::animation("layersIn").await {
            checks.push(layers_animation_check(lasting));
        }
    }
    checks.extend(systemd_checks().await);
    if battery && has_battery(Path::new(POWER_SUPPLY)) {
        let collector = eave_dbus::systemd::file_state(EBB_COLLECTOR).await.ok();
        checks.push(ebb_check(collector.as_deref()));
    }
    checks
}

const POWER_SUPPLY: &str = "/sys/class/power_supply";
/// The unit that runs ebb's collector, which the battery widget reads its forecasts from (D-134).
const EBB_COLLECTOR: &str = "ebb-collector.service";

/// Whether the machine has a battery of its own: not a mouse's (`scope = Device`).
fn has_battery(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let text = |name: &str| std::fs::read_to_string(entry.path().join(name)).ok();
        text("type").is_some_and(|kind| kind.trim() == "Battery")
            && text("scope").is_none_or(|scope| scope.trim() != "Device")
    })
}

/// Whether ebb records the machine, so the battery widget can say a time left learned from use
/// rather than the kernel's guess. `state` is its collector unit's, `None` when it isn't there.
fn ebb_check(state: Option<&str>) -> Check {
    let install = "Install it with cargo install ebb-cli --locked, then add its collector unit and \
                   enable it: https://github.com/francescomengarelli/ebb#install";
    match state {
        Some("enabled") => Check::good(
            "ebb learns the battery",
            "The battery widget's time left comes from it, with a range, and its popover says \
             what draws the power.",
        ),
        Some(state) => Check::warning(
            format!("ebb's collector is {state}, so ebb stops learning the battery"),
            "systemctl --user enable --now ebb-collector",
        ),
        None => Check::warning(
            "ebb isn't installed, so the battery's time left is the kernel's guess",
            format!(
                "ebb learns what this machine costs to run and how you use it, for a steadier \
                 time left with a range, and what draws the power. {install}"
            ),
        ),
    }
}

/// Whether config.toml is there and reads, and what it warns about. The config, when it reads.
fn config_checks(path: Option<&Path>, checks: &mut Vec<Check>) -> Option<Config> {
    let Some(path) = path else {
        checks.push(Check::problem(
            "Can't find the config directory",
            "Neither XDG_CONFIG_HOME nor HOME is set.",
        ));
        return None;
    };
    if !path.exists() {
        checks.push(Check::warning(
            "There's no config.toml, so the bar is empty",
            format!(
                "eave-build init writes a starter one at {}.",
                path.display()
            ),
        ));
        return None;
    }
    match config::load_from(path) {
        Ok(config) => {
            checks.push(Check::good("config.toml reads", path.display().to_string()));
            for warning in &config.warnings {
                checks.push(Check::warning(
                    format!("config.toml: {warning}"),
                    "The bar skips it.",
                ));
            }
            Some(config)
        }
        Err(error) => {
            checks.push(Check::problem(
                "config.toml doesn't read",
                format!("{error}\nA running bar keeps what it last read."),
            ));
            None
        }
    }
}

/// Whether every widget config.toml names is in this build and takes its options. The widgets
/// that are, made: their actions say their keys.
fn widget_checks(
    config: &Config,
    registry: &Registry,
    checks: &mut Vec<Check>,
) -> Vec<(Entry, Box<dyn ErasedWidget>)> {
    let mut widgets = Vec::new();
    let mut fine = true;
    for entry in &config.entries {
        let name = config::widget_name(&entry.widget);
        let Some(construct) = registry.get(name) else {
            fine = false;
            checks.push(Check::problem(
                format!("`{}` isn't in this build", entry.widget),
                "Run eave-build (eave-rebuild.path does on save). If it failed, \
                 journalctl --user -u eave-rebuild says why.",
            ));
            continue;
        };
        // Made only for its actions: its task never runs.
        match construct(entry.options.clone()) {
            Ok((widget, _)) => widgets.push((entry.clone(), widget)),
            Err(error) => {
                fine = false;
                checks.push(Check::problem(
                    format!("`{name}` doesn't take its options"),
                    format!("{error}. The bar skips it."),
                ));
            }
        }
    }
    if config.entries.is_empty() {
        checks.push(Check::warning(
            "config.toml names no widgets, so the bar is empty",
            "",
        ));
    } else if fine {
        checks.push(Check::good(
            format!("All {} widgets are built and set up", config.entries.len()),
            "",
        ));
    }
    widgets
}

/// The dormer's keys, settled the way the bar settles them (D-91): which actions lose theirs.
fn key_checks(widgets: &[(Entry, Box<dyn ErasedWidget>)]) -> Vec<Check> {
    let actions: Vec<_> = widgets
        .iter()
        .flat_map(|(entry, widget)| {
            widget
                .actions()
                .into_iter()
                .map(Action::into_parts)
                .map(move |action| (entry, action.id, action.shortcut))
        })
        .collect();
    let candidates: Vec<_> = actions
        .iter()
        .map(|(entry, id, default)| shortcut::Candidate {
            id: id.as_deref(),
            default: *default,
            keys: &entry.keys,
            widget: config::widget_name(&entry.widget),
        })
        .collect();
    let (_, warnings) = shortcut::resolve_actions(&candidates);
    if warnings.is_empty() {
        return vec![Check::good("No two actions want the same key", "")];
    }
    warnings
        .into_iter()
        .map(|warning| {
            Check::warning(
                warning,
                "Give one of them another key under `keys` in config.toml.",
            )
        })
        .collect()
}

/// Whether Hyprland answers, and whether the screen the bar opens on has what it needs measured:
/// a notch the kernel shows wants `notch` (D-71, D-94).
fn screen_checks(
    monitor: Option<&Monitor>,
    hardware: Option<(Option<f32>, f32)>,
    screens: &Screens,
) -> Vec<Check> {
    let Some(monitor) = monitor else {
        return vec![Check::problem(
            "Hyprland doesn't answer",
            "Is it running? The bar finds it through HYPRLAND_INSTANCE_SIGNATURE.",
        )];
    };
    let name = &monitor.name;
    let mut checks = vec![Check::good(
        "Hyprland answers",
        format!("The bar opens on {name}, the focused monitor."),
    )];
    let setting = screens.screen_for(name, &monitor.description);
    let table = screens
        .matching(name, &monitor.description)
        .map_or("the top level".to_owned(), |(table, _)| {
            format!("[monitor.\"{table}\"]")
        });
    // As the bar finds it: only the built-in panel, upright, has the notch.
    let notched = hardware.is_some() && name.starts_with("eDP") && monitor.transform == 0;
    checks.push(match (notched, setting.notch) {
        (true, Notch::Fixed(_)) => Check::good(
            format!("{name}'s notch is measured"),
            format!("In {table} of config.toml."),
        ),
        (true, Notch::Off) => Check::good(format!("{name}'s notch is turned off"), ""),
        (true, Notch::Auto) => Check::warning(
            format!("{name}'s notch isn't measured"),
            match hardware {
                Some((None, _)) => {
                    "The bar guesses how wide it is. Calibrate screen (eave \
                                    calibrate) measures it."
                }
                _ => {
                    "The bar uses this model's size. Calibrate screen (eave calibrate) fits \
                      it to the pixel."
                }
            },
        ),
        (false, _) if setting.screen.is_some() => Check::good(
            format!("{name}'s corners are measured"),
            format!("In {table} of config.toml."),
        ),
        (false, _) => Check::good(
            format!("{name} has nothing measured"),
            "Fine for square corners. If they're rounded, Calibrate screen fits the bar to them.",
        ),
    });
    checks
}

/// Where eave-build installs hypr-eave (D-112), and where running.md has Hyprland load it from.
fn plugin_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("eave/hypr-eave.so"))
}

/// What there is of hypr-eave, installed and in Hyprland.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HyprEave {
    /// Not installed, not loaded.
    None,
    /// eave-build installed it, and Hyprland doesn't load it.
    Installed,
    /// Hyprland has it loaded.
    Loaded,
    /// Hyprland has it loaded from where eave-build installs it, and eave-build has installed
    /// a new one there since: renamed over it, so Hyprland maps the old file, now deleted.
    Older,
}

async fn hypr_eave() -> HyprEave {
    let path = plugin_path();
    let loaded = eave_hyprland::query::<Vec<eave_hyprland::types::Plugin>>("plugin list")
        .await
        .is_ok_and(|plugins| plugins.iter().any(|plugin| plugin.name == "hypr-eave"));
    if !loaded {
        let installed = path.as_deref().is_some_and(Path::exists);
        return if installed {
            HyprEave::Installed
        } else {
            HyprEave::None
        };
    }
    let maps =
        eave_hyprland::pid().and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/maps")));
    match (path, maps) {
        (Some(path), Ok(maps)) if maps_only_deleted(&maps, &path) => HyprEave::Older,
        _ => HyprEave::Loaded,
    }
}

/// Whether `maps` (a `/proc/<pid>/maps`) has `path` mapped only as a file since deleted.
fn maps_only_deleted(maps: &str, path: &Path) -> bool {
    let path = path.to_string_lossy();
    let deleted = format!("{path} (deleted)");
    let mut found = false;
    for line in maps.lines() {
        // The path is the last field, after the inode, and may hold spaces.
        let Some((_, file)) = line.split_once(&format!(" {path}")) else {
            continue;
        };
        if format!("{path}{file}") == deleted {
            found = true;
        } else if file.is_empty() {
            return false;
        }
    }
    found
}

fn plugin_check(hypr_eave: HyprEave, glass: bool) -> Check {
    let load = "Add hl.plugin.load(os.getenv(\"HOME\") .. \"/.local/share/eave/hypr-eave.so\") \
                to hyprland.lua, then restart Hyprland.";
    match hypr_eave {
        HyprEave::Loaded => Check::good(
            "hypr-eave is loaded",
            if glass {
                "The islands are glass, and the pointer goes under the notch."
            } else {
                "The pointer goes under the notch and the grown edge."
            },
        ),
        HyprEave::Older => Check::warning(
            "Hyprland runs an older hypr-eave",
            "eave-build has installed a new one since Hyprland loaded it. Restart Hyprland to \
             load it.",
        ),
        HyprEave::Installed if glass => Check::warning(
            "hypr-eave is built, but Hyprland doesn't load it, so the glass theme looks dark",
            load,
        ),
        HyprEave::Installed => Check::warning(
            "hypr-eave is built, but Hyprland doesn't load it",
            format!("It puts the pointer under the notch. {load}"),
        ),
        HyprEave::None if glass => Check::warning(
            "The glass theme needs hypr-eave, so it looks dark",
            "eave-build builds it when Hyprland's headers are installed (pkg-config hyprland).",
        ),
        HyprEave::None => Check::good(
            "hypr-eave isn't installed",
            "It's optional: it puts the pointer under the notch. eave-build builds it when \
             Hyprland's headers are installed (pkg-config hyprland).",
        ),
    }
}

/// Whether Hyprland animates a layer surface's size, which squeezes the bar each time a popover
/// opens or closes (D-123). `lasting` is how long `layersIn` takes, `None` when it's off.
fn layers_animation_check(lasting: Option<Duration>) -> Check {
    match lasting {
        None => Check::good(
            "Hyprland doesn't animate the bar's size",
            "layersIn is off.",
        ),
        Some(lasting) => Check::warning(
            "Hyprland animates the bar's size, so popovers squeeze it",
            format!(
                "Hyprland's layersIn animation also moves a layer's box when it resizes: each \
                 time a popover opens or closes, the bar is squeezed or stretched into a box \
                 sliding to its new height over {} ms. Turn it off: \
                 hl.animation({{ leaf = \"layersIn\", enabled = false }}) in hyprland.lua, or \
                 animation = layersIn, 0 in hyprland.conf. Layers still fade in (fadeLayersIn); \
                 only their slide or pop-in goes.",
                lasting.as_millis()
            ),
        ),
    }
}

/// The units that run the bar and rebuild and update it (running.md), and the environment the
/// bar needs from systemd. Asked over D-Bus.
async fn systemd_checks() -> Vec<Check> {
    use eave_dbus::systemd;

    let Ok(environment) = systemd::environment().await else {
        return vec![Check::good(
            "No systemd user manager",
            "The bar runs without one, started by Hyprland. Restart bar and the updates in \
             the dormer need it.",
        )];
    };
    let mut checks = Vec::new();
    let bar = systemd::file_state("eave.service").await.ok();
    checks.push(unit_check(
        "eave.service",
        bar.as_deref(),
        "systemctl --user enable --now eave.service starts the bar with your session.",
    ));
    if bar.as_deref() == Some("enabled") {
        for missing in missing_environment(&environment) {
            checks.push(Check::problem(
                format!("systemd doesn't know {missing}"),
                "So eave.service can't reach the compositor. running.md says what to add to \
                 Hyprland's config.",
            ));
        }
    }
    let rebuild = systemd::file_state("eave-rebuild.path").await.ok();
    checks.push(unit_check(
        "eave-rebuild.path",
        rebuild.as_deref(),
        "Without it, a widget new to config.toml waits for you to run eave-build. \
         systemctl --user enable --now eave-rebuild.path",
    ));
    let mut updates = Vec::new();
    for unit in UPDATE_UNITS {
        updates.push(systemd::file_state(unit).await.is_ok());
    }
    checks.push(if updates.iter().all(|&found| found) {
        Check::good("The update units are installed", "")
    } else {
        Check::warning(
            "The update units aren't installed",
            "eave-build init installs them, for Check for updates and Update eave.",
        )
    });
    checks
}

/// What eave-build runs `check` and `update` in, for the `update` plugin (D-97).
const UPDATE_UNITS: [&str; 2] = ["eave-update.service", "eave-update-check.service"];

fn unit_check(unit: &str, state: Option<&str>, fix: &str) -> Check {
    match state {
        Some("enabled") => Check::good(format!("{unit} is enabled"), ""),
        Some(state) => Check::warning(format!("{unit} is {state}"), fix),
        None => Check::warning(
            format!("{unit} isn't installed"),
            "eave-build init installs it.",
        ),
    }
}

/// The variables eave.service needs that systemd's environment lacks.
fn missing_environment(environment: &[String]) -> Vec<&'static str> {
    ["WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"]
        .into_iter()
        .filter(|name| {
            !environment.iter().any(|variable| {
                variable
                    .split_once('=')
                    .is_some_and(|(key, value)| key == *name && !value.is_empty())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use report::Level;

    fn parse(source: &str) -> Config {
        config::parse(source).unwrap()
    }

    fn levels(checks: &[Check]) -> Vec<Level> {
        checks.iter().map(|check| check.level).collect()
    }

    #[test]
    fn config_toml_that_doesnt_read_is_a_problem() {
        let dir = std::env::temp_dir().join(format!("eave-doctor-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let mut checks = Vec::new();
        assert!(config_checks(Some(&path), &mut checks).is_none());
        assert_eq!(levels(&checks), [Level::Warning]);

        std::fs::write(&path, "[[left]\n").unwrap();
        let mut checks = Vec::new();
        assert!(config_checks(Some(&path), &mut checks).is_none());
        assert_eq!(levels(&checks), [Level::Problem]);

        std::fs::write(&path, "wat = 1\n[[left]]\nwidget = \"clock\"\n").unwrap();
        let mut checks = Vec::new();
        assert!(config_checks(Some(&path), &mut checks).is_some());
        assert_eq!(levels(&checks), [Level::Good, Level::Warning]);
        assert!(checks[1].title.contains("wat"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    mod widgets {
        use eave_api::ui::{self, Item, shortcut};
        use eave_api::{Effect, NoConfig, Task, Widget};

        /// A widget with one action, `go`, that wants Ctrl+G.
        pub struct Go;

        impl Widget for Go {
            type Config = NoConfig;
            type Message = ();

            fn new(_: NoConfig) -> (Self, Task<()>) {
                (Self, Task::none())
            }

            fn update(&mut self, _: ()) -> Effect<()> {
                Effect::none()
            }

            fn item(&self) -> Item<()> {
                Item::hidden()
            }

            fn actions(&self) -> Vec<ui::Action<()>> {
                vec![ui::action("Go", ()).id("go").shortcut(shortcut('g').ctrl())]
            }
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.widget::<widgets::Go>("go");
        registry.widget::<widgets::Go>("also");
        registry
    }

    #[test]
    fn widgets_not_built_or_with_bad_options_are_problems() {
        let config = parse(
            "[[left]]\nwidget = \"go\"\n\
             [[left]]\nwidget = \"someone/eave-weather\"\n\
             [[left]]\nwidget = \"also\"\nformat = \"%H\"\n",
        );
        let mut checks = Vec::new();
        let widgets = widget_checks(&config, &registry(), &mut checks);
        assert_eq!(widgets.len(), 1);
        assert_eq!(levels(&checks), [Level::Problem, Level::Problem]);
        assert!(checks[0].title.contains("someone/eave-weather"));
        assert!(checks[1].title.contains("`also`"));

        let fine = parse("[[dormer]]\nwidget = \"go\"\n");
        let mut checks = Vec::new();
        widget_checks(&fine, &registry(), &mut checks);
        assert_eq!(levels(&checks), [Level::Good]);
    }

    #[test]
    fn two_actions_wanting_one_key_are_reported() {
        let config = parse("[[dormer]]\nwidget = \"go\"\n[[dormer]]\nwidget = \"also\"\n");
        let widgets = widget_checks(&config, &registry(), &mut Vec::new());
        let checks = key_checks(&widgets);
        assert_eq!(levels(&checks), [Level::Warning]);
        assert!(checks[0].title.contains("also"), "{}", checks[0].title);

        let rebound = parse(
            "[[dormer]]\nwidget = \"go\"\n\
             [[dormer]]\nwidget = \"also\"\nkeys = { go = \"ctrl+h\" }\n",
        );
        let widgets = widget_checks(&rebound, &registry(), &mut Vec::new());
        assert_eq!(levels(&key_checks(&widgets)), [Level::Good]);
    }

    fn monitor(name: &str) -> Monitor {
        Monitor {
            name: name.into(),
            description: "Apple Computer Inc 1234".into(),
            scale: 2.0,
            ..Monitor::default()
        }
    }

    #[test]
    fn a_notch_wants_measuring() {
        let screens = Screens::default();
        let built_in = monitor("eDP-1");
        let checks = screen_checks(Some(&built_in), Some((None, 74.0)), &screens);
        assert_eq!(levels(&checks), [Level::Good, Level::Warning]);
        assert!(checks[1].detail.contains("guesses"));

        let measured =
            parse("[monitor.\"Apple Computer Inc\"]\nnotch = { width = 300, height = 74 }\n")
                .screens();
        let checks = screen_checks(Some(&built_in), Some((None, 74.0)), &measured);
        assert_eq!(levels(&checks), [Level::Good, Level::Good]);
        assert!(checks[1].detail.contains("Apple Computer Inc"));

        // No notch: an external monitor, or a rotated panel.
        let checks = screen_checks(Some(&monitor("DP-1")), Some((None, 74.0)), &screens);
        assert_eq!(levels(&checks), [Level::Good, Level::Good]);
        assert!(checks[1].title.contains("nothing measured"));
        let corners =
            parse("[monitor.\"DP-1\"]\nscreen = { corners = [20, 20, 0, 0] }\n").screens();
        let checks = screen_checks(Some(&monitor("DP-1")), None, &corners);
        assert!(
            checks[1].title.contains("corners are measured"),
            "{}",
            checks[1].title
        );
        let rotated = Monitor {
            transform: 1,
            ..monitor("eDP-1")
        };
        let checks = screen_checks(Some(&rotated), Some((None, 74.0)), &screens);
        assert_eq!(levels(&checks), [Level::Good, Level::Good]);

        assert_eq!(
            levels(&screen_checks(None, None, &screens)),
            [Level::Problem]
        );
    }

    #[test]
    fn hypr_eave_is_checked_loaded_and_current() {
        let level = |hypr_eave, glass| plugin_check(hypr_eave, glass).level;
        assert_eq!(level(HyprEave::Loaded, false), Level::Good);
        assert_eq!(level(HyprEave::Older, false), Level::Warning);
        assert_eq!(level(HyprEave::Installed, false), Level::Warning);
        assert!(
            plugin_check(HyprEave::Installed, false)
                .detail
                .contains("hl.plugin.load")
        );
        assert_eq!(level(HyprEave::None, false), Level::Good);
        assert_eq!(level(HyprEave::None, true), Level::Warning);
    }

    #[test]
    fn an_animated_layer_size_is_a_warning() {
        assert_eq!(layers_animation_check(None).level, Level::Good);
        let animated = layers_animation_check(Some(Duration::from_millis(400)));
        assert_eq!(animated.level, Level::Warning);
        assert!(animated.detail.contains("400 ms"));
        assert!(
            animated
                .detail
                .contains("leaf = \"layersIn\", enabled = false")
        );
    }

    #[test]
    fn a_replaced_plugin_is_mapped_deleted() {
        let path = Path::new("/home/me/.local/share/eave/hypr-eave.so");
        let line =
            |file: &str| format!("fffec2770000-fffec27b8000 r-xp 00000000 103:05 5112490   {file}");
        let old = line("/home/me/.local/share/eave/hypr-eave.so (deleted)");
        let current = line("/home/me/.local/share/eave/hypr-eave.so");
        let elsewhere = line("/run/user/1000/hypr-eave-1.so");
        assert!(maps_only_deleted(&format!("{old}\n{elsewhere}"), path));
        assert!(!maps_only_deleted(&format!("{old}\n{current}"), path));
        assert!(!maps_only_deleted(&current, path));
        assert!(!maps_only_deleted(&elsewhere, path));
    }

    #[test]
    fn systemd_needs_the_compositors_variables() {
        let environment = [
            "PATH=/usr/bin".to_owned(),
            "WAYLAND_DISPLAY=wayland-1".to_owned(),
            "HYPRLAND_INSTANCE_SIGNATURE=".to_owned(),
        ];
        assert_eq!(
            missing_environment(&environment),
            ["HYPRLAND_INSTANCE_SIGNATURE"]
        );
        assert_eq!(
            missing_environment(&[]),
            ["WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"]
        );
    }

    #[test]
    fn ebb_says_how_to_get_it() {
        assert_eq!(ebb_check(Some("enabled")).level, Level::Good);
        assert_eq!(ebb_check(Some("disabled")).level, Level::Warning);
        let missing = ebb_check(None);
        assert_eq!(missing.level, Level::Warning);
        assert!(missing.detail.contains("cargo install ebb-cli"));
    }

    #[test]
    fn a_mouse_is_not_a_battery() {
        let root = std::env::temp_dir().join(format!("eave-doctor-power-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mouse = root.join("hidpp_battery_0");
        std::fs::create_dir_all(&mouse).unwrap();
        std::fs::write(mouse.join("type"), "Battery\n").unwrap();
        std::fs::write(mouse.join("scope"), "Device\n").unwrap();
        assert!(!has_battery(&root));
        let own = root.join("BAT0");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("type"), "Battery\n").unwrap();
        assert!(has_battery(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
