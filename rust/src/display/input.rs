//! Fingers and the pen from the app, onto the virtual monitor (appdisplay.py
//! Input). Each packet says what touches the screen now (DisplayInput.kt);
//! the fingers work as on a laptop's touchpad, where they touch:
//! - one tapped: the left button; moved: a drag with it; held: the right;
//! - two tapped: the right button; moved together: scrolling that goes on
//!   after they lift (faster flicked, farther); apart or together: a pinch,
//!   as touches (zoom);
//! - three swiped: the overview up, out of it down, sideways a workspace.

use super::monitor::Injector;
use super::pen::{Pen, BTN_LEFT, BTN_RIGHT, IN_RANGE, REPORT_ID};
use std::collections::VecDeque;
use std::sync::Arc;

const TOUCH_MAX: f64 = 65535.0;
const MAX_FINGERS: usize = 10;
const FINGER: usize = 5;      // id, x, y (0..65535 of the screen)
const PEN_SAMPLE: usize = 9;  // the S Pen's report after its ID
pub const INPUT_SILENCE: f64 = 0.4;  // s: the app repeats what touches every 0.1 s; quiet this long, all is lifted
const TAP_TIME: f64 = 0.3;
const TAP_SLOP: f64 = 0.012;
const TAP_MOVE: f64 = 0.02;
const DRAG_WAIT: f64 = 0.1;
const DRAG_FAST: f64 = 0.05;
const PROMOTE: f64 = 0.25;
const HOLD: f64 = 0.6;
const GESTURE_START: f64 = 0.015;
const SWIPE: f64 = 0.1;
const VELOCITY_WINDOW: f64 = 0.1;
const FLING_START: f64 = 300.0;  // monitor pixels a second
const FLING_TAU: f64 = 0.35;
const FLING_STOP: f64 = 20.0;
const KEY_SUPER: u32 = 0xffeb;
const KEY_ESCAPE: u32 = 0xff1b;
const KEY_PAGE_UP: u32 = 0xff55;
const KEY_PAGE_DOWN: u32 = 0xff56;
const HIDPI_WIDTH: f64 = 2560.0;

type Point = (f64, f64);
type Fingers = Vec<(u8, Point)>;  // in the packet's order

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Pending,
    Drag,
    Scroll,
    Pinch,
    Swipe,
    Done,
}

/// What the fingers do, from the first one down to the last one up.
struct Gesture {
    since: f64,
    kind: Kind,
    start: Fingers,          // where each came down, in order
    most: usize,             // fingers at once, at most
    moved: f64,              // the farthest a finger went, of the screen
    fingers: Fingers,
    pair: Option<(f64, Point)>,  // the first two fingers' spread and middle, together
    last: Point,             // their middle, as last scrolled from
    samples: VecDeque<(f64, f64, f64)>,  // (time, dx, dy) of the scroll, for its speed
    touching: Fingers,       // a pinch's fingers, as the desktop has them
}

fn find(fingers: &Fingers, id: u8) -> Option<Point> {
    fingers.iter().find(|(f, _)| *f == id).map(|(_, p)| *p)
}

fn middle(fingers: &Fingers) -> Point {
    let n = fingers.len() as f64;
    (fingers.iter().map(|(_, p)| p.0).sum::<f64>() / n, fingers.iter().map(|(_, p)| p.1).sum::<f64>() / n)
}

fn spread(fingers: &Fingers) -> f64 {
    let (a, b) = (fingers[0].1, fingers[1].1);
    (b.0 - a.0).hypot(b.1 - a.1).max(1.0)
}

impl Gesture {
    fn new(since: f64) -> Gesture {
        Gesture { since, kind: Kind::Pending, start: vec![], most: 0, moved: 0.0, fingers: vec![], pair: None,
                  last: (0.0, 0.0), samples: VecDeque::new(), touching: vec![] }
    }

    fn update(&mut self, fingers: Fingers, size: Point) {
        for &(f, p) in &fingers {
            if find(&self.start, f).is_none() {
                self.start.push((f, p));
            }
        }
        self.most = self.most.max(fingers.len());
        for &(f, (x, y)) in &fingers {
            let (x0, y0) = find(&self.start, f).unwrap();
            self.moved = self.moved.max((x - x0).abs() / size.0 + (y - y0).abs() / size.1);
        }
        if fingers.len() == 2 && self.pair.is_none() {
            self.pair = Some((spread(&fingers), middle(&fingers)));
        }
        self.fingers = fingers;
    }
}

