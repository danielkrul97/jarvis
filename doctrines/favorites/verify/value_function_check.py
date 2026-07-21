#!/usr/bin/env python3
"""Kill-gate 2: the 0..1 value function behaves. Runs the SAME formula as
schema.sql's fav_signal_value() in sqlite over sample data, and asserts:
  - every value is within [0,1] (clamping works at both ends),
  - a known/recent/converted favorite outranks an anon/old/churned one."""
import sqlite3
import sys

# Identical to fav_signal_value() in schema.sql (linear recency decay, no exp()).
VALUE_SQL = """
    MIN(1.0, MAX(0.0,
          0.35 * (CASE WHEN is_known THEN 1.0 ELSE 0.4 END)
        + 0.35 * MAX(0.0, 1.0 - age_days / 60.0)
        + 0.30 * (CASE WHEN converted THEN 1.0 ELSE 0.0 END)
        - 0.25 * MIN(1.0, (add_count - 1) * 0.2)
    ))
"""

db = sqlite3.connect(":memory:")
db.execute(
    "CREATE TABLE s (label TEXT, is_known INT, age_days REAL, converted INT, add_count INT)"
)
rows = [
    ("A_known_recent_converted", 1, 1, 1, 1),   # expect ~0.99
    ("B_anon_old_churned", 0, 45, 0, 5),         # expect ~0.03
    ("C_clamp_low", 0, 999, 0, 20),              # raw negative → clamps to 0
    ("D_clamp_high", 1, 0, 1, 1),                # raw > ... → within [0,1], near max
]
db.executemany("INSERT INTO s VALUES (?,?,?,?,?)", rows)

res = dict(
    (label, val)
    for label, val in db.execute(f"SELECT label, {VALUE_SQL} AS v FROM s")
)

errors = []
for label, v in res.items():
    if not (0.0 <= v <= 1.0):
        errors.append(f"{label}: value {v} out of [0,1]")
if not (res["A_known_recent_converted"] > res["B_anon_old_churned"]):
    errors.append(
        f"ordering: A ({res['A_known_recent_converted']:.3f}) must outrank "
        f"B ({res['B_anon_old_churned']:.3f})"
    )
if res["C_clamp_low"] != 0.0:
    errors.append(f"C_clamp_low must clamp to 0.0, got {res['C_clamp_low']}")

if errors:
    print("KILL-GATE 2 FAILED:")
    for e in errors:
        print("  -", e)
    sys.exit(1)
print("KILL-GATE 2 PASS: value function ∈ [0,1], clamps, and ranks correctly")
for label, v in res.items():
    print(f"    {label:28s} = {v:.3f}")
