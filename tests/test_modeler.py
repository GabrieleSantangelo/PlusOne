"""The stroke modeler: smooth ink that still ends where the pen did."""
import math

import pytest

from plusone.modeler import STILL, Params, StrokeModeler

DT = 1 / 285  # one S Pen report


def draw(model, samples, dt=DT):
    """Feed (x, y, pressure) samples one report apart; modelled points."""
    out = model.down(*samples[0], 0.0)
    for i, sample in enumerate(samples[1:-1], 1):
        out += model.move(*sample, i * dt)
    return out + model.up(*samples[-1], (len(samples) - 1) * dt)


def distance_to(point, polyline):
    best = math.inf
    for a, b in zip(polyline, polyline[1:]):
        dx, dy = b[0] - a[0], b[1] - a[1]
        t = max(0.0, min(1.0, ((point[0] - a[0]) * dx + (point[1] - a[1]) * dy) / (dx * dx + dy * dy)))
        best = min(best, math.dist(point[:2], (a[0] + t * dx, a[1] + t * dy)))
    return best


def test_straight_line_stays_straight_and_ends_at_the_pen():
    samples = [(100 + 2 * i, 50, 0.4) for i in range(60)]
    out = draw(StrokeModeler(Params.smoothing(2)), samples)
    assert all(abs(y - 50) < 1e-6 for _x, y, _p in out)
    xs = [x for x, _y, _p in out]
    assert xs == sorted(xs)                     # never runs backwards
    assert out[-1][:2] == pytest.approx((218, 50))
    assert out[0][:2] == (100, 50)


def test_slow_jitter_is_smoothed():
    # Slow movement with the digitizer's +-0.4 px zigzag.
    samples = [(100 + 0.05 * i, 50 + (0.4 if i % 2 else -0.4), 0.4) for i in range(80)]
    out = draw(StrokeModeler(Params.smoothing(2)), samples)
    middle = out[len(out) // 4:-len(out) // 4]
    assert max(abs(y - 50) for _x, y, _p in middle) < 0.1


def test_more_smoothing_lags_and_rounds_more():
    lag = [Params.smoothing(level).mass * Params.smoothing(level).drag for level in (0, 2, 5, 10)]
    assert lag == sorted(lag)
    assert lag[0] == pytest.approx(0.0067, abs=0.001) and lag[-1] == pytest.approx(0.114, abs=0.002)
    assert Params.smoothing(99) == Params.smoothing(10)
    # A right angle drawn fast (285 px/s): the default barely rounds it.
    samples = [(100 + i, 100, 0.4) for i in range(40)] + [(139, 101 + i, 0.4) for i in range(40)]
    cut = {level: min(math.dist(p[:2], (139, 100))
                      for p in draw(StrokeModeler(Params.smoothing(level)), samples)) for level in (0, 2, 8)}
    assert cut[0] < 1 and cut[2] < 2.5 and cut[8] > 8


def test_follows_the_samples_closely():
    # A circle of radius 30 px drawn in half a second.
    samples = [(100 + 30 * math.cos(i / 22), 100 + 30 * math.sin(i / 22), 0.4) for i in range(140)]
    out = draw(StrokeModeler(Params.smoothing(2)), samples)
    assert max(distance_to(p, samples) for p in out) < 0.6


def test_pressure_follows_the_pen():
    samples = [(100 + 2 * i, 50, 0.1 + 0.01 * i) for i in range(50)]
    out = draw(StrokeModeler(), samples)
    pressures = [p for _x, _y, p in out]
    assert pressures[0] == 0.1 and pressures[-1] == pytest.approx(0.59)
    assert all(b >= a - 1e-9 for a, b in zip(pressures, pressures[1:]))


def test_pressure_only_reports_do_not_slow_the_pen():
    # Every other report has new pressure only: the pen still moves 2 px
    # each 2 reports, and the prediction knows it.
    model = StrokeModeler()
    model.down(100, 50, 0.4, 0.0)
    for i in range(1, 40):
        model.move(100 + 2 * (i // 2), 50, 0.4 + 0.001 * i, i * DT)
    _pos, velocity, _acc, _jerk = model.kalman.estimate()
    assert velocity[0] == pytest.approx(1 / DT, rel=0.1) and abs(velocity[1]) < 1


def test_prediction_runs_ahead_on_the_line_then_stops_with_the_pen():
    model = StrokeModeler()
    model.down(100, 50, 0.4, 0.0)
    for i in range(1, 60):                     # 2 px a report: 570 px/s
        model.move(100 + 2 * i, 50, 0.4, i * DT)
    tail = model.predict()
    pen = 100 + 2 * 59
    assert all(abs(y - 50) < 0.5 for _x, y, _p in tail)
    assert tail[-1][0] > pen + 10              # ahead of the last sample
    assert tail[-1][0] < pen + 570 * Params().prediction + 2
    # The pen stops (pressure-only reports): the prediction goes back to it.
    t = 59 * DT
    while t < 59 * DT + 3 * STILL:
        t += DT
        model.move(pen, 50, 0.4, t)
    assert model.predict()[-1][0] < pen + 1.5


def test_single_tap_is_a_dot():
    model = StrokeModeler()
    assert model.down(10, 20, 0.5, 0.0) == [(10, 20, 0.5)]
    assert model.up(10, 20, 0.5, 0.0) == []
    assert model.predict() == []