pub struct Input {
    monitor: Arc<dyn Injector>,
    size: Point,
    scale: f64,  // the monitor's pixels per desktop pixel
    make_pen: Box<dyn FnMut() -> Option<Box<dyn Pen>> + Send>,
    pen: Option<Option<Box<dyn Pen>>>,  // None: not made yet; Some(None): none to be had
    pen_last: Option<Vec<u8>>,  // the last report, while the pen is in range
    heard: f64,
    gesture: Option<Gesture>,
    fling: Option<(f64, f64, f64)>,  // vx, vy, at: a scroll going on by itself
    clock: Box<dyn Fn() -> f64 + Send>,
}

impl Input {
    pub fn new(monitor: Arc<dyn Injector>, size: (u32, u32), make_pen: Box<dyn FnMut() -> Option<Box<dyn Pen>> + Send>,
               clock: Box<dyn Fn() -> f64 + Send>) -> Input {
        let size = (size.0 as f64, size.1 as f64);
        let heard = clock();
        Input { monitor, size, scale: if size.0 >= HIDPI_WIDTH { 2.0 } else { 1.0 }, make_pen, pen: None,
                pen_last: None, heard, gesture: None, fling: None, clock }
    }

    /// body: an INPUT packet; false if it is not a well-formed one.
    pub fn packet(&mut self, body: &[u8]) -> bool {
        let Some(&count) = body.get(1) else { return false };
        let count = count as usize;
        if count > MAX_FINGERS {
            return false;
        }
        let mut at = 2;
        let mut fingers = Vec::with_capacity(count);
        for _ in 0..count {
            let Some(f) = body.get(at..at + FINGER) else { return false };
            let (x, y) = (u16::from_be_bytes([f[1], f[2]]) as f64, u16::from_be_bytes([f[3], f[4]]) as f64);
            fingers.push((f[0], (x / TOUCH_MAX * self.size.0, y / TOUCH_MAX * self.size.1)));
            at += FINGER;
        }
        let Some(&samples) = body.get(at) else { return false };
        at += 1;
        if body.len() != at + samples as usize * PEN_SAMPLE {
            return false;
        }
        self.heard = (self.clock)();
        self.touch(fingers, false);
        for i in 0..samples as usize {
            let mut report = vec![REPORT_ID];
            report.extend_from_slice(&body[at + i * PEN_SAMPLE..at + (i + 1) * PEN_SAMPLE]);
            self.pen_report(report);
        }
        true
    }

    /// Whether check() has something to look after.
    pub fn busy(&self) -> bool {
        self.gesture.is_some() || self.fling.is_some() || self.pen_last.is_some()
    }

    /// Every few ms while busy(): what time decides (a finger held, a scroll
    /// slowing down); nothing heard a while, whatever touches is lifted.
    pub fn check(&mut self) {
        let now = (self.clock)();
        if self.fling.is_some() {
            self.fling_step(now);
        }
        if self.gesture.is_some() {
            if now - self.heard >= INPUT_SILENCE {
                self.touch(vec![], true);
            } else {
                let mut g = self.gesture.take().unwrap();
                self.step(&mut g, now);
                self.gesture = Some(g);
            }
        }
        if let Some(last) = self.pen_last.clone() {
            if now - self.heard >= INPUT_SILENCE {
                self.pen_report(lifted(&last));
            }
        }
    }

    pub fn close(&mut self) {
        if self.gesture.is_some() {
            self.touch(vec![], true);
        }
        if self.fling.is_some() {
            self.stop_fling();
        }
        if let Some(last) = self.pen_last.clone() {
            self.pen_report(lifted(&last));
        }
        if let Some(Some(pen)) = self.pen.as_mut() {
            pen.close();
        }
        self.pen = None;
    }

    // -- fingers -----------------------------------------------------------------
    fn touch(&mut self, fingers: Fingers, lost: bool) {
        let now = (self.clock)();
        if self.gesture.is_none() {
            if fingers.is_empty() {
                return;
            }
            if self.fling.is_some() {
                self.stop_fling();  // a finger down stops a scroll going on, as on a phone
            }
            self.gesture = Some(Gesture::new(now));
        }
        let mut g = self.gesture.take().unwrap();
        let lifted = fingers.is_empty();
        g.update(fingers, self.size);
        if lifted {
            self.end(&mut g, now, lost);
        } else {
            self.step(&mut g, now);
            self.gesture = Some(g);
        }
    }

