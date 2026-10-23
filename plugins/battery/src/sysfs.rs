//! Batteries straight from the kernel: `/sys/class/power_supply`, re-read on the kernel's
//! `power_supply` uevents. No UPower ([D-23](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)).
//!
//! One thread serves every widget. It blocks on the uevent socket, so plugging in the charger
//! shows at once. Drivers don't send an event for every percent, so while there's a battery it
//! also re-reads every 30 s. `/sys` is the one thing we're allowed to poll.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt, stream};
use rustix::fd::OwnedFd;
use rustix::net::{self, AddressFamily, SocketType, netlink};
use tokio::sync::watch as channel;

use crate::learned::Forecaster;
use crate::{Reading, State};

const ROOT: &str = "/sys/class/power_supply";
const REFRESH: Duration = Duration::from_secs(30);

/// The reading now, then again whenever it changes.
pub fn watch() -> impl Stream<Item = Reading> {
    let receiver = readings().subscribe();
    stream::unfold((receiver, true), |(mut receiver, first)| async move {
        if !first {
            receiver.changed().await.ok()?;
        }
        let reading = receiver.borrow_and_update().clone();
        Some((reading, (receiver, false)))
    })
    .boxed()
}

fn readings() -> &'static channel::Sender<Reading> {
    static READINGS: OnceLock<channel::Sender<Reading>> = OnceLock::new();
    READINGS.get_or_init(|| {
        let (sender, _) = channel::channel(Reading::default());
        let readings = sender.clone();
        std::thread::Builder::new()
            .name("eave-battery".into())
            .spawn(move || listen(&readings))
            .expect("spawning a thread");
        sender
    })
}

fn listen(readings: &channel::Sender<Reading>) {
    let mut forecaster = Forecaster::new();
    let socket = uevents()
        .inspect_err(|error| eprintln!("eave-battery: uevent socket: {error}, polling only"))
        .ok();
    let mut buffer = vec![0u8; 8192];
    loop {
        let mut reading = read(Path::new(ROOT));
        reading.forecast = forecaster.forecast(&reading).map(Box::new);
        // Without a battery there's nothing to refresh; a new one arrives as a uevent.
        let refresh = (reading.present || socket.is_none()).then_some(REFRESH);
        readings.send_if_modified(|current| {
            let changed = *current != reading;
            *current = reading;
            changed
        });

        let Some(socket) = &socket else {
            std::thread::sleep(REFRESH);
            continue;
        };
        let deadline = refresh.map(|refresh| Instant::now() + refresh);
        loop {
            let timeout = deadline.map(|deadline| {
                // A zero timeout means "block forever" to the kernel.
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1))
            });
            if net::sockopt::set_socket_timeout(socket, net::sockopt::Timeout::Recv, timeout)
                .is_err()
            {
                std::thread::sleep(REFRESH);
                break;
            }
            match net::recv(socket, &mut buffer, net::RecvFlags::empty()) {
                Ok((length, _)) if is_power_supply(&buffer[..length]) => break,
                Ok(_) => {}
                // Timed out (or interrupted): time for the periodic re-read.
                Err(_) => break,
            }
        }
    }
}

/// A socket that receives the kernel's uevents. Unprivileged processes may listen.
fn uevents() -> rustix::io::Result<OwnedFd> {
    let socket = net::socket(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        Some(netlink::KOBJECT_UEVENT),
    )?;
    // Group 1 is the kernel's own broadcast (udev rebroadcasts on group 2).
    net::bind(&socket, &netlink::SocketAddrNetlink::new(0, 1))?;
    Ok(socket)
}

/// A uevent is `action@devpath`, then `KEY=value` fields, all NUL-separated.
fn is_power_supply(event: &[u8]) -> bool {
    event
        .split(|&byte| byte == 0)
        .any(|field| field == b"SUBSYSTEM=power_supply")
}

/// One battery's state, in watt-hours and watts.
#[derive(Debug, Default)]
struct Supply {
    state: State,
    capacity: Option<f64>,
    energy: Option<f64>,
    energy_full: Option<f64>,
    power: Option<f64>,
    time_to_empty: Option<i64>,
    time_to_full: Option<i64>,
    energy_full_design: Option<f64>,
    cycles: Option<u32>,
    /// °C.
    temperature: Option<f64>,
    /// The kernel's `health`, like `Good`.
    health: Option<String>,
    /// The charge it stops at, in percent, when that's under 100.
    charge_limit: Option<u8>,
}

