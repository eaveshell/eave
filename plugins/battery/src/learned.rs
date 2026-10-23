//! Time left as ebb learns it ([D-134](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)):
//! from what the machine costs to run and what you tend to do next, with a range. ebb's collector
//! records in the background; this only reads its recording, read-only, on the battery thread.
//! Without a recording there's no forecast and the kernel's estimate stands.
//!
//! A forecast costs ebb tens of milliseconds and a passing ~12 MB (it brings its models up to
//! date from the recording), so it's made every few minutes, and in between the time left counts
//! down by the clock. The memory goes back to the system right after.

use std::time::{Duration, Instant};

use crate::{Reading, State};

/// How often to forecast again while the battery keeps charging, or discharging: ebb's estimate
/// moves slowly, and between forecasts it counts down by the clock.
const AGAIN: Duration = Duration::from_secs(5 * 60);

/// What ebb makes of the battery now.
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    /// None while plugged in and held at the limit: there's nothing to wait for.
    pub estimate: Option<Estimate>,
    /// The kernel's own estimate, seconds, to compare.
    pub kernel: Option<i64>,
    /// What you seem to be doing.
    pub activity: Option<Activity>,
    /// Percent of the battery a suspended hour costs, to a tenth.
    pub asleep: Option<f64>,
    /// What the charger offers, in whole watts.
    pub charger: Option<u32>,
    /// Why a better method wasn't used, when one wasn't.
    pub why: Option<String>,
}

/// Seconds to empty, or to full (or the charge limit) while charging, rounded to the minute so
/// an estimate that didn't move doesn't redraw.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub to_full: bool,
    pub median: i64,
    /// The 10–90% range; both equal to the median when the method gives one number.
    pub low: i64,
    pub high: i64,
    /// Further out than ebb simulates (two days): read as "more than".
    pub beyond: bool,
    pub method: Method,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Many simulated futures of what you usually do.
    Habits,
    /// The load of the last minutes, held.
    Load,
    /// The battery's own power reading, held.
    Gauge,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    /// ebb's name for it: the programs it's about and what it costs, `firefox · 9 W`.
    pub label: String,
    /// The program it's most about, for its icon.
    pub program: Option<String>,
    /// Minutes in it so far, and how long it usually lasts.
    pub minutes: i64,
    pub usual: Option<i64>,
}

/// Forecasts on the battery thread, again only when it's due.
pub struct Forecaster {
    local: ::ebb::Local,
    last: Option<Last>,
}

struct Last {
    at: Instant,
    state: State,
    plugged: Option<bool>,
    forecast: Option<Forecast>,
}

impl Forecaster {
    pub fn new() -> Self {
        Self {
            local: ::ebb::Local::open(),
            last: None,
        }
    }

    /// The forecast to go with `reading`: worked out again after a minute, or at once when the
    /// battery starts or stops charging. None without a recording, or with more than one battery
    /// (ebb follows only the first).
    pub fn forecast(&mut self, reading: &Reading) -> Option<Forecast> {
        if !reading.present || reading.batteries != 1 || !self.local.recorded() {
            self.last = None;
            return None;
        }
        if let Some(last) = &self.last
            && last.at.elapsed() < AGAIN
            && last.state == reading.state
            && last.plugged == reading.ac_online
        {
            return last.forecast.as_ref().map(|f| aged(f, last.at.elapsed()));
        }
        let forecast = match self.local.forecast() {
            Ok(outlook) => Some(from_outlook(&outlook)),
            Err(error) => {
                eprintln!("eave-battery: ebb: {error}");
                None
            }
        };
        trim();
        self.last = Some(Last {
            at: Instant::now(),
            state: reading.state,
            plugged: reading.ac_online,
            forecast: forecast.clone(),
        });
        forecast
    }
}

pub fn from_outlook(outlook: &::ebb::Outlook) -> Forecast {
    let estimate = outlook.estimate.as_ref().map(|estimate| Estimate {
        to_full: estimate.to_full,
        median: minute(estimate.median_s),
        low: minute(estimate.low_s.min(estimate.median_s)),
        high: minute(estimate.high_s.max(estimate.median_s)),
        beyond: estimate.beyond_horizon,
        method: match estimate.method {
            ebb_core::forecast::Method::Behaviour => Method::Habits,
            ebb_core::forecast::Method::Workload => Method::Load,
            ebb_core::forecast::Method::Rate => Method::Gauge,
        },
    });
    let activity = outlook.activity.as_ref().map(|activity| Activity {
        label: activity.label.clone(),
        program: activity.programs.first().cloned(),
        minutes: (activity.elapsed_s / 60.0).round() as i64,
        usual: activity.usual_s.map(|usual| (usual / 60.0).round() as i64),
    });
    Forecast {
        estimate,
        kernel: outlook
            .kernel_s
            .filter(|seconds| *seconds > 0.0)
            .map(minute),
        activity,
        asleep: outlook
            .asleep_pct_per_h
            .filter(|pct| pct.is_finite() && *pct > 0.0)
            .map(|pct| (pct * 10.0).round() / 10.0),
        charger: outlook
            .reading
            .supply_limit_w
            .filter(|watts| *watts > 0.5)
            .map(|watts| watts.round() as u32),
        why: outlook.why.clone(),
    }
}

