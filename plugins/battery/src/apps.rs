//! What draws the power right now, per program, while the popover is open: ebb's power model
//! applied to two readings of `/proc` and `/sys` a few seconds apart ([D-134](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)).
//! Until the model has seen enough time on battery, it's each program's share of a core instead.
//! Nothing runs while the popover is closed: the thread ends when the stream is dropped.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ebb_platform::Platform;
use ebb_platform::linux::LinuxPlatform;
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::mpsc;

/// Between two readings: long enough that a program's share settles, short enough to follow it.
const WINDOW: Duration = Duration::from_secs(3);
/// Rows in the popover.
const SHOWN: usize = 6;

/// The power each program draws, busiest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Apps {
    pub lines: Vec<Line>,
    /// Watts per line, rather than a share of a core per line.
    pub trained: bool,
    /// What the battery measured, or the model's sum, in watts.
    pub total: f64,
    /// Seconds on battery the model still needs before it can say watts.
    pub learning: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub name: String,
    /// Watts, or a share of one core when not trained.
    pub value: f64,
    /// Not a program: the display, the baseline, the GPU.
    pub system: bool,
}

/// A reading every few seconds, for as long as the stream is held.
pub fn watch() -> impl Stream<Item = Apps> {
    let (sender, receiver) = mpsc::channel(1);
    let spawned = std::thread::Builder::new()
        .name("eave-battery-apps".into())
        .spawn(move || sample(&sender));
    if let Err(error) = spawned {
        eprintln!("eave-battery: apps: {error}");
    }
    stream::unfold(receiver, |mut receiver| async move {
        let apps = receiver.recv().await?;
        Some((apps, receiver))
    })
    .boxed()
}

fn sample(sender: &mpsc::Sender<Apps>) {
    let local = ::ebb::Local::open();
    let (fit, trained) = fit(&local);
    let learning = (trained < ::ebb::predict::MIN_TRAINED_S)
        .then_some(::ebb::predict::MIN_TRAINED_S - trained);
    crate::learned::trim();
    let mut platform = LinuxPlatform::new();
    let mut before = platform.snapshot();
    loop {
        std::thread::sleep(WINDOW);
        let after = platform.snapshot();
        let seconds = after.clock.monotonic_s - before.clock.monotonic_s;
        if seconds <= 0.0 {
            before = after;
            continue;
        }
        let power = ::ebb::local::power_between(&before, &after, fit.as_ref(), seconds);
        if sender.blocking_send(apps(power, learning)).is_err() {
            crate::learned::trim();
            return;
        }
        before = after;
    }
}

/// The power model, and how many seconds on battery it has learned from.
fn fit(local: &::ebb::Local) -> (Option<ebb_core::power_model::Fit>, f64) {
    if !local.recorded() {
        return (None, 0.0);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64());
    let current = local
        .store()
        .and_then(|store| ::ebb::model_store::current(&store, local.db_path(), now));
    match current {
        Ok(current) => (current.power.fit(), current.power.trained_s),
        Err(error) => {
            eprintln!("eave-battery: ebb: {error}");
            (None, 0.0)
        }
    }
}

fn apps(power: ::ebb::PowerUse, learning: Option<f64>) -> Apps {
    let mut lines: Vec<Line> = power
        .lines
        .into_iter()
        .filter(|line| line.value.is_finite() && line.value > 0.0)
        .map(|line| Line {
            name: line.name,
            value: line.value,
            system: line.system,
        })
        .collect();
    // Untrained lines come in no order.
    lines.sort_by(|a, b| b.value.total_cmp(&a.value));
    lines.truncate(SHOWN);
    Apps {
        lines,
        trained: power.trained,
        total: power.total_w,
        learning: (!power.trained).then_some(learning).flatten(),
    }
}

/// A system line's name in words: `[display]` is the display.
pub fn system_name(name: &str) -> String {
    let bare = name.trim_start_matches('[').trim_end_matches(']');
    match bare {
        "display" => "Display".into(),
        "baseline" => "System".into(),
        "gpu" => "Graphics".into(),
        "wakeups" => "Wakeups".into(),
        "kernel & untracked cpu" => "Kernel".into(),
        "unexplained" => "Unaccounted".into(),
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, value: f64, system: bool) -> ::ebb::ProgramPower {
        ::ebb::ProgramPower {
            name: name.into(),
            value,
            system,
        }
    }

    #[test]
    fn busiest_first_and_only_a_few() {
        let power = ::ebb::PowerUse {
            seconds: 3.0,
            lines: (0..10)
                .map(|i| line(&format!("p{i}"), f64::from(i) / 10.0, false))
                .chain([line("idle", 0.0, false)])
                .collect(),
            trained: false,
            total_w: 0.0,
            measured: false,
        };
        let apps = apps(power, Some(3600.0));
        assert_eq!(apps.lines.len(), SHOWN);
        assert_eq!(apps.lines[0].name, "p9");
        assert_eq!(apps.learning, Some(3600.0));
    }

    #[test]
    fn trained_says_watts_and_needs_no_learning() {
        let power = ::ebb::PowerUse {
            seconds: 3.0,
            lines: vec![line("[display]", 6.0, true), line("firefox", 4.0, false)],
            trained: true,
            total_w: 14.3,
            measured: true,
        };
        let apps = apps(power, Some(10.0));
        assert!(apps.trained);
        assert_eq!(apps.learning, None);
        assert_eq!(apps.total, 14.3);
    }

    #[test]
    fn system_lines_read_as_words() {
        assert_eq!(system_name("[display]"), "Display");
        assert_eq!(system_name("[kernel & untracked cpu]"), "Kernel");
        assert_eq!(system_name("[npu]"), "Npu");
    }
}
