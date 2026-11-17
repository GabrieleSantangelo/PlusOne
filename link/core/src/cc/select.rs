//! Which controller a QUIC path runs, for a live A/B of them:
//! PLUSONE_LINK_CC, read once (at Node::new), and its tunables,
//! PLUSONE_LINK_CC_PARAMS="key=value,...". gcc (the default: today's, as it
//! was), gcc-l23 (with the l23 rules), copa, c4, and quinn's own bbr and
//! cubic as references. A name not known is gcc; a tunable not known, not
//! a number or out of its safe range keeps its default; each said in the
//! log, and the controller with all its tunables at start ("congestion
//! control: NAME KEY=VALUE ..."). Nothing refused stops the link.

use super::c4::C4Params;
use super::copa::CopaParams;
use super::GccParams;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Choice {
    Gcc(GccParams),
    Copa(CopaParams),
    C4(C4Params),
    Bbr,
    Cubic,
}

/// The names PLUSONE_LINK_CC takes.
pub(crate) const NAMES: [&str; 6] = ["gcc", "gcc-l23", "copa", "c4", "bbr", "cubic"];

impl Choice {
    /// The controller named (none: gcc-l23) with its tunables as text
    /// ("key=value,..."), and what was refused, a line each.
    pub(crate) fn parse(name: Option<&str>, params: Option<&str>) -> (Choice, Vec<String>) {
        let mut notes = Vec::new();
        let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("gcc-l23").to_ascii_lowercase();
        let mut choice = match name.as_str() {
            "gcc" => Choice::Gcc(GccParams::default()),
            "gcc-l23" => Choice::Gcc(GccParams { l23: true, ..GccParams::default() }),
            "copa" => Choice::Copa(CopaParams::default()),
            "c4" => Choice::C4(C4Params::default()),
            "bbr" => Choice::Bbr,
            "cubic" => Choice::Cubic,
            other => {
                notes.push(format!("PLUSONE_LINK_CC: \"{other}\" is none of {}: gcc-l23", NAMES.join(", ")));
                Choice::Gcc(GccParams { l23: true, ..GccParams::default() })
            }
        };
        for pair in params.unwrap_or("").split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let set = match pair.split_once('=') {
                Some((key, value)) => choice.set(key.trim(), value.trim()),
                None => Err("not key=value".into()),
            };
            if let Err(why) = set {
                notes.push(format!("PLUSONE_LINK_CC_PARAMS: \"{pair}\" refused ({why}): its default kept"));
            }
        }
        if let Choice::Gcc(params) = &mut choice {
            if let Some(why) = params.check() {
                notes.push(format!("PLUSONE_LINK_CC_PARAMS: {why}"));
            }
        }
        (choice, notes)
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match self {
            Choice::Gcc(params) => params.set(key, value),
            Choice::Copa(params) => params.set(key, value),
            Choice::C4(params) => params.set(key, value),
            Choice::Bbr | Choice::Cubic => Err(format!("{} takes none", self.name())),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Choice::Gcc(params) if params.l23 => "gcc-l23",
            Choice::Gcc(_) => "gcc",
            Choice::Copa(_) => "copa",
            Choice::C4(_) => "c4",
            Choice::Bbr => "bbr",
            Choice::Cubic => "cubic",
        }
    }

    /// Its name and every tunable, as the log says it.
    pub(crate) fn describe(&self) -> String {
        let params = match self {
            Choice::Gcc(params) => params.describe(),
            Choice::Copa(params) => params.describe(),
            Choice::C4(params) => params.describe(),
            Choice::Bbr | Choice::Cubic => "(quinn's own, no tunables)".into(),
        };
        format!("{} {params}", self.name())
    }
}

/// The controller this program's QUIC paths run: read from the environment
/// once, and said in the log then.
pub(crate) fn selected() -> &'static Choice {
    static CHOICE: OnceLock<Choice> = OnceLock::new();
    CHOICE.get_or_init(|| {
        let name = std::env::var("PLUSONE_LINK_CC").ok();
        let params = std::env::var("PLUSONE_LINK_CC_PARAMS").ok();
        let (choice, notes) = Choice::parse(name.as_deref(), params.as_deref());
        for note in notes {
            crate::log!("{note}");
        }
        crate::log!("congestion control: {}", choice.describe());
        choice
    })
}

/// A tunable's number, within lo..=hi.
pub(crate) fn number(value: &str, lo: f64, hi: f64) -> Result<f64, String> {
    let n: f64 = value.parse().map_err(|_| format!("\"{value}\" is no number"))?;
    if !n.is_finite() || n < lo || n > hi {
        return Err(format!("{value} is out of {lo}..={hi}"));
    }
    Ok(n)
}