    fn step(&mut self, g: &mut Gesture, now: f64) {
        if g.kind == Kind::Drag && g.fingers.len() >= 2 && now - g.since < PROMOTE {
            // A second finger, late: it was two from the start, not a drag.
            self.monitor.pointer_button(BTN_LEFT, false);
            g.kind = Kind::Pending;
            g.pair = Some((spread(&g.fingers), middle(&g.fingers)));
        }
        if g.kind == Kind::Pending {
            self.decide(g, now);
        }
        match g.kind {
            Kind::Drag if !g.fingers.is_empty() => {
                let (x, y) = g.fingers[0].1;
                self.monitor.pointer_move(x, y);
            }
            Kind::Scroll => {
                if g.fingers.len() != 2 {  // one lifted: no more scrolling from these
                    self.scroll_end(g, now, false);
                    g.kind = Kind::Done;
                } else {
                    let (x, y) = middle(&g.fingers);
                    let (lx, ly) = g.last;
                    if (x, y) != (lx, ly) {
                        g.last = (x, y);
                        g.samples.push_back((now, x - lx, y - ly));
                        while g.samples.front().is_some_and(|s| now - s.0 > VELOCITY_WINDOW) {
                            g.samples.pop_front();
                        }
                        // Natural: the page follows the fingers.
                        self.monitor.pointer_axis((lx - x) / self.scale, (ly - y) / self.scale, false);
                    }
                }
            }
            Kind::Pinch => {
                for &(f, p) in &g.fingers.clone() {
                    if find(&g.touching, f) != Some(p) {
                        self.monitor.touch_motion(f as u32, p.0, p.1);
                        g.touching.retain(|(t, _)| *t != f);
                        g.touching.push((f, p));
                    }
                }
                let gone: Vec<u8> = g.touching.iter().map(|(f, _)| *f).filter(|f| find(&g.fingers, *f).is_none()).collect();
                for f in gone {
                    self.monitor.touch_up(f as u32);
                    g.touching.retain(|(t, _)| *t != f);
                }
            }
            Kind::Swipe if g.fingers.len() >= 3 => {
                let ((x, y), (x0, y0)) = (middle(&g.fingers), g.last);
                let (dx, dy) = ((x - x0) / self.size.0, (y - y0) / self.size.1);
                if dx.abs().max(dy.abs()) >= SWIPE {
                    g.kind = Kind::Done;
                    if dy.abs() > dx.abs() {
                        self.keys(&[if dy < 0.0 { KEY_SUPER } else { KEY_ESCAPE }]);  // the overview, or out of it
                    } else {  // the workspace to the right with the fingers going left
                        self.keys(&[KEY_SUPER, if dx < 0.0 { KEY_PAGE_DOWN } else { KEY_PAGE_UP }]);
                    }
                }
            }
            _ => {}
        }
    }

    fn decide(&mut self, g: &mut Gesture, now: f64) {
        if g.most >= 3 {
            if g.fingers.len() >= 3 {
                g.kind = Kind::Swipe;
                g.last = middle(&g.fingers);
            }
            return;
        }
        if g.most == 2 {
            if g.fingers.len() != 2 {
                return;
            }
            let (spread0, (x0, y0)) = g.pair.unwrap();
            let (x, y) = middle(&g.fingers);
            let shift = (x - x0).hypot(y - y0);
            let change = (spread(&g.fingers) - spread0).abs();
            if shift.max(change) < GESTURE_START * self.size.0 {
                return;
            }
            if change > shift {
                // A pinch on a touchscreen: browsers, maps and viewers zoom the
                // picture under the fingers, as a touchpad's pinch does.
                g.kind = Kind::Pinch;
                for &(f, _) in &g.fingers.clone() {
                    let p = find(&g.start, f).unwrap();
                    self.monitor.touch_down(f as u32, p.0, p.1);
                    g.touching.push((f, p));
                }
            } else {
                // From where they began: the way that decided it scrolls too.
                g.kind = Kind::Scroll;
                g.last = (x0, y0);
                self.monitor.pointer_move(x, y);  // the scroll goes to what is under the fingers
            }
            return;
        }
        if g.moved >= TAP_SLOP && (now - g.since >= DRAG_WAIT || g.moved >= DRAG_FAST) {
            g.kind = Kind::Drag;
            let (x, y) = g.start[0].1;
            self.monitor.pointer_move(x, y);
            self.monitor.pointer_button(BTN_LEFT, true);
        } else if now - g.since >= HOLD {
            g.kind = Kind::Done;
            self.click(g.start[0].1, BTN_RIGHT);
        }
    }

