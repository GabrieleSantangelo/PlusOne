"""Pen stroke modelling: smooth, steady ink from noisy pen samples.

The model is the one Google's ink-stroke-modeler describes (Apache-2.0,
github.com/google/ink-stroke-modeler), which is also what Notewise draws
with. Plus One reimplements it from that description:

- slow pen movements are averaged over a short window (wobble smoothing), so
  the digitizer's jitter does not show where the pen barely moves;
- the ink tip is a mass pulled towards the pen by a spring, against drag:
  a well damped follower that turns jagged samples into round curves. It is
  stepped at a fixed minimum rate, so curves stay round between samples;
- when the pen lifts, the tip is walked the rest of the way to the pen, so a
  stroke ends exactly where the pen did;
- pressure is read off the raw samples nearest to the modelled tip.

The tip trails the pen while drawing (15 ms at the default smoothing);
predict() gives the rest of the way to the pen, plus a short extrapolation
past it, for drawing only: it is replaced on the next update.

Positions are in screen pixels (the speeds and distances below assume
them), times in seconds.
"""
import math
from dataclasses import dataclass

LEVELS = 10    # smoothing 0 (least) to 10
STILL = 0.012  # seconds without moving: the pen has stopped


@dataclass
class Params:
    # Wobble smoothing: below speed_floor (units/s) positions are averaged
    # over the last `wobble_window` seconds; above speed_ceiling they are not.
    wobble_window: float = 0.010
    speed_floor: float = 20.0
    speed_ceiling: float = 30.0
    # Spring-mass tip. mass is mass / spring constant (s^2), drag per second.
    mass: float = 2.16e-5
    drag: float = 705.6
    min_rate: float = 500.0         # tip steps per second, at least
    stop_distance: float = 0.01     # end of stroke: close enough to the pen
    end_iterations: int = 20
    pressure_samples: int = 20      # raw samples pressure is projected onto
    # Prediction past the last sample, for drawing only. Tuned on a real
    # signature (widget pixels): a straight line along the filtered
    # velocity (curving it with acceleration and jerk guessed worse), up to
    # 30 ms ahead (Notewise: 24-36 ms); it strays from the line drawn next
    # by 0.12 px RMS.
    prediction: float = 0.030       # seconds ahead, at full confidence
    process_noise: float = 1e-5
    measurement_noise: float = 1.0
    stable_after: int = 4           # samples before predicting
    time_samples: int = 20
    acceleration_weight: float = 0.0
    jerk_weight: float = 0.0
    confident_samples: int = 20
    max_estimation_distance: float = 2.5
    min_travel_speed: float = 25.0
    max_travel_speed: float = 150.0
    max_linear_deviation: float = 18.0
    baseline_linearity: float = 0.4

    @classmethod
    def smoothing(cls, level, **overrides):
        """Notewise's stabilization scale: 0 follows the pen within ~7 ms,
        10 lags ~115 ms behind it and rounds everything off."""
        level = max(0, min(LEVELS, level))
        return cls(mass=(30 + 10 * level * level) / 32400 / 100,
                   drag=(72 - 0.36 * level * level) * 10, **overrides)