/// Every system battery under `root`, combined into one reading the way UPower's display device
/// does. Peripherals (a mouse's battery) have `scope = Device` and are skipped.
pub(crate) fn read(root: &Path) -> Reading {
    let Ok(entries) = fs::read_dir(root) else {
        return Reading::default();
    };
    let dirs: Vec<_> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    let mut supplies: Vec<Supply> = dirs.iter().filter_map(|dir| supply(dir)).collect();
    // read_dir order isn't stable; the combination shouldn't depend on it.
    supplies.sort_by(|a, b| {
        a.energy_full
            .partial_cmp(&b.energy_full)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut reading = combine(&supplies);
    if reading.present {
        reading.ac_online = ac_online(&dirs);
    }
    reading
}

/// Whether a charger is plugged in: a mains or USB supply that's online. `None` when the machine
/// reports no such supply at all.
fn ac_online(dirs: &[std::path::PathBuf]) -> Option<bool> {
    let mut seen = None;
    for dir in dirs {
        let text = |name: &str| fs::read_to_string(dir.join(name)).ok();
        let kind = text("type").unwrap_or_default();
        if !matches!(kind.trim(), "Mains" | "USB") {
            continue;
        }
        let Some(online) = text("online") else {
            continue;
        };
        let online = online.trim() == "1";
        seen = Some(seen.unwrap_or(false) || online);
    }
    seen
}

fn supply(dir: &Path) -> Option<Supply> {
    let text = |name: &str| {
        fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| s.trim().to_owned())
    };
    let number = |name: &str| text(name)?.parse::<f64>().ok();

    if text("type")? != "Battery" || text("scope").as_deref() == Some("Device") {
        return None;
    }
    if text("present").as_deref() == Some("0") {
        return None;
    }

    // Charge (µAh) and current (µA) need a voltage to become energy and power.
    let voltage = number("voltage_now").filter(|v| *v > 0.0);
    let design_voltage = number("voltage_min_design")
        .filter(|v| *v > 0.0)
        .or(voltage);
    let energy = |name: &str, charge: &str| {
        number(name)
            .map(|uwh| uwh / 1e6)
            .or_else(|| Some(number(charge)? * design_voltage? / 1e12))
    };
    let power = number("power_now")
        .map(|uw| uw.abs() / 1e6)
        .or_else(|| Some((number("current_now")? * voltage? / 1e12).abs()));
    let seconds = |name: &str| number(name).map(|s| s as i64).filter(|s| *s > 0);

    Some(Supply {
        state: State::from_sysfs(&text("status").unwrap_or_default()),
        capacity: number("capacity"),
        energy: energy("energy_now", "charge_now"),
        energy_full: energy("energy_full", "charge_full"),
        power,
        time_to_empty: seconds("time_to_empty_now"),
        time_to_full: seconds("time_to_full_now"),
        energy_full_design: energy("energy_full_design", "charge_full_design"),
        cycles: number("cycle_count")
            .filter(|cycles| *cycles > 0.0)
            .map(|cycles| cycles as u32),
        // Tenths of a degree.
        temperature: number("temp").map(|tenths| tenths / 10.0),
        health: text("health").filter(|health| !health.is_empty() && health != "Unknown"),
        charge_limit: number("charge_control_end_threshold")
            .filter(|limit| *limit > 0.0 && *limit < 100.0)
            .map(|limit| limit as u8),
    })
}

fn combine(supplies: &[Supply]) -> Reading {
    let [first, rest @ ..] = supplies else {
        return Reading::default();
    };
    let any = |state| supplies.iter().any(|s| s.state == state);
    let state = if any(State::Charging) {
        State::Charging
    } else if any(State::Discharging) {
        State::Discharging
    } else if supplies.iter().all(|s| s.state == State::Full) {
        State::Full
    } else if any(State::NotCharging) || any(State::Full) {
        State::NotCharging
    } else {
        first.state
    };

    let sum =
        |field: fn(&Supply) -> Option<f64>| -> Option<f64> { supplies.iter().map(field).sum() };
    let energy = sum(|s| s.energy);
    let energy_full = sum(|s| s.energy_full).filter(|full| *full > 0.0);
    let power = sum(|s| s.power).unwrap_or(0.0);

    // Stored energy over full energy, as UPower computes it, so the percentage and the time left
    // agree. `capacity` can differ: on Apple Silicon it's the firmware's rounded-up display
    // value (98% where the energy says 93%). It's only the fallback.
    let percentage = match (energy, energy_full) {
        (Some(energy), Some(full)) => energy / full * 100.0,
        _ => {
            let capacities: Vec<f64> = supplies.iter().filter_map(|s| s.capacity).collect();
            capacities.iter().sum::<f64>() / capacities.len().max(1) as f64
        }
    };

    let hours_to = |target: Option<f64>| -> i64 {
        match (target, power > 0.1) {
            (Some(wh), true) => (wh / power * 3600.0) as i64,
            _ => 0,
        }
    };
    let estimate =
        |field: fn(&Supply) -> Option<i64>, fallback: i64| match (rest.is_empty(), field(first)) {
            (true, Some(seconds)) => seconds,
            _ => fallback,
        };
    // Past a few days the estimate means nothing (a nearly idle battery).
    let sane = |seconds: i64| if seconds < 5 * 24 * 3600 { seconds } else { 0 };
    let (time_to_empty, time_to_full) = match state {
        State::Discharging => (sane(estimate(|s| s.time_to_empty, hours_to(energy))), 0),
        State::Charging => {
            let missing = energy
                .zip(energy_full)
                .map(|(now, full)| (full - now).max(0.0));
            (0, sane(estimate(|s| s.time_to_full, hours_to(missing))))
        }
        _ => (0, 0),
    };

    // Health is about the battery, so with more than one only a total makes sense: full energy
    // over design energy, once every battery reports both.
    let design = sum(|s| s.energy_full_design).filter(|design| *design > 0.0);
    let one =
        |field: fn(&Supply) -> Option<String>| rest.is_empty().then(|| field(first)).flatten();
    Reading {
        present: true,
        batteries: supplies.len(),
        percentage: percentage.clamp(0.0, 100.0),
        state,
        time_to_empty,
        time_to_full,
        energy_rate: power,
        energy,
        energy_full,
        health: energy_full.zip(design).map(|(full, design)| full / design),
        cycles: supplies.iter().filter_map(|s| s.cycles).max(),
        temperature: supplies
            .iter()
            .filter_map(|s| s.temperature)
            .reduce(f64::max),
        condition: one(|s| s.health.clone()),
        charge_limit: supplies.iter().filter_map(|s| s.charge_limit).min(),
        ac_online: None,
        forecast: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eave-battery-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn supply(root: &Path, name: &str, files: &[(&str, &str)]) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        for (file, value) in files {
            fs::write(dir.join(file), format!("{value}\n")).unwrap();
        }
    }

    #[test]
    fn no_battery_is_absent() {
        let root = root("none");
        supply(&root, "AC", &[("type", "Mains"), ("online", "1")]);
        assert!(!read(&root).present);
        assert!(!read(&root.join("missing")).present);
    }

    #[test]
    fn reads_a_charge_battery() {
        // macsmc-battery on Asahi: charge_* in µAh, power_now in µW.
        let root = root("charge");
        supply(
            &root,
            "macsmc-battery",
            &[
                ("type", "Battery"),
                ("scope", "System"),
                ("status", "Discharging"),
                ("capacity", "98"),
                ("charge_now", "4922000"),
                ("charge_full", "5097000"),
                ("voltage_min_design", "9000000"),
                ("voltage_now", "12793000"),
                ("power_now", "8405000"),
            ],
        );
        let reading = read(&root);
        assert!(reading.present);
        assert_eq!(reading.state, State::Discharging);
        assert!((reading.percentage - 4922.0 / 5097.0 * 100.0).abs() < 1e-9);
        assert!((reading.energy_rate - 8.405).abs() < 1e-6);
        // 4.922 Ah × 9 V = 44.3 Wh at 8.4 W.
        assert_eq!(reading.time_to_empty, (44.298 / 8.405 * 3600.0) as i64);
    }

    #[test]
    fn reads_health_and_the_charger() {
        // This machine's macsmc-battery and macsmc-ac, plus a USB-C port with a charger in it.
        let root = root("health");
        supply(
            &root,
            "macsmc-battery",
            &[
                ("type", "Battery"),
                ("scope", "System"),
                ("status", "Not charging"),
                ("health", "Good"),
                ("cycle_count", "449"),
                ("energy_full_design", "69255000"),
                ("energy_full", "57809400"),
                ("energy_now", "36525600"),
                ("power_now", "-14013000"),
                ("temp", "329"),
                ("charge_control_end_threshold", "80"),
            ],
        );
        supply(&root, "macsmc-ac", &[("type", "Mains"), ("online", "0")]);
        supply(
            &root,
            "tps6598x-source-psy-0-0038",
            &[("type", "USB"), ("online", "1")],
        );
        let reading = read(&root);
        assert_eq!(reading.cycles, Some(449));
        assert_eq!(reading.temperature, Some(32.9));
        assert_eq!(reading.condition.as_deref(), Some("Good"));
        assert_eq!(reading.charge_limit, Some(80));
        assert_eq!(reading.ac_online, Some(true));
        let health = reading.health.unwrap();
        assert!((health - 57.8094 / 69.255).abs() < 1e-9, "{health}");
        assert!((reading.energy.unwrap() - 36.5256).abs() < 1e-9);
    }

    #[test]
    fn no_charger_supply_means_unknown() {
        let root = root("no-charger");
        supply(
            &root,
            "BAT0",
            &[
                ("type", "Battery"),
                ("capacity", "50"),
                ("charge_control_end_threshold", "100"),
            ],
        );
        let reading = read(&root);
        assert_eq!(reading.ac_online, None);
        assert_eq!(reading.charge_limit, None, "100% is no limit");
        assert_eq!(reading.health, None);
    }

    #[test]
    fn prefers_the_kernels_estimate() {
        let root = root("estimate");
        supply(
            &root,
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Charging"),
                ("capacity", "50"),
                ("energy_now", "25000000"),
                ("energy_full", "50000000"),
                ("power_now", "10000000"),
                ("time_to_full_now", "4000"),
            ],
        );
        let reading = read(&root);
        assert_eq!(reading.state, State::Charging);
        assert_eq!(reading.time_to_full, 4000);
        assert_eq!(reading.time_to_empty, 0);
    }

    #[test]
    fn combines_two_batteries_by_energy() {
        let root = root("two");
        let battery = |status, now: &str, full: &str| {
            vec![
                ("type", "Battery"),
                ("status", status),
                ("energy_now", now),
                ("energy_full", full),
                ("power_now", "5000000"),
            ]
            .into_iter()
            .map(|(k, v)| (k, v.to_owned()))
            .collect::<Vec<_>>()
        };
        for (name, files) in [
            ("BAT0", battery("Discharging", "10000000", "20000000")),
            ("BAT1", battery("Not charging", "70000000", "80000000")),
        ] {
            let files: Vec<(&str, &str)> = files.iter().map(|(k, v)| (*k, v.as_str())).collect();
            supply(&root, name, &files);
        }
        let reading = read(&root);
        assert_eq!(reading.state, State::Discharging);
        assert_eq!(reading.percentage, 80.0);
        assert!((reading.energy_rate - 10.0).abs() < 1e-6);
        assert_eq!(reading.time_to_empty, 8 * 3600);
    }

    #[test]
    fn falls_back_to_capacity() {
        let root = root("capacity");
        supply(
            &root,
            "BAT0",
            &[("type", "Battery"), ("status", "Full"), ("capacity", "100")],
        );
        let reading = read(&root);
        assert_eq!(reading.percentage, 100.0);
        assert_eq!(reading.state, State::Full);
    }

    #[test]
    fn skips_peripherals() {
        let root = root("peripheral");
        supply(
            &root,
            "hidpp_battery_0",
            &[("type", "Battery"), ("scope", "Device"), ("capacity", "40")],
        );
        assert!(!read(&root).present);
    }

    #[test]
    fn recognises_power_supply_events() {
        assert!(is_power_supply(
            b"change@/devices/platform/macsmc-battery\0ACTION=change\0SUBSYSTEM=power_supply\0"
        ));
        assert!(!is_power_supply(
            b"add@/devices/usb1\0ACTION=add\0SUBSYSTEM=usb\0"
        ));
    }
}
