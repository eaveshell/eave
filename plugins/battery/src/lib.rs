//! Battery level, as a battery icon filled to the charge: green while charging, red when nearly
//! empty. Hovering it shows the percentage instead, gulping while it charges.
//! Read from the kernel's `/sys/class/power_supply`, with every system battery combined into one.
//! Hidden when there's no battery (a desktop).
//!
//! When ebb records the machine ([`learned`]), the time left is ebb's: learned from how the
//! machine is used, with a range, and the popover also says what you seem to be doing and, while
//! it's open, which programs draw the power ([`apps`]). Without ebb it's the kernel's estimate,
//! and the popover says ebb would do better.
//!
//! It sends a notification when the charger goes in, when the battery runs low and when it's done
//! charging ([`notify`]).

use eave_api::ui::{self, Icon, Item, Node, Panel, Tone};
use eave_api::{Effect, Registry, Subscription, Task, Widget};
use serde::Deserialize;

mod apps;
mod learned;
mod notify;
mod sysfs;

use apps::Apps;
use learned::{Forecast, Method};

pub fn register(registry: &mut Registry) {
    registry.widget::<Battery>("battery");
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Percentage at or below which a discharging battery shows as a warning.
    warning: u8,
    /// Percentage at or below which it shows as critical, and a notification says so.
    critical: u8,
    /// Whether to send notifications: battery low, and charged.
    notify: bool,
    /// Whether to send one when the charger goes in, too (if `notify`).
    charging: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            warning: 20,
            critical: 10,
            notify: true,
            charging: true,
        }
    }
}

/// Every battery combined, read as one snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    present: bool,
    /// How many batteries were combined.
    batteries: usize,
    percentage: f64,
    state: State,
    /// Seconds, 0 when unknown.
    time_to_empty: i64,
    time_to_full: i64,
    /// Watts.
    energy_rate: f64,
    /// Watt-hours stored now, and when full.
    energy: Option<f64>,
    energy_full: Option<f64>,
    /// Full capacity over what it was designed for.
    health: Option<f64>,
    cycles: Option<u32>,
    /// °C.
    temperature: Option<f64>,
    /// The kernel's word for the battery's health, like `Good`.
    condition: Option<String>,
    /// The charge it stops at, in percent, when that's under 100.
    charge_limit: Option<u8>,
    /// Whether a charger is plugged in, when the machine says.
    ac_online: Option<bool>,
    /// ebb's forecast, when it records this machine.
    forecast: Option<Box<Forecast>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Full,
    /// Plugged in, but not charging (charge limit reached, or waiting).
    NotCharging,
}

impl State {
    /// The kernel's `status` attribute.
    fn from_sysfs(status: &str) -> Self {
        match status {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Full" => Self::Full,
            "Not charging" => Self::NotCharging,
            _ => Self::Unknown,
        }
    }
}

pub struct Battery {
    config: Config,
    reading: Reading,
    /// Warned that the battery is low, since the charger was last plugged in.
    warned: bool,
    /// The low battery notification's id, once the server gave it.
    warning: Option<u32>,
    /// The id of the notification that the charger went in, once the server gave it.
    plugged: Option<u32>,
    /// The popover is open: what draws the power is being measured.
    open: bool,
    /// What draws the power, since the popover opened.
    apps: Option<Apps>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Read(Reading),
    /// The low battery notification went up, with this id.
    Warned(u32),
    /// The notification that the charger went in went up, with this id.
    Plugged(u32),
    Opened,
    Closed,
    Apps(Apps),
}

/// A notification to send or take away.
#[derive(Debug, Clone, PartialEq)]
enum Alert {
    /// Running low, at this percent.
    Low(u8),
    /// The charger went in, at this percent: say so, and the low battery notification can go.
    Plugged(u8),
    /// The charger came out: the notification that it went in can go.
    Unplugged,
    /// Done charging, at this percent.
    Charged(u8),
}

impl Widget for Battery {
    type Config = Config;
    type Message = Message;

    fn new(config: Config) -> (Self, Task<Message>) {
        let battery = Self {
            config,
            reading: Reading::default(),
            warned: false,
            warning: None,
            plugged: None,
            open: false,
            apps: None,
        };
        (battery, Task::none())
    }