class StrokeModeler:
    """Feed one stroke's samples with down(), move() and up(); each returns
    the new modelled points, (x, y, pressure)."""

    def __init__(self, params=None):
        self.params = params or Params()
        self.reset()

    def reset(self):
        self.raw = []        # (x, y, pressure, time) of this stroke
        self.window = []     # recent (x, y, time, speed) for wobble smoothing
        self.anchor = None   # last smoothed pen position, with its time
        self.pos = self.vel = None
        self.kalman = None

    # -- input -----------------------------------------------------------------
    def down(self, x, y, pressure, t):
        self.reset()
        self.raw.append((x, y, pressure, t))
        self.window.append((x, y, t, 0.0))
        self.anchor = (x, y, t)
        self.pos, self.vel = (x, y), (0.0, 0.0)
        self.kalman = _Predictor(self.params, x, y, t)
        return [(x, y, pressure)]

    def move(self, x, y, pressure, t):
        if self.anchor is None:
            return self.down(x, y, pressure, t)
        t = max(t, self.anchor[2] + 1e-4)  # time must move forward
        if (x, y) == self.raw[-1][:2]:
            # New pressure only (the S Pen sends one such report for each
            # move): time passes, the pen stays, speeds are not halved.
            # Only once it has not moved for a while is it still.
            self.raw[-1] = (x, y, pressure, t)
            if t - self.kalman.times[-1] > STILL:
                self.kalman.update(x, y, t)
            return self._follow(*self.anchor[:2], t)
        self.raw.append((x, y, pressure, t))
        self.kalman.update(x, y, t)
        return self._follow(*self._smooth(x, y, t), t)

    def up(self, x, y, pressure, t):
        """Last sample: the tip is brought to it (not smoothed)."""
        if self.anchor is None:
            return self.down(x, y, pressure, t)
        out = []
        if (x, y) != self.raw[-1][:2]:
            t = max(t, self.anchor[2] + 1e-4)
            self.raw.append((x, y, pressure, t))
            out = self._follow(x, y, t)
        else:
            self.anchor = (x, y, self.anchor[2])
        pos, vel, tail = self._settle(self.pos, self.vel, self.anchor[:2])
        self.pos, self.vel = pos, vel
        return out + [(px, py, self._pressure(px, py)) for px, py in tail]

    # -- model -----------------------------------------------------------------
    def _smooth(self, x, y, t):
        p = self.params
        last = self.window[-1]
        dt = t - last[2]
        speed = math.dist((x, y), last[:2]) / dt if dt > 0 else 0.0
        self.window.append((x, y, t, speed))
        while self.window[0][2] < t - p.wobble_window:
            self.window.pop(0)
        n = len(self.window)
        mx = sum(s[0] for s in self.window) / n
        my = sum(s[1] for s in self.window) / n
        mean_speed = sum(s[3] for s in self.window) / n
        span = p.speed_ceiling - p.speed_floor
        k = 1.0 if span <= 0 else min(1.0, max(0.0, (mean_speed - p.speed_floor) / span))
        return mx + k * (x - mx), my + k * (y - my)

    def _follow(self, x, y, t):
        """Step the tip towards the moving anchor, at least min_rate steps
        a second; the anchor moves in a straight line between samples."""
        ax, ay, at = self.anchor
        steps = max(1, math.ceil((t - at) * self.params.min_rate))
        dt = (t - at) / steps
        out = []
        for i in range(1, steps + 1):
            f = i / steps
            self.pos, self.vel = self._step(self.pos, self.vel, (ax + f * (x - ax), ay + f * (y - ay)), dt)
            out.append((*self.pos, self._pressure(*self.pos)))
        self.anchor = (x, y, t)
        return out

    def _step(self, pos, vel, anchor, dt):
        p = self.params
        ax = (anchor[0] - pos[0]) / p.mass - p.drag * vel[0]
        ay = (anchor[1] - pos[1]) / p.mass - p.drag * vel[1]
        vel = (vel[0] + dt * ax, vel[1] + dt * ay)
        return (pos[0] + dt * vel[0], pos[1] + dt * vel[1]), vel

    def _settle(self, pos, vel, anchor):
        """Walk the tip the rest of the way to a still anchor: smaller steps
        where a step would overshoot it. Returns (pos, vel, positions)."""
        p = self.params
        dt = 1.0 / p.min_rate
        out = []
        for _ in range(p.end_iterations):
            if math.dist(pos, anchor) <= p.stop_distance:
                break
            new_pos, new_vel = self._step(pos, vel, anchor, dt)
            if math.dist(new_pos, pos) < p.stop_distance:
                break
            to_anchor = (anchor[0] - pos[0], anchor[1] - pos[1])
            past = (anchor[0] - new_pos[0]) * to_anchor[0] + (anchor[1] - new_pos[1]) * to_anchor[1]
            if past < 0:  # overshot: try again with half the step
                dt /= 2
                continue
            pos, vel = new_pos, new_vel
            out.append(pos)
        if math.dist(pos, anchor) > p.stop_distance:
            out.append(anchor)  # never leave the end short of the pen
            pos = anchor
        return pos, vel, out

    def _pressure(self, x, y):
        """Pressure where the tip is, interpolated along the closest segment
        of the recent raw samples."""
        recent = self.raw[-self.params.pressure_samples:]
        if len(recent) == 1:
            return recent[0][2]
        best, value = math.inf, recent[-1][2]
        for a, b in zip(recent, recent[1:]):
            dx, dy = b[0] - a[0], b[1] - a[1]
            length = dx * dx + dy * dy
            r = 0.0 if not length else max(0.0, min(1.0, ((x - a[0]) * dx + (y - a[1]) * dy) / length))
            d = math.dist((x, y), (a[0] + r * dx, a[1] + r * dy))
            if d <= best:
                best, value = d, a[2] + r * (b[2] - a[2])
        return value

    # -- prediction ------------------------------------------------------------
    def predict(self):
        """Points from the tip to the pen, then on to where the pen should
        be by the time they are seen: drawing only."""
        if self.anchor is None or len(self.raw) < 2:
            return []
        x, y, pressure, now = self.raw[-1]
        _pos, _vel, tail = self._settle(self.pos, self.vel, (x, y))
        tail += self.kalman.ahead((x, y), now)
        return [(px, py, pressure) for px, py in tail]