/// A tunable's yes or no: 1/0, true/false, on/off, yes/no.
pub(crate) fn flag(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        _ => Err(format!("\"{value}\" is neither 1 nor 0")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cc::{Budget, CcConfig};
    use quinn::congestion::ControllerFactory;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn each_name_builds_its_controller() {
        for name in NAMES {
            let (choice, notes) = Choice::parse(Some(name), None);
            assert!(notes.is_empty(), "{name}: {notes:?}");
            assert_eq!(choice.name(), name);
            assert!(choice.describe().starts_with(&format!("{name} ")), "{}", choice.describe());
            let config = Arc::new(CcConfig::with(choice));
            let controller = config.clone().build(Instant::now(), 1452);
            // A window quinn paces (it stops pacing past u32::MAX), of a few packets at least.
            let window = controller.window();
            assert!(window >= 2 * 1452 && window <= u32::MAX as u64, "{name}: window {window}");
            assert!(config.rate.load(std::sync::atomic::Ordering::Relaxed) > 0, "{name}: no rate");
        }
    }

    #[test]
    fn none_or_unknown_is_gcc_l23() {
        assert_eq!(Choice::parse(None, None), (Choice::Gcc(GccParams { l23: true, ..GccParams::default() }), vec![]));
        let l23 = || Choice::Gcc(GccParams { l23: true, ..GccParams::default() });
        assert_eq!(Choice::parse(Some(" "), None).0, l23());
        assert_eq!(Choice::parse(Some("GCC-L23"), None).0.name(), "gcc-l23");
        let (choice, notes) = Choice::parse(Some("reno"), None);
        assert_eq!(choice, l23());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("\"reno\"") && notes[0].ends_with(": gcc-l23"), "{}", notes[0]);
    }

    #[test]
    fn params_are_taken_or_refused_one_by_one() {
        let (choice, notes) = Choice::parse(Some("gcc-l23"),
            Some(" threshold_min=11, overuse_send_time=1,beta=0.9 ,held_ms=40, overuse_ms=10"));
        assert!(notes.is_empty(), "{notes:?}");
        let Choice::Gcc(p) = &choice else { panic!("{choice:?}") };
        assert!(p.l23 && p.overuse_send_time);
        assert_eq!((p.threshold_min, p.beta, p.held_ms, p.overuse_ms), (11.0, 0.9, 40.0, 10.0));
        let text = choice.describe();
        for key in ["threshold_min=11", "threshold_max=600", "threshold_gain=4", "k_up=0.0087", "k_down=0.039",
                    "overuse_ms=10", "overuse_send_time=1", "beta=0.9", "queue_limit_ms=20", "loss_cut=0.1",
                    "silence_ms=60", "held_ms=40", "burst_most_ms=100"] {
            assert!(text.contains(key), "{key} not in {text}");
        }
        // Unknown, not a number, out of range, not key=value, gcc-l23's
        // under gcc: each refused, its default kept, the rest taken.
        let (choice, notes) = Choice::parse(Some("gcc"),
            Some("thresold_min=10,beta=lots,k_up=7,overuse_ms,silence_ms=80,threshold_min=10,beta=NaN"));
        assert_eq!(notes.len(), 6, "{notes:?}");
        assert!(notes.iter().all(|n| n.contains("refused") && n.ends_with("its default kept")), "{notes:?}");
        assert_eq!(choice, Choice::Gcc(GccParams { threshold_min: 10.0, ..GccParams::default() }));
        // What does not hold together: both back.
        let (choice, notes) = Choice::parse(None, Some("threshold_min=50,threshold_max=20"));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(choice, Choice::Gcc(GccParams { l23: true, ..GccParams::default() }));
        // References take none; Copa its delta.
        let (choice, notes) = Choice::parse(Some("bbr"), Some("delta=0.2"));
        assert_eq!((choice, notes.len()), (Choice::Bbr, 1));
        let (choice, notes) = Choice::parse(Some("copa"), Some("delta=0.25"));
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(choice, Choice::Copa(CopaParams { delta: 0.25 }));
        assert!(choice.describe().contains("delta=0.25"), "{}", choice.describe());
        let (_, notes) = Choice::parse(Some("copa"), Some("delta=0"));
        assert_eq!(notes.len(), 1, "{notes:?}");
        let (choice, notes) = Choice::parse(Some("c4"), Some("delay_max=40,beta=0.9,wcap=1"));
        assert_eq!(notes.len(), 1, "{notes:?}");
        let Choice::C4(p) = &choice else { panic!("{choice:?}") };
        assert_eq!((p.delay_max, p.beta, p.wcap), (40.0, 0.25, 1.0));
    }

    #[test]
    fn the_budget_comes_from_each() {
        // Every controller, built as quinn builds it, tells a budget once its
        // first acknowledgement frame is read (here one with none in it).
        for name in NAMES {
            let config = Arc::new(CcConfig::with(Choice::parse(Some(name), None).0));
            let budget: Arc<Budget> = config.budget.clone();
            let mut controller = config.clone().build(Instant::now(), 1452);
            let now = Instant::now() + std::time::Duration::from_millis(200);
            controller.on_end_acks(now, 0, false, None);
            assert!(budget.last().0 >= 1, "{name}: no budget told");
            assert!(config.rate.load(std::sync::atomic::Ordering::Relaxed) >= 30_000, "{name}");
        }
    }
}