    fn end(&mut self, g: &mut Gesture, now: f64, lost: bool) {
        let quick = now - g.since < TAP_TIME && !lost;
        match g.kind {
            Kind::Pending => {
                if g.most == 1 && quick && g.moved < TAP_SLOP {
                    self.click(g.start[0].1, BTN_LEFT);
                } else if g.most == 2 && quick && g.moved < TAP_MOVE {
                    self.click(g.start[0].1, BTN_RIGHT);
                }
            }
            Kind::Drag => self.monitor.pointer_button(BTN_LEFT, false),
            Kind::Scroll => self.scroll_end(g, now, !lost),
            Kind::Pinch => {
                for &(f, _) in &g.touching {
                    self.monitor.touch_up(f as u32);
                }
            }
            _ => {}
        }
    }

    fn scroll_end(&mut self, g: &Gesture, now: f64, fling: bool) {
        let samples: Vec<&(f64, f64, f64)> = g.samples.iter().filter(|s| now - s.0 <= VELOCITY_WINDOW).collect();
        if fling && samples.len() >= 2 {
            let span = (samples[samples.len() - 1].0 - samples[0].0).max(1.0 / 120.0);
            let vx = samples[1..].iter().map(|s| s.1).sum::<f64>() / span;
            let vy = samples[1..].iter().map(|s| s.2).sum::<f64>() / span;
            if vx.hypot(vy) >= FLING_START {
                self.fling = Some((vx, vy, now));
                return;
            }
        }
        self.monitor.pointer_axis(0.0, 0.0, true);  // finish
    }

    fn fling_step(&mut self, now: f64) {
        let (vx, vy, at) = self.fling.unwrap();
        let decay = (-(now - at) / FLING_TAU).exp();
        // What the speed covers over the time since, slowing down all the while.
        let (dx, dy) = (vx * FLING_TAU * (1.0 - decay), vy * FLING_TAU * (1.0 - decay));
        let (vx, vy) = (vx * decay, vy * decay);
        self.monitor.pointer_axis(-dx / self.scale, -dy / self.scale, false);
        if vx.hypot(vy) < FLING_STOP {
            self.stop_fling();
        } else {
            self.fling = Some((vx, vy, now));
        }
    }

    fn stop_fling(&mut self) {
        self.fling = None;
        self.monitor.pointer_axis(0.0, 0.0, true);  // finish
    }

    fn click(&self, (x, y): Point, button: i32) {
        self.monitor.pointer_move(x, y);
        self.monitor.pointer_button(button, true);
        self.monitor.pointer_button(button, false);
    }

    fn keys(&self, keys: &[u32]) {
        for &key in keys {
            self.monitor.keysym(key, true);
        }
        for &key in keys.iter().rev() {
            self.monitor.keysym(key, false);
        }
    }

    // -- the pen -----------------------------------------------------------------
    fn pen_report(&mut self, report: Vec<u8>) {
        if self.pen.is_none() {
            self.pen = Some((self.make_pen)());
        }
        if let Some(Some(pen)) = self.pen.as_mut() {
            pen.report(&report);
        }
        self.pen_last = (report[1] & IN_RANGE != 0).then_some(report);
    }
}