class _Kalman1D:
    """Position, velocity, acceleration and jerk of one coordinate, one
    sample per time step."""

    F = ((1, 1, 0.5, 1 / 6), (0, 1, 1, 0.5), (0, 0, 1, 1), (0, 0, 0, 1))
    G = (1 / 6, 0.5, 1, 1)

    def __init__(self, x, process_noise, measurement_noise):
        self.s = [x, 0.0, 0.0, 0.0]
        # Nothing known yet: the first samples set the speed.
        self.P = [[1e3 if i == j else 0.0 for j in range(4)] for i in range(4)]
        self.P[0][0] = measurement_noise
        self.q = process_noise
        self.r = measurement_noise

    def update(self, z):
        F, G = self.F, self.G
        s = [sum(F[i][k] * self.s[k] for k in range(4)) for i in range(4)]
        FP = [[sum(F[i][k] * self.P[k][j] for k in range(4)) for j in range(4)] for i in range(4)]
        P = [[sum(FP[i][k] * F[j][k] for k in range(4)) + self.q * G[i] * G[j] for j in range(4)]
             for i in range(4)]
        innovation = P[0][0] + self.r
        gain = [P[i][0] / innovation for i in range(4)]
        residual = z - s[0]
        self.s = [s[i] + gain[i] * residual for i in range(4)]
        self.P = [[P[i][j] - gain[i] * P[0][j] for j in range(4)] for i in range(4)]


class _Predictor:
    """Kalman estimate of where the pen is going, with a confidence that
    shortens the prediction when it would be a guess."""

    def __init__(self, params, x, y, t):
        self.params = params
        self.x = _Kalman1D(x, params.process_noise, params.measurement_noise)
        self.y = _Kalman1D(y, params.process_noise, params.measurement_noise)
        self.times = [t]
        self.samples = 1
        self.last = (x, y)
        self.moved = t  # when the pen last moved

    def update(self, x, y, t):
        self.x.update(x)
        self.y.update(y)
        self.times = (self.times + [t])[-self.params.time_samples:]
        self.samples += 1
        if (x, y) != self.last:
            self.last, self.moved = (x, y), t

    def estimate(self):
        """(position, velocity, acceleration, jerk) per second, or None
        until the filter has settled."""
        p = self.params
        if self.samples < p.stable_after:
            return None
        step = (self.times[-1] - self.times[0]) / (len(self.times) - 1)
        sx, sy = self.x.s, self.y.s
        return ((sx[0], sy[0]), (sx[1] / step, sy[1] / step),
                (p.acceleration_weight * sx[2] / step ** 2, p.acceleration_weight * sy[2] / step ** 2),
                (p.jerk_weight * sx[3] / step ** 3, p.jerk_weight * sy[3] / step ** 3))

    def confidence(self, estimate):
        """0..1: enough samples, the filter agreeing with them, a pen moving
        fast enough to be worth it, along a path that is not too curved."""
        p = self.params
        pos, vel, acc, jerk = estimate
        t = p.prediction
        end = _cubic(pos, vel, acc, jerk, t)
        linear = (pos[0] + vel[0] * t, pos[1] + vel[1] * t)
        samples = min(1.0, self.samples / p.confident_samples)
        distance = 1 - min(1.0, math.dist(self.last, pos) / p.max_estimation_distance)
        speed = math.dist(end, pos) / t
        span = p.max_travel_speed - p.min_travel_speed
        travel = min(1.0, max(0.0, (speed - p.min_travel_speed) / span))
        deviation = min(1.0, math.dist(end, linear) / p.max_linear_deviation)
        linearity = 1 - (1 - p.baseline_linearity) * deviation
        return samples * distance * travel * linearity

    def ahead(self, pen, now):
        """Points past the pen's last sample, along its estimated motion;
        none once it has stopped."""
        estimate = self.estimate()
        if estimate is None or now - self.moved > STILL:
            return []
        _pos, vel, acc, jerk = estimate
        dt = 1.0 / self.params.min_rate
        steps = math.ceil(self.confidence(estimate) * self.params.prediction / dt)
        return [_cubic(pen, vel, acc, jerk, i * dt) for i in range(1, steps + 1)]


def _cubic(pos, vel, acc, jerk, t):
    return tuple(pos[k] + vel[k] * t + acc[k] * t * t / 2 + jerk[k] * t ** 3 / 6 for k in (0, 1))
