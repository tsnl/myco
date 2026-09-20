import json
import subprocess
import sys
import tempfile
from pathlib import Path

cases = [
    ('{"id":"x","sequence":2,"value":"old"}\n{"id":"x","sequence":1}\n{"id":"x","sequence":2,"value":"new"}\n', {"x": {"id":"x","sequence":2,"value":"new"}}),
    ('\n{"id":"雪","sequence":-1,"extra":[1,2]}\n', {"雪": {"id":"雪","sequence":-1,"extra":[1,2]}}),
    ("\n", {}),
    ('{"id":"x","sequence":1}\nnot json\n', 2),
    ('\n{"id":"x","sequence":true}\n', 2),
    ('{"id":3,"sequence":1}\n', 1),
]
feedback = []
for index, (data, expected) in enumerate(cases):
    with tempfile.TemporaryDirectory() as directory:
        source = Path(directory) / "events.jsonl"
        source.write_text(data)
        try:
            result = subprocess.run([sys.executable, str(Path.cwd() / "index_events.py"), str(source)],
                                    capture_output=True, text=True, timeout=3)
            if isinstance(expected, int):
                passed = result.returncode != 0 and str(expected) in result.stderr and not result.stdout.strip()
            else:
                passed = result.returncode == 0 and json.loads(result.stdout) == expected
        except (subprocess.TimeoutExpired, ValueError):
            passed = False
        if not passed:
            feedback.append(f"case {index}: expected {expected!r} for {data!r}")
print(json.dumps({"score": (len(cases) - len(feedback)) / len(cases),
                  "feedback": "\n".join(feedback) or "All event ordering and input checks passed."}))
