"""tools/switchbench.sh's logs summed up: each cycle's numbers, a line of
DIR/cycles.jsonl each, and over the cycles a Markdown table on stdout (the
median and the worst of each; the threads and descriptors of each side at
the start, the most and at the end). A cycle runs from its path taken down
to the next one's (the last to the end), what was sent IN_FLIGHT before
it included; the video's and the sound's numbers are given apart for the
time the path was down and for the time after its return.
  python3 -I tools/switchbench-summary.py DIR"""
import bisect
import json
import os
import statistics
import sys

SOUND_LATE = 150e6     # ns: a sound packet later than this is a gap (the speakers' buffer)
FRAME_LATE = 250e6     # ns: a frame this late is shown late (counted apart)
BEFORE = 2e9           # ns before the cycle in which the last frame shown is looked for
IN_FLIGHT = 100e6      # ns: what was sent this long before a path went down was lost with it
SOUND_PERIOD_MS = 5


def events(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def rows(path):
    """A CSV of whole numbers, a tuple a line (a line cut short at the end left out)."""
    out = []
    with open(path) as f:
        for line in f:
            try:
                out.append(tuple(int(x) for x in line.strip().split(",")))
            except ValueError:
                pass
    return out


def first(evs, start, end, test):
    for e in evs:
        if start <= e["at"] < end and test(e):
            return (e["at"] - start) / 1e6
    return None


def arrivals(received):
    """{number: first arrival} and how many came twice."""
    seen, twice = {}, 0
    for row in received:
        if row[0] in seen:
            twice += 1
        else:
            seen[row[0]] = row
    return seen, twice


def longest_run(flags):
    best = run = 0
    for bad in flags:
        run = run + 1 if bad else 0
        best = max(best, run)
    return best


def side_resources(evs, start):
    res = [e for e in evs if e["ev"] == "res"]
    if not res:
        return None
    before = [e for e in res if e["at"] < start] or res[:1]
    return {k: (before[-1][k], max(e[k] for e in res), res[-1][k]) for k in ("threads", "fds")}


def main():
    out = sys.argv[1]
    actions = []
    with open(os.path.join(out, "actions.txt")) as f:
        for line in f:
            actions.append(line.split())
    downs = [(int(a[0]), a[2], a[3], int(a[4])) for a in actions if len(a) == 5 and a[1] == "down"]
    ups = {int(a[0]): int(a[4]) for a in actions if len(a) == 5 and a[1] == "up"}
    end = next(int(a[1]) for a in actions if a[0] == "end")
    unsettled = sum(1 for a in actions if a[0] == "unsettled")
    pc = events(os.path.join(out, "pc/events.jsonl"))
    tablet = events(os.path.join(out, "tablet/events.jsonl"))
    sent_video = rows(os.path.join(out, "pc/sent-video.csv"))      # frame, key, why, sent, bytes, ok
    got_video, _ = arrivals(rows(os.path.join(out, "tablet/got-video.csv")))  # frame, key, sent, arrived, decodable
    shown = sorted(r[3] for r in got_video.values() if r[4])
    flows = {}
    for flow, sender, receiver in (("audio", "pc", "tablet"), ("control", "pc", "tablet"), ("input", "tablet", "pc")):
        sent = rows(os.path.join(out, f"{sender}/sent-{flow}.csv"))   # number, tag, sent, ok
        received = rows(os.path.join(out, f"{receiver}/got-{flow}.csv"))  # number, tag, sent, arrived
        got, twice = arrivals(received)
        flows[flow] = (sent, got, twice, received)
    cycles = []
    for i, (cycle, path, how, down) in enumerate(downs):
        stop = downs[i + 1][3] if i + 1 < len(downs) else end
        on = lambda e: e.get("kind") == path  # noqa: E731
        c = {"cycle": cycle, "path": path, "how": how, "ms": round((stop - down) / 1e6)}
        for side, evs in (("pc", pc), ("tablet", tablet)):
            c[f"{side}_suspect_ms"] = first(evs, down, stop, lambda e: e["ev"] == "path" and on(e) and e["state"] == "suspect")
            c[f"{side}_failed_ms"] = first(evs, down, stop, lambda e: e["ev"] == "path" and on(e) and e["state"] == "failed")
            c[f"{side}_detached"] = sum(1 for e in evs if down <= e["at"] < stop and e["ev"] == "detached")
        moved = [e for e in pc if down <= e["at"] < stop and e["ev"] in ("moved", "up")]
        c["moved_ms"] = (moved[0]["at"] - down) / 1e6 if moved else None
        c["moved_to"] = moved[0]["path"] if moved else None
        back = [e for e in moved if e["at"] >= ups.get(cycle, stop) and e["path"] == path]
        c["back_ms"] = (back[0]["at"] - ups[cycle]) / 1e6 if back else None
        c["sessions_gone"] = sum(1 for e in pc if down <= e["at"] < stop and e["ev"] == "gone")
        c["dials"] = sum(1 for e in pc if down <= e["at"] < stop and e["ev"] == "dial")
        # The video and the sound from the path down to its return, and from
        # its return (a gap that began before it, the first's).
        up = ups.get(cycle, stop)
        k = bisect.bisect_left(shown, down)
        times = ([shown[k - 1]] if k > 0 and shown[k - 1] >= down - BEFORE else []) \
            + shown[k:bisect.bisect_left(shown, stop)] + [stop]
        pairs = list(zip(times, times[1:]))
        sound_sent, sound_got, _, _ = flows["audio"]
        for part, start, end_ in (("", down - IN_FLIGHT, up), ("back_", up, stop - IN_FLIGHT)):
            # The longest time with no frame shown (decodable): from the last
            # one before the cycle, to the first after (or the cycle's end).
            c[f"{part}video_gap_ms"] = round(max((b - a for a, b in pairs if (up if part else 0) <= a < (stop if part else up)),
                                                 default=0) / 1e6, 1)
            frames = [r for r in sent_video if start <= r[3] < end_ and r[5]]
            c[f"{part}frames"] = len(frames)
            c[f"{part}frames_unsent"] = sum(1 for r in sent_video if start <= r[3] < end_ and not r[5])
            c[f"{part}frames_lost"] = sum(1 for r in frames if r[0] not in got_video)
            c[f"{part}frames_broken"] = sum(1 for r in frames if r[0] in got_video and not got_video[r[0]][4])
            c[f"{part}frames_late"] = sum(1 for r in frames if r[0] in got_video and got_video[r[0]][4]
                                          and got_video[r[0]][3] - r[3] > FRAME_LATE)
            c[f"{part}keyframes"] = sum(1 for r in frames if r[1])
            c[f"{part}keyframes_asked"] = sum(1 for r in frames if r[2] == 2)
            # The sound: packets in order, each missing or later than SOUND_LATE a gap.
            window = [r for r in sound_sent if start <= r[2] < end_]
            bad = [r[0] not in sound_got or sound_got[r[0]][3] - r[2] > SOUND_LATE for r in window]
            c[f"{part}sound_gap_ms"] = longest_run(bad) * SOUND_PERIOD_MS
            c[f"{part}sound_lost_ms"] = sum(bad) * SOUND_PERIOD_MS
            c[f"{part}sound_opus"] = sum(1 for r in window if r[1] == 1)
        for flow in ("input", "control"):
            sent, got, twice, received = flows[flow]
            window = [r for r in sent if down - IN_FLIGHT <= r[2] < stop - IN_FLIGHT and r[3]]
            c[f"{flow}_sent"] = len(window)
            c[f"{flow}_lost"] = sum(1 for r in window if r[0] not in got)
            late = [(got[r[0]][3] - r[2]) / 1e6 for r in window if r[0] in got]
            c[f"{flow}_latest_ms"] = round(max(late), 1) if late else None
            # Handed on older than one before it (input: never).
            newest, backwards = -1, 0
            for r in received:
                if down <= r[3] < stop:
                    backwards += r[0] < newest
                    newest = max(newest, r[0])
            c[f"{flow}_backwards"] = backwards
        cycles.append(c)
    with open(os.path.join(out, "cycles.jsonl"), "w") as f:
        for c in cycles:
            f.write(json.dumps(c) + "\n")
    first_down = downs[0][3] if downs else end
    resources = {"pc": side_resources(pc, first_down), "tablet": side_resources(tablet, first_down)}
    control_twice = flows["control"][2]
    input_backwards = sum(c["input_backwards"] for c in cycles)
    summary = {"cycles": len(cycles), "unsettled": unsettled, "resources": resources, "control_twice": control_twice,
               "input_backwards": input_backwards}
    with open(os.path.join(out, "summary.json"), "w") as f:
        json.dump(summary, f)

    def number(n):
        if isinstance(n, float) and not n.is_integer():
            return f"{n:.0f}" if n >= 10 else f"{n:.1f}"
        return f"{n:g}"

    def stat(key, worst=max):
        values = [c[key] for c in cycles if c[key] is not None]
        if not values:
            return f"never ({len(cycles)} of {len(cycles)})", "-"
        never = len(cycles) - len(values)
        median = statistics.median(values)
        text = number(median)
        if never:
            text += f" (never in {never})"
        return text, number(worst(values))

    lines = [
        ("Suspect, PC / tablet (ms after down)", ("pc_suspect_ms", "tablet_suspect_ms")),
        ("Failed, PC / tablet (ms)", ("pc_failed_ms", "tablet_failed_ms")),
        ("On another path, PC (ms)", ("moved_ms",)),
        ("Back on the path, from its return (ms)", ("back_ms",)),
        ("Detached, PC / tablet", ("pc_detached", "tablet_detached")),
        ("Sessions gone", ("sessions_gone",)),
        ("Video gap (ms with no frame shown)", ("video_gap_ms",)),
        ("Frames lost / broken / shown late (>250 ms)", ("frames_lost", "frames_broken", "frames_late")),
        ("Frames not sent (no channel)", ("frames_unsent",)),
        ("Keyframes / asked for", ("keyframes", "keyframes_asked")),
        ("Sound's longest gap (ms, lost or >150 ms late)", ("sound_gap_ms",)),
        ("Sound missing in all (ms)", ("sound_lost_ms",)),
        ("Once the path is back: video gap (ms)", ("back_video_gap_ms",)),
        ("Once back: frames lost / broken / late", ("back_frames_lost", "back_frames_broken", "back_frames_late")),
        ("Once back: keyframes / asked for", ("back_keyframes", "back_keyframes_asked")),
        ("Once back: sound's longest gap / missing (ms)", ("back_sound_gap_ms", "back_sound_lost_ms")),
        ("Pen input lost (285/s, tablet to PC) / latest (ms)", ("input_lost", "input_latest_ms")),
        ("Control messages lost / latest (ms)", ("control_lost", "control_latest_ms")),
    ]
    dropped = ", ".join(sorted({c["path"] for c in cycles}))
    settled = f"; {unsettled} never settled" if unsettled else ""
    print(f"{len(cycles)} cycles ({dropped} dropped{settled}); median, worst:\n")
    print("| | median | worst |")
    print("|---|---|---|")
    for label, keys in lines:
        stats = [stat(k) for k in keys]
        print(f"| {label} | {' / '.join(s[0] for s in stats)} | {' / '.join(s[1] for s in stats)} |")
    for side in ("pc", "tablet"):
        r = resources[side]
        if r:
            print(f"| Threads, fds of the {side if side == 'tablet' else 'PC'}: start, most, end "
                  f"| {r['threads'][0]}, {r['threads'][1]}, {r['threads'][2]} threads | "
                  f"{r['fds'][0]}, {r['fds'][1]}, {r['fds'][2]} fds |")
    if control_twice:
        print(f"\nControl messages that came twice: {control_twice}")
    if input_backwards:
        print(f"\nPen input handed on older than the one before: {input_backwards}")
    moved_to = {}
    for c in cycles:
        moved_to[c["moved_to"]] = moved_to.get(c["moved_to"], 0) + 1
    print("\nOn which path after the drop: " + ", ".join(f"{k or 'none'} {v}" for k, v in sorted(moved_to.items(), key=str)))
    by_path = {}
    for c in cycles:
        by_path.setdefault(c["path"], []).append(c)
    if len(by_path) > 1:
        def median(values):
            values = [v for v in values if v is not None]
            return f"{statistics.median(values):.0f}" if values else "never"
        print("\nBy the path dropped, median: " + "; ".join(
            f"{path} ({len(cs)}): on another path {median(c['moved_ms'] for c in cs)} ms, "
            f"video gap {median(c['video_gap_ms'] for c in cs)} ms" for path, cs in sorted(by_path.items())))


if __name__ == "__main__":
    main()
