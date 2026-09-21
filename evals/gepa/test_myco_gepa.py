import json
import tempfile
import unittest
from pathlib import Path

from gepa import optimize
from myco_gepa import MycoAdapter, dataset

FAKE = '''#!/usr/bin/env python3
import json, sys
from pathlib import Path
args = sys.argv[1:]
assert args[0] == "run" and "--free-only" in args
case = Path(args[1]); output = Path(args[args.index("--output") + 1]) / "attempt"
(output / "workspace").mkdir(parents=True)
prelude = Path(args[args.index("--prelude") + 1]).read_text()
reflect = json.loads((case / "case.json").read_text())["name"] == "prelude-reflection"
if reflect:
    (output / "workspace/prelude.md").write_text("Verify the resulting artifact before finishing.")
score = float(reflect or "Verify" in prelude)
(output / "events.jsonl").write_text('{"event":"tool_finished","is_error":false}\\n')
(output / "result.json").write_text(json.dumps({"status":"completed","score":score,
    "feedback":"Verify the artifact.","agent":{"answer":"Finished.",
    "metrics":{"requests":1,"input_tokens":100,"output_tokens":10}}}))
print(json.dumps({"groups":[{"estimated_cost_usd":0.0}]}))
'''


class MycoGepaTests(unittest.TestCase):
    def test_real_gepa_loop_uses_myco_adapter_reflection_and_keeps_test_split_held_out(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "fake-myco-eval"
            binary.write_text(FAKE)
            binary.chmod(0o755)
            config = root / "config.toml"
            config.write_text("no credentials used")
            cases = root / "cases"
            for split in ["train", "validation", "test"]:
                case = cases / split
                case.mkdir(parents=True)
                (case / "case.json").write_text(json.dumps({"name":split, "split":split, "source":None}))
                (case / "context.json").write_text(json.dumps([
                    {"UserMessage":{"content":[{"Text":{"text":f"Task {split}"}}]}}]))
            splits = dataset(cases)
            adapter = MycoAdapter(binary, config, "free", root / "runs", max_reflections=2)
            result = optimize(seed_candidate={"prelude":"baseline"}, trainset=splits["train"],
                              valset=splits["validation"], adapter=adapter, max_metric_calls=6,
                              reflection_minibatch_size=1, seed=0, raise_on_exception=True)
            self.assertIn("Verify", result.best_candidate["prelude"])
            self.assertEqual(adapter.reflections, 1)
            state = adapter.get_adapter_state()
            adapter.reflections = 0
            adapter.set_adapter_state(state)
            self.assertEqual(adapter.reflections, 1)
            for prompt in (root / "runs/proposals").rglob("context.json"):
                self.assertNotIn("Task test", prompt.read_text())
                self.assertNotIn("Task validation", prompt.read_text())

    def test_related_session_cuts_cannot_cross_splits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for split in ["train", "validation"]:
                case = root / split
                case.mkdir()
                (case / "case.json").write_text(json.dumps({"split":split, "source":{"session_id":"same"}}))
            with self.assertRaisesRegex(ValueError, "multiple splits"):
                dataset(root)


if __name__ == "__main__":
    unittest.main()
