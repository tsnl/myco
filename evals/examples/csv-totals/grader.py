import json
import subprocess
import sys
import tempfile
from pathlib import Path

script = Path.cwd() / "summarize.py"
cases = [
    ("team,amount\nalpha,0.10\nalpha,0.20\n", {"alpha": 30}),
    ('team,amount\n"a, b",-12.25\n"a, b",12.5\nz,0.00\n', {"a, b": 25, "z": 0}),
    ("team,amount\n", {}),
    ("team,amount\nx,1.005\n", None),
    ("team,value\nx,1.00\n", None),
]
feedback = []
for index, (data, expected) in enumerate(cases):
    with tempfile.TemporaryDirectory() as directory:
        source = Path(directory) / "input.csv"
        source.write_text(data)
        try:
            result = subprocess.run([sys.executable, str(script), str(source)],
                                    capture_output=True, text=True, timeout=3)
            if expected is None:
                passed = result.returncode != 0 and bool(result.stderr.strip()) and not result.stdout.strip()
            else:
                passed = result.returncode == 0 and json.loads(result.stdout) == expected
        except (subprocess.TimeoutExpired, ValueError):
            passed = False
        if not passed:
            feedback.append(f"case {index}: expected {expected!r} for {data!r}")
print(json.dumps({"score": (len(cases) - len(feedback)) / len(cases),
                  "feedback": "\n".join(feedback) or "All CSV and exact-cent checks passed."}))
