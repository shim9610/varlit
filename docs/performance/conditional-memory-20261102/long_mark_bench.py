#!/usr/bin/env python3
"""Additional diagnostic: same 4 GiB file, 65,536 rather than 8,192 marks.

Never pool these samples with the original workload. Both A/B sides and the
same-binary control use exactly the same increased work and unchanged budgets.
"""
from pathlib import Path
source = (Path(__file__).resolve().parents[3] / 'docs/performance/audit-followup-20261102/bench.py').read_text()
before = "['writer', 'mixed', 'random', '131072', '8192']"
after = "['writer', 'mixed', 'random', '131072', '65536']"
assert source.count(before) == 1
exec(compile(source.replace(before, after), 'extended-native-mark-driver', 'exec'))