    fn update(&mut self, message: Message) -> Effect<Message> {
        match message {
            Message::Read(reading) => {
                let before = std::mem::replace(&mut self.reading, reading);
                match self.alert(&before) {
                    Some(alert) => Effect::task(self.send(alert)),
                    None => Effect::none(),
                }
            }
            // Plugged in before the server answered: it can go right away.
            Message::Warned(id) if !self.warned => Effect::task(close(id)),
            Message::Warned(id) => {
                self.warning = Some(id);
                Effect::none()
            }
            Message::Plugged(id) => {
                self.plugged = Some(id);
                Effect::none()
            }
            Message::Opened => {
                self.open = true;
                Effect::none()
            }
            Message::Closed => {
                self.open = false;
                self.apps = None;
                Effect::none()
            }
            Message::Apps(apps) => {
                if self.open {
                    self.apps = Some(apps);
                }
                Effect::none()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let reading = Subscription::run(sysfs::watch).map(Message::Read);
        // Only with ebb, and only while someone looks.
        if self.open && self.reading.forecast.is_some() {
            Subscription::batch([reading, Subscription::run(apps::watch).map(Message::Apps)])
        } else {
            reading
        }
    }

    fn item(&self) -> Item<Message> {
        if !self.reading.present {
            return Item::hidden();
        }
        let percent = self.percent();
        let icon = ui::icon(Icon::Battery {
            charge: f32::from(percent) / 100.0,
            charging: self.charging(),
        });
        let label = ui::label(format!("{percent}%"));
        let label = if self.charging() {
            ui::gulp(label)
        } else {
            label
        };
        Item::new(ui::hover(icon, label).tone(self.tone()))
    }

    fn popover(&self) -> Option<Panel<Message>> {
        if !self.reading.present {
            return None;
        }
        let reading = &self.reading;
        let percent = self.percent();
        let hero = ui::row([
            ui::display(ui::counter(percent.into(), 0..=100, "%")),
            ui::fill(),
            ui::column(self.outlook()),
        ])
        .tone(self.tone());
        let mut meter = ui::meter(f32::from(percent) / 100.0).tone(self.tone());
        if let Some(limit) = reading.charge_limit {
            meter = meter.limit(f32::from(limit) / 100.0);
        }
        let mut parts = vec![hero, meter];
        if let Some(activity) = reading.forecast.as_ref().and_then(|f| f.activity.as_ref()) {
            parts.push(now(activity));
        }

        let mut tabs = Vec::new();
        if reading.forecast.is_some() {
            tabs.push(("Apps", self.apps()));
        }
        tabs.push(("Power", ui::column(self.power())));
        let health = self.health();
        if !health.is_empty() {
            tabs.push(("Health", ui::column(health)));
        }
        parts.push(match tabs.len() {
            1 => tabs.pop().map(|(_, page)| page).unwrap_or_else(ui::empty),
            _ => ui::tabs(tabs),
        });
        // ebb follows one battery; with more there's nothing to suggest.
        if reading.forecast.is_none() && reading.batteries == 1 {
            parts.push(ui::separator());
            parts.push(
                ui::label("A steadier time left needs ebb: eave doctor says how").tone(Tone::Dim),
            );
        }
        Some(
            Panel::new(ui::column(parts))
                .on_open(Message::Opened)
                .on_close(Message::Closed),
        )
    }
}

impl Battery {
    /// Beside the percentage: the time left and what it's until, with ebb's range under it, or
    /// the state when there's no time to give.
    fn outlook(&self) -> Vec<Node<Message>> {
        let reading = &self.reading;
        let estimate = reading
            .forecast
            .as_ref()
            .and_then(|forecast| forecast.estimate.as_ref());
        let until_full = match reading.charge_limit {
            Some(limit) => format!("until {limit}%"),
            None => "until full".into(),
        };
        let (when, what) = match reading.state {
            State::Charging | State::Discharging => {
                let charging = reading.state == State::Charging;
                let what = if charging {
                    until_full
                } else {
                    "until empty".into()
                };
                match self.left() {
                    Some(seconds) => (duration(seconds), what),
                    None if estimate.is_some_and(|e| e.beyond && e.to_full == charging) => {
                        ("Over 2 days".into(), what)
                    }
                    None if charging => ("Charging".into(), String::new()),
                    None => ("On battery".into(), String::new()),
                }
            }
            State::Full => ("Fully charged".into(), String::new()),
            State::NotCharging => (
                "Not charging".into(),
                match reading.charge_limit {
                    Some(limit) => format!("held at the {limit}% limit"),
                    None => "plugged in".into(),
                },
            ),
            State::Unknown => ("Unknown state".into(), String::new()),
        };
        let mut side = vec![ui::label(when)];
        if !what.is_empty() {
            side.push(ui::label(what).tone(Tone::Dim));
        }
        // Only ebb's simulated futures give a range worth reading.
        if let Some(estimate) = estimate
            && self.left().is_some()
            && estimate.method == Method::Habits
            && estimate.high - estimate.low >= 10 * 60
        {
            let range = format!("likely {}", span(estimate.low, estimate.high));
            side.push(ui::label(range).tone(Tone::Dim));
        }
        side
    }

    /// What draws the power, since the popover opened.
    fn apps(&self) -> Node<Message> {
        let Some(apps) = &self.apps else {
            return ui::label("Measuring…").tone(Tone::Dim);
        };
        let mut parts = Vec::new();
        if apps.trained && apps.total > 0.05 {
            parts.push(ui::detail("Drawing", ui::label(watts(apps.total))));
        }
        if apps.lines.is_empty() {
            parts.push(ui::label("Nothing busy").tone(Tone::Dim));
        }
        let rows = apps.lines.iter().map(|line| {
            let (name, icon) = if line.system {
                (
                    apps::system_name(&line.name),
                    ui::icon(system_icon(&line.name)),
                )
            } else {
                (line.name.clone(), ui::app_icon(line.name.clone()))
            };
            let value = if apps.trained {
                watts(line.value)
            } else {
                format!("{:.0}% CPU", line.value * 100.0)
            };
            ui::list_item(name)
                .leading(icon)
                .trailing(ui::label(value).tone(Tone::Dim))
        });
        parts.push(ui::list(rows));
        if let Some(seconds) = apps.learning {
            parts.push(
                ui::label(format!(
                    "Watts per program after {} more on battery",
                    duration(seconds as i64)
                ))
                .tone(Tone::Dim),
            );
        }
        ui::column(parts)
    }

    fn power(&self) -> Vec<Node<Message>> {
        let reading = &self.reading;
        let forecast = reading.forecast.as_ref();
        let mut power = Vec::new();
        let plugged = reading.ac_online.unwrap_or(matches!(
            reading.state,
            State::Charging | State::Full | State::NotCharging
        ));
        let source = if plugged { "Charger" } else { "Battery" };
        power.push(ui::detail("Source", ui::label(source)));
        if reading.energy_rate > 0.05 {
            let direction = match reading.state {
                State::Charging => "Charging at",
                _ => "Drawing",
            };
            power.push(ui::detail(direction, ui::label(watts(reading.energy_rate))));
        }
        if plugged && let Some(charger) = forecast.and_then(|f| f.charger) {
            power.push(ui::detail("Charger", ui::label(format!("{charger} W"))));
        }
        if let (Some(energy), Some(full)) = (reading.energy, reading.energy_full) {
            power.push(ui::detail(
                "Energy",
                of(format!("{energy:.1}"), format!("of {full:.1} Wh")),
            ));
        }
        if let Some(limit) = reading.charge_limit {
            power.push(ui::detail("Charge limit", ui::label(format!("{limit}%"))));
        }
        let Some(forecast) = forecast else {
            return power;
        };
        if let Some(asleep) = forecast.asleep {
            power.push(ui::detail(
                "Asleep",
                ui::label(format!("{asleep:.1}% an hour")),
            ));
        }
        if let Some(estimate) = &forecast.estimate
            && self.left().is_some()
        {
            let method = match estimate.method {
                Method::Habits => "From your habits",
                Method::Load => "From the recent load",
                Method::Gauge => "From the power now",
            };
            power.push(ui::detail("Estimate", ui::label(method)));
            if let Some(kernel) = forecast.kernel {
                power.push(ui::detail(
                    "Kernel's estimate",
                    ui::label(duration(kernel)).tone(Tone::Dim),
                ));
            }
        }
        if let Some(why) = &forecast.why {
            power.push(ui::label(capitalized(why)).tone(Tone::Dim));
        }
        power
    }

    fn health(&self) -> Vec<Node<Message>> {
        let reading = &self.reading;
        let mut health = Vec::new();
        if let Some(fraction) = reading.health {
            let capacity = format!("{:.0}%", fraction * 100.0);
            health.push(ui::detail("Capacity", of(capacity, "of design".into())));
        }
        if let Some(cycles) = reading.cycles {
            health.push(ui::detail("Cycles", ui::label(cycles.to_string())));
        }
        if let Some(temperature) = reading.temperature {
            health.push(ui::detail(
                "Temperature",
                ui::label(format!("{temperature:.0} °C")),
            ));
        }
        if let Some(condition) = &reading.condition {
            let tone = if condition == "Good" {
                Tone::Good
            } else {
                Tone::Warning
            };
            health.push(ui::detail(
                "Condition",
                ui::label(condition.clone()).tone(tone),
            ));
        }
        health
    }
}

/// What you seem to be doing, and for how long, as ebb sees it.
fn now(activity: &learned::Activity) -> Node<Message> {
    let icon = match &activity.program {
        Some(program) => ui::app_icon(program.clone()),
        None => ui::icon(Icon::Info),
    };
    let since = match activity.usual {
        Some(usual) if usual > 0 => format!(
            "{} in, usually {}",
            duration(activity.minutes * 60),
            duration(usual * 60)
        ),
        _ => format!("{} in", duration(activity.minutes * 60)),
    };
    ui::list([ui::list_item(activity.label.clone())
        .subtitle(since)
        .leading(icon)])
}

/// An icon for a line that isn't a program.
fn system_icon(name: &str) -> Icon {
    match name {
        "[display]" | "[gpu]" => Icon::Display,
        "[wakeups]" => Icon::Bell { muted: false },
        "[unexplained]" => Icon::Info,
        _ => Icon::Power,
    }
}

fn watts(watts: f64) -> String {
    format!("{watts:.1} W")
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// A value and, dimmed after it, what it's out of: `36.5 of 57.8 Wh`.
fn of(value: String, whole: String) -> ui::Node<Message> {
    ui::row([ui::label(value), ui::label(whole).tone(Tone::Dim)])
}

impl Battery {
    /// What to tell the user now that the reading went from `before` to the current one.
    fn alert(&mut self, before: &Reading) -> Option<Alert> {
        let reading = &self.reading;
        if !self.config.notify || !reading.present {
            return None;
        }
        let percent = self.percent();
        // Only a change: a bar starting with the charger in has nothing to say.
        let plug = match (plugged(before), plugged(reading)) {
            (Some(was), Some(is)) if was != is => Some(is),
            _ => None,
        };
        match reading.state {
            // Once per discharge, also when the bar starts on a low battery.
            State::Discharging if percent <= self.config.critical && !self.warned => {
                self.warned = true;
                Some(Alert::Low(percent))
            }
            _ if plug == Some(true) => {
                self.warned = false;
                Some(Alert::Plugged(percent))
            }
            // Charging with no plug seen going in: the low battery notification still goes.
            State::Charging | State::Full | State::NotCharging if self.warned => {
                self.warned = false;
                Some(Alert::Plugged(percent))
            }
            _ if plug == Some(false) => Some(Alert::Unplugged),
            // Only right after charging: plugging in a full battery isn't news.
            _ if before.state == State::Charging && self.charged() => Some(Alert::Charged(percent)),
            _ => None,
        }
    }

    /// Full, or held at its charge limit.
    fn charged(&self) -> bool {
        match self.reading.state {
            State::Full => true,
            State::NotCharging => self
                .reading
                .charge_limit
                .is_some_and(|limit| self.percent() >= limit),
            _ => false,
        }
    }

    fn send(&mut self, alert: Alert) -> Task<Message> {
        let note = match alert {
            Alert::Unplugged => return self.plugged.take().map_or_else(Task::none, close),
            Alert::Plugged(percent) => {
                let low = self.warning.take().map_or_else(Task::none, close);
                if !self.config.charging {
                    return low;
                }
                let note = self.plugged_note(percent);
                let send = Task::future(async move {
                    match notify::send(note).await {
                        Ok(id) => Some(Message::Plugged(id)),
                        Err(error) => {
                            eprintln!("eave-battery: notification not sent: {error}");
                            None
                        }
                    }
                })
                .and_then(Task::done);
                return Task::batch([low, send]);
            }
            Alert::Low(percent) => {
                let left = match self.left_to(false) {
                    None => String::new(),
                    Some(seconds) => format!(", about {}", duration(seconds)),
                };
                notify::Note {
                    icon: "battery-caution",
                    summary: "Battery low".into(),
                    body: format!("{percent}% left{left}. Plug in the charger."),
                    critical: true,
                    expires: None,
                    replaces: 0,
                }
            }
            Alert::Charged(percent) => notify::Note {
                icon: "battery-full-charged",
                summary: "Battery charged".into(),
                body: match self.reading.charge_limit {
                    Some(limit) if limit < 100 => format!("Charged to the {limit}% limit."),
                    _ => format!("{percent}%. You can unplug the charger."),
                },
                critical: false,
                expires: None,
                replaces: 0,
            },
        };
        let low = matches!(alert, Alert::Low(_));
        Task::future(async move {
            match notify::send(note).await {
                Ok(id) => low.then_some(Message::Warned(id)),
                Err(error) => {
                    eprintln!("eave-battery: notification not sent: {error}");
                    None
                }
            }
        })
        .and_then(Task::done)
    }

    /// That the charger went in: charging, and till when if the kernel says, or held where it is.
    fn plugged_note(&self, percent: u8) -> notify::Note {
        let (summary, body) = match self.reading.state {
            State::Full => ("Plugged in", format!("{percent}%, charged.")),
            State::NotCharging => ("Plugged in", format!("{percent}%, not charging.")),
            // Just in, the battery may still read discharging: it's about to charge.
            _ => match self.left_to(true) {
                None => ("Charging", format!("{percent}%")),
                Some(seconds) => (
                    "Charging",
                    format!("{percent}%, full in about {}.", duration(seconds)),
                ),
            },
        };
        notify::Note {
            icon: "battery-good-charging",
            summary: summary.into(),
            body,
            critical: false,
            expires: Some(PLUGGED_SHOWN),
            // Plugged in again while the last one shows: it takes its place.
            replaces: self.plugged.unwrap_or(0),
        }
    }

    /// Seconds left until empty, or until full while charging: ebb's estimate when it has one,
    /// the kernel's otherwise. None when there's none, or no direction to go.
    fn left(&self) -> Option<i64> {
        match self.reading.state {
            State::Charging => self.left_to(true),
            State::Discharging => self.left_to(false),
            _ => None,
        }
    }

    /// Seconds until full (`full`) or empty.
    fn left_to(&self, full: bool) -> Option<i64> {
        let reading = &self.reading;
        let learned = reading
            .forecast
            .as_ref()
            .and_then(|forecast| forecast.estimate.as_ref())
            .filter(|estimate| estimate.to_full == full && !estimate.beyond && estimate.median > 0)
            .map(|estimate| estimate.median);
        let kernel = if full {
            reading.time_to_full
        } else {
            reading.time_to_empty
        };
        learned.or((kernel > 0).then_some(kernel))
    }

    fn charging(&self) -> bool {
        self.reading.state == State::Charging
    }

    fn percent(&self) -> u8 {
        self.reading.percentage.round().clamp(0.0, 100.0) as u8
    }

    fn tone(&self) -> Tone {
        let percent = self.percent();
        match self.reading.state {
            State::Charging => Tone::Good,
            State::Full | State::NotCharging => Tone::Normal,
            _ if percent <= self.config.critical => Tone::Critical,
            _ if percent <= self.config.warning => Tone::Warning,
            _ => Tone::Normal,
        }
    }
}

/// Milliseconds the notification that the charger went in shows for: it's news, not a warning.
const PLUGGED_SHOWN: i32 = 4000;

/// Whether the charger is in: as the machine says, or failing that as the battery does. `None`
/// when neither can tell.
fn plugged(reading: &Reading) -> Option<bool> {
    if !reading.present {
        return None;
    }
    reading.ac_online.or(match reading.state {
        State::Charging | State::Full | State::NotCharging => Some(true),
        State::Discharging => Some(false),
        State::Unknown => None,
    })
}

fn close(id: u32) -> Task<Message> {
    Task::future(async move {
        if let Err(error) = notify::close(id).await {
            eprintln!("eave-battery: notification not closed: {error}");
        }
    })
    .discard()
}

/// `3 h 02 – 4 h 25`, or `40–55 min` under an hour.
fn span(low: i64, high: i64) -> String {
    let (low, high) = ((low + 30) / 60, (high + 30) / 60);
    if high < 60 {
        return format!("{low}–{high} min");
    }
    let part = |minutes: i64| match minutes / 60 {
        0 => format!("{minutes} min"),
        hours => format!("{hours} h {:02}", minutes % 60),
    };
    format!("{} – {}", part(low), part(high))
}

/// `2 h 05 min`, or `14 min` under an hour.
fn duration(seconds: i64) -> String {
    let minutes = (seconds + 30) / 60;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, minutes) => format!("{hours} h {minutes:02} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(percentage: f64, state: State) -> Battery {
        let (mut battery, _) = Battery::new(Config::default());
        battery.update(Message::Read(Reading {
            present: true,
            percentage,
            state,
            ..Reading::default()
        }));
        battery
    }

    #[test]
    fn warns_only_while_discharging() {
        assert_eq!(battery(50.0, State::Discharging).tone(), Tone::Normal);
        assert_eq!(battery(20.6, State::Discharging).tone(), Tone::Normal);
        assert_eq!(battery(19.6, State::Discharging).tone(), Tone::Warning);
        assert_eq!(battery(8.0, State::Discharging).tone(), Tone::Critical);
        assert_eq!(battery(8.0, State::Charging).tone(), Tone::Good);
        assert_eq!(battery(8.0, State::NotCharging).tone(), Tone::Normal);
    }

    #[test]
    fn shows_the_percentage_on_hover() {
        let content = battery(42.0, State::Discharging)
            .item()
            .into_parts()
            .content;
        let ui::Kind::Alternate {
            mut options,
            current: 0,
            hovered: Some(1),
            grows: false,
        } = content.unwrap().into_kind()
        else {
            panic!("the icon, with the percentage on hover");
        };
        assert!(matches!(
            options.pop().unwrap().into_kind(),
            ui::Kind::Label { text, .. } if text == "42%"
        ));
    }

    #[test]
    fn the_percentage_gulps_while_charging() {
        let content = battery(42.0, State::Charging).item().into_parts().content;
        let ui::Kind::Alternate { mut options, .. } = content.unwrap().into_kind() else {
            panic!("the icon, with the percentage on hover");
        };
        let ui::Kind::Gulp(label) = options.pop().unwrap().into_kind() else {
            panic!("a gulping percentage");
        };
        assert!(matches!(label.into_kind(), ui::Kind::Label { text, .. } if text == "42%"));
    }

    #[test]
    fn hidden_without_a_battery() {
        let (battery, _) = Battery::new(Config::default());
        assert!(battery.item().into_parts().content.is_none());
        assert!(battery.popover().is_none());
    }

    fn read(battery: &mut Battery, percentage: f64, state: State) -> Option<Alert> {
        let before = std::mem::replace(
            &mut battery.reading,
            Reading {
                present: true,
                percentage,
                state,
                ..Reading::default()
            },
        );
        battery.alert(&before)
    }

    #[test]
    fn warns_once_when_low() {
        let mut widget = battery(12.0, State::Discharging);
        assert_eq!(read(&mut widget, 11.0, State::Discharging), None);
        assert_eq!(
            read(&mut widget, 10.0, State::Discharging),
            Some(Alert::Low(10))
        );
        assert_eq!(read(&mut widget, 9.0, State::Discharging), None);
        assert_eq!(read(&mut widget, 9.0, State::Unknown), None);
        assert_eq!(
            read(&mut widget, 9.0, State::Charging),
            Some(Alert::Plugged(9))
        );
        assert_eq!(read(&mut widget, 30.0, State::Charging), None);
    }

    fn plug(battery: &mut Battery, percentage: f64, state: State, ac: bool) -> Option<Alert> {
        let before = std::mem::replace(
            &mut battery.reading,
            Reading {
                present: true,
                percentage,
                state,
                ac_online: Some(ac),
                ..Reading::default()
            },
        );
        battery.alert(&before)
    }

    #[test]
    fn says_when_the_charger_goes_in() {
        let (mut widget, _) = Battery::new(Config::default());
        assert_eq!(
            plug(&mut widget, 45.0, State::Discharging, false),
            None,
            "starting is no change"
        );
        // The charger is in before the battery says it's charging.
        assert_eq!(
            plug(&mut widget, 45.0, State::Discharging, true),
            Some(Alert::Plugged(45))
        );
        assert_eq!(plug(&mut widget, 45.0, State::Charging, true), None);
        assert_eq!(
            plug(&mut widget, 46.0, State::Discharging, false),
            Some(Alert::Unplugged)
        );
        // Without a charger to read, the battery says.
        let mut widget = battery(45.0, State::Discharging);
        assert_eq!(
            read(&mut widget, 45.0, State::Charging),
            Some(Alert::Plugged(45))
        );
        assert_eq!(read(&mut widget, 45.0, State::Unknown), None);
    }

    #[test]
    fn starting_plugged_in_is_no_news() {
        let (mut widget, _) = Battery::new(Config::default());
        assert_eq!(plug(&mut widget, 60.0, State::Charging, true), None);
        assert_eq!(plug(&mut widget, 61.0, State::Charging, true), None);
    }

    #[test]
    fn a_full_battery_plugged_in_says_so() {
        let mut widget = battery(100.0, State::Discharging);
        assert_eq!(
            read(&mut widget, 100.0, State::Full),
            Some(Alert::Plugged(100))
        );
        assert_eq!(widget.plugged_note(100).summary, "Plugged in");
        let mut widget = battery(45.0, State::Discharging);
        read(&mut widget, 45.0, State::Charging);
        let note = widget.plugged_note(45);
        assert_eq!(
            (note.summary.as_str(), note.expires),
            ("Charging", Some(4000))
        );
    }

    #[test]
    fn warns_when_starting_low() {
        let (mut widget, _) = Battery::new(Config::default());
        assert_eq!(
            read(&mut widget, 6.0, State::Discharging),
            Some(Alert::Low(6))
        );
    }

    #[test]
    fn says_when_charged() {
        let mut widget = battery(99.0, State::Charging);
        assert_eq!(
            read(&mut widget, 100.0, State::Full),
            Some(Alert::Charged(100))
        );
        assert_eq!(read(&mut widget, 100.0, State::Full), None);
        // Plugging in a battery that's already full isn't charging it.
        let mut full = battery(100.0, State::Discharging);
        assert_ne!(
            read(&mut full, 100.0, State::Full),
            Some(Alert::Charged(100))
        );
    }

    #[test]
    fn says_when_held_at_the_limit() {
        let mut widget = battery(79.0, State::Charging);
        let before = std::mem::replace(
            &mut widget.reading,
            Reading {
                present: true,
                percentage: 80.0,
                state: State::NotCharging,
                charge_limit: Some(80),
                ..Reading::default()
            },
        );
        assert_eq!(widget.alert(&before), Some(Alert::Charged(80)));
    }

    #[test]
    fn stays_quiet_when_told() {
        let (mut widget, _) = Battery::new(Config {
            notify: false,
            ..Config::default()
        });
        assert_eq!(read(&mut widget, 5.0, State::Discharging), None);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(885), "15 min");
        assert_eq!(duration(7500), "2 h 05 min");
        assert_eq!(span(2400, 3300), "40–55 min");
        assert_eq!(span(10_920, 15_900), "3 h 02 – 4 h 25");
        assert_eq!(span(2700, 4500), "45 min – 1 h 15");
    }

    fn learned(to_full: bool, median: i64, beyond: bool) -> Box<Forecast> {
        Box::new(Forecast {
            estimate: Some(learned::Estimate {
                to_full,
                median,
                low: median,
                high: median,
                beyond,
                method: Method::Habits,
            }),
            kernel: None,
            activity: None,
            asleep: None,
            charger: None,
            why: None,
        })
    }

    #[test]
    fn time_left_is_ebbs_then_the_kernels() {
        let mut widget = battery(62.0, State::Discharging);
        assert_eq!(widget.left(), None);
        widget.reading.time_to_empty = 3600;
        assert_eq!(widget.left(), Some(3600));
        widget.reading.forecast = Some(learned(false, 7200, false));
        assert_eq!(widget.left(), Some(7200));
        // Past what ebb looks at, or the other way: the kernel's.
        widget.reading.forecast = Some(learned(false, 7200, true));
        assert_eq!(widget.left(), Some(3600));
        widget.reading.forecast = Some(learned(true, 7200, false));
        assert_eq!(widget.left(), Some(3600));
        widget.reading.state = State::Full;
        assert_eq!(widget.left(), None);
    }

    #[test]
    fn maps_sysfs_states() {
        assert_eq!(State::from_sysfs("Charging"), State::Charging);
        assert_eq!(State::from_sysfs("Not charging"), State::NotCharging);
        assert_eq!(State::from_sysfs("Unknown"), State::Unknown);
    }
}