/// The same report, the pen lifted out of range.
fn lifted(report: &[u8]) -> Vec<u8> {
    let mut out = report.to_vec();
    out[1] = 0;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Injector for Recorder {
        fn touch_down(&self, slot: u32, x: f64, y: f64) { self.0.lock().unwrap().push(format!("down {slot} {x:.0} {y:.0}")) }
        fn touch_motion(&self, slot: u32, _x: f64, _y: f64) { self.0.lock().unwrap().push(format!("motion {slot}")) }
        fn touch_up(&self, slot: u32) { self.0.lock().unwrap().push(format!("up {slot}")) }
        fn pointer_move(&self, x: f64, y: f64) { self.0.lock().unwrap().push(format!("move {x:.0} {y:.0}")) }
        fn pointer_button(&self, button: i32, pressed: bool) {
            self.0.lock().unwrap().push(format!("{} {}", if button == BTN_LEFT { "left" } else { "right" }, pressed))
        }
        fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) {
            self.0.lock().unwrap().push(if finish { "finish".into() } else { format!("axis {dx:.0} {dy:.0}") })
        }
        fn keysym(&self, keysym: u32, pressed: bool) { self.0.lock().unwrap().push(format!("key {keysym:x} {pressed}")) }
    }

    struct Rig {
        input: Input,
        events: Arc<Recorder>,
        now: Arc<Mutex<f64>>,
    }

    impl Rig {
        fn new() -> Rig {
            let events = Arc::new(Recorder::default());
            let now = Arc::new(Mutex::new(0.0));
            let clock = now.clone();
            let input = Input::new(events.clone(), (1000, 1000), Box::new(|| None),
                                   Box::new(move || *clock.lock().unwrap()));
            Rig { input, events, now }
        }

        /// Fingers at (x, y) in desktop pixels of the 1000x1000 monitor, at time t.
        fn at(&mut self, t: f64, fingers: &[(u8, f64, f64)]) {
            *self.now.lock().unwrap() = t;
            let mut body = vec![6, fingers.len() as u8];
            for &(f, x, y) in fingers {
                body.push(f);
                body.extend_from_slice(&((x / 1000.0 * TOUCH_MAX) as u16).to_be_bytes());
                body.extend_from_slice(&((y / 1000.0 * TOUCH_MAX) as u16).to_be_bytes());
            }
            body.push(0);
            assert!(self.input.packet(&body));
        }

        fn events(&self) -> Vec<String> {
            std::mem::take(&mut *self.events.0.lock().unwrap())
        }
    }

    #[test]
    fn a_tap_is_a_left_click() {
        let mut rig = Rig::new();
        rig.at(0.0, &[(0, 100.0, 200.0)]);
        rig.at(0.1, &[]);
        assert_eq!(rig.events(), vec!["move 100 200", "left true", "left false"]);
    }

    #[test]
    fn a_finger_moved_drags_with_the_left_button() {
        let mut rig = Rig::new();
        rig.at(0.0, &[(0, 100.0, 100.0)]);
        rig.at(0.15, &[(0, 150.0, 100.0)]);
        rig.at(0.2, &[(0, 200.0, 100.0)]);
        rig.at(0.25, &[]);
        assert_eq!(rig.events(), vec!["move 100 100", "left true", "move 150 100", "move 200 100", "left false"]);
    }

    #[test]
    fn two_fingers_moved_together_scroll_and_a_flick_goes_on() {
        let mut rig = Rig::new();
        rig.at(0.0, &[(0, 400.0, 500.0), (1, 600.0, 500.0)]);
        rig.at(0.02, &[(0, 400.0, 460.0), (1, 600.0, 460.0)]);
        rig.at(0.04, &[(0, 400.0, 420.0), (1, 600.0, 420.0)]);
        rig.at(0.06, &[(0, 400.0, 380.0), (1, 600.0, 380.0)]);
        rig.at(0.07, &[]);
        let events = rig.events();
        assert_eq!(events[0], "move 500 460");  // where the fingers are: the scroll goes there
        assert!(events.contains(&"axis 0 40".to_owned()));  // natural: the page follows the fingers
        assert!(!events.contains(&"finish".to_owned()) && rig.input.busy());  // flicked: going on
        *rig.now.lock().unwrap() = 3.0;
        rig.input.check();
        assert_eq!(rig.events().last().unwrap(), "finish");
        assert!(!rig.input.busy());
    }

    #[test]
    fn two_fingers_moved_apart_pinch_as_touches() {
        let mut rig = Rig::new();
        rig.at(0.0, &[(0, 450.0, 500.0), (1, 550.0, 500.0)]);
        rig.at(0.05, &[(0, 400.0, 500.0), (1, 600.0, 500.0)]);
        rig.at(0.1, &[]);
        assert_eq!(rig.events(), vec!["down 0 450 500", "down 1 550 500", "motion 0", "motion 1", "up 0", "up 1"]);
    }

    #[test]
    fn held_still_is_the_right_button_and_silence_lifts_it_all() {
        let mut rig = Rig::new();
        rig.at(0.0, &[(0, 100.0, 100.0)]);
        rig.at(0.65, &[(0, 100.0, 100.0)]);
        assert_eq!(rig.events(), vec!["move 100 100", "right true", "right false"]);
        rig.at(0.7, &[(0, 300.0, 100.0)]);
        *rig.now.lock().unwrap() = 1.2;  // the app went quiet
        rig.input.check();
        assert!(!rig.input.busy());
    }

    #[test]
    fn three_fingers_swiped_up_open_the_overview() {
        let mut rig = Rig::new();
        rig.at(0.0, &[(0, 300.0, 600.0), (1, 500.0, 600.0), (2, 700.0, 600.0)]);
        rig.at(0.05, &[(0, 300.0, 450.0), (1, 500.0, 450.0), (2, 700.0, 450.0)]);
        assert_eq!(rig.events(), vec!["key ffeb true", "key ffeb false"]);
    }

    #[test]
    fn a_malformed_packet_is_refused() {
        let mut rig = Rig::new();
        assert!(!rig.input.packet(&[6, 1, 0, 0]));
        assert!(!rig.input.packet(&[6, 11]));
        assert!(!rig.input.packet(&[6, 0, 1, 0]));
    }
}
