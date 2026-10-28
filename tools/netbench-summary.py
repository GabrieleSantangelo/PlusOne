"""tools/netbench.sh's output summed up: for each test, each number's
median over the runs and, in brackets, the worst run's (the most for
times, gaps and failures; the least for the rest), TCP beside QUIC.
  tools/netbench.sh tim 3 | python3 tools/netbench-summary.py"""
import re
import statistics
import sys

runs = {}  # (transport, test) -> {key: [values]}
transport = None
for line in sys.stdin:
    head = re.match(r"== \S+ run \d+ (\w+)", line)
    if head:
        transport = head.group(1)
        continue
    if not line.startswith("test=") or transport is None:
        continue
    fields = dict(re.findall(r"(\w+)=([^\s]+)", line))
    test = fields.pop("test")
    for key, value in fields.items():
        number = re.match(r"^-?[0-9.]+", value.rstrip("%"))
        if number:
            runs.setdefault((transport, test), {}).setdefault(key, []).append(float(number.group()))

tests = []
for _, test in runs:
    if test not in tests:
        tests.append(test)
for test in tests:
    keys = []
    for transport in ("tcp", "quic"):
        for key in runs.get((transport, test), {}):
            if key not in keys:
                keys.append(key)
    cells = []
    for key in keys:
        pair = []
        for transport in ("tcp", "quic"):
            values = runs.get((transport, test), {}).get(key)
            if not values:
                pair.append("-")
                continue
            worse_high = key.startswith("p") or key in ("gap", "failed", "dropped")
            worst = max(values) if worse_high else min(values)
            pair.append(f"{statistics.median(values):g} ({worst:g})" if len(values) > 1 else f"{values[0]:g}")
        cells.append(f"{key} {pair[0]} / {pair[1]}")
    print(f"{test:9} " + ", ".join(cells))