/// `forecast`, `elapsed` later: that much less to go, and that much longer in what you're doing.
fn aged(forecast: &Forecast, elapsed: Duration) -> Forecast {
    let minutes = (elapsed.as_secs() / 60) as i64;
    let mut forecast = forecast.clone();
    let less = |seconds: &mut i64| *seconds = (*seconds - minutes * 60).max(0);
    if let Some(estimate) = &mut forecast.estimate {
        less(&mut estimate.median);
        less(&mut estimate.low);
        less(&mut estimate.high);
    }
    if let Some(kernel) = &mut forecast.kernel {
        less(kernel);
    }
    if let Some(activity) = &mut forecast.activity {
        activity.minutes += minutes;
    }
    forecast
}

/// Hands the memory a forecast passed through back to the system: glibc keeps freed memory for
/// the thread that freed it otherwise, and RSS would stay ~12 MB up for good.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free memory; it has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Seconds, to the minute.
fn minute(seconds: f64) -> i64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    ((seconds / 60.0).round() as i64) * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outlook() -> ::ebb::Outlook {
        ::ebb::Outlook {
            reading: ::ebb::Reading {
                percent: 62.0,
                energy_wh: 35.0,
                full_wh: 56.0,
                status: ebb_core::types::ChargeStatus::Discharging,
                plugged: false,
                charging: false,
                power_w: Some(-14.3),
                charge_limit_pct: 100.0,
                holding: false,
                supply_limit_w: None,
            },
            estimate: Some(::ebb::Estimate {
                to_full: false,
                median_s: 13_270.4,
                low_s: 10_930.0,
                high_s: 15_901.0,
                method: ebb_core::forecast::Method::Behaviour,
                load_w: None,
                beyond_horizon: false,
            }),
            kernel_s: Some(10_680.0),
            activity: Some(::ebb::Activity {
                state: 4,
                label: "firefox · 9 W".into(),
                programs: vec!["firefox".into()],
                elapsed_s: 745.0,
                usual_s: Some(1490.0),
            }),
            asleep_pct_per_h: Some(2.7197),
            why: None,
        }
    }

    #[test]
    fn rounds_to_the_minute_and_keeps_the_range() {
        let forecast = from_outlook(&outlook());
        let estimate = forecast.estimate.unwrap();
        assert_eq!(estimate.median, 221 * 60);
        assert_eq!(estimate.low, 182 * 60);
        assert_eq!(estimate.high, 265 * 60);
        assert_eq!(estimate.method, Method::Habits);
        assert_eq!(forecast.kernel, Some(178 * 60));
        assert_eq!(forecast.asleep, Some(2.7));
        let activity = forecast.activity.unwrap();
        assert_eq!(activity.program.as_deref(), Some("firefox"));
        assert_eq!((activity.minutes, activity.usual), (12, Some(25)));
    }

    #[test]
    fn a_range_never_leaves_out_the_median() {
        let mut outlook = outlook();
        let estimate = outlook.estimate.as_mut().unwrap();
        estimate.low_s = 20_000.0;
        estimate.method = ebb_core::forecast::Method::Rate;
        let estimate = from_outlook(&outlook).estimate.unwrap();
        assert_eq!(estimate.low, estimate.median);
        assert_eq!(estimate.method, Method::Gauge);
    }

    #[test]
    fn counts_down_between_forecasts() {
        let forecast = from_outlook(&outlook());
        let later = aged(&forecast, Duration::from_secs(150));
        let estimate = later.estimate.unwrap();
        assert_eq!(estimate.median, 219 * 60);
        assert_eq!(estimate.low, 180 * 60);
        assert_eq!(later.activity.unwrap().minutes, 14);
        let much_later = aged(&forecast, Duration::from_secs(48 * 3600));
        assert_eq!(much_later.estimate.unwrap().high, 0);
    }

    #[test]
    fn held_at_the_limit_has_no_estimate() {
        let mut outlook = outlook();
        outlook.estimate = None;
        outlook.activity = None;
        outlook.reading.supply_limit_w = Some(64.6);
        let forecast = from_outlook(&outlook);
        assert_eq!(forecast.estimate, None);
        assert_eq!(forecast.charger, Some(65));
    }
}
