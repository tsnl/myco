import unittest

from release import CHECKS, bump_manifest, checks_passed

MANIFEST = '''[workspace.package]
version = "0.3.0"

[workspace.dependencies]
myco-model = { path = "crates/myco-model", version = "=0.3.0" }
myco-agent = { path = "crates/myco-agent", version = "=0.3.0" }
external = "=0.3.0"
'''


def check(name, identifier=1, status="completed", conclusion="success"):
    return dict(name=name, id=identifier, status=status, conclusion=conclusion, app={"slug": "github-actions"})


class ReleaseTests(unittest.TestCase):
    def test_bumps_workspace_and_internal_pins_together(self):
        for bump, expected in [("patch", "0.3.1"), ("minor", "0.4.0"), ("major", "1.0.0")]:
            updated, old, new = bump_manifest(MANIFEST, bump)
            self.assertEqual((old, new), ("0.3.0", expected))
            self.assertEqual(updated.count(f'version = "={expected}"'), 2)
            self.assertIn(f'version = "{expected}"', updated)
            self.assertIn('external = "=0.3.0"', updated)

    def test_rejects_an_internal_dependency_outside_the_release(self):
        with self.assertRaises(ValueError):
            bump_manifest(MANIFEST.replace('version = "=0.3.0"', 'version = "=0.2.0"', 1), "patch")

    def test_pending_or_missing_checks_cannot_authorize_a_release(self):
        successful = [check(name) for name in CHECKS]
        self.assertTrue(checks_passed(successful))
        self.assertFalse(checks_passed(successful[:-1]))
        self.assertFalse(checks_passed(successful + [check("Test", 2, "in_progress", None)]))

    def test_failed_skipped_or_cancelled_checks_stop_the_release(self):
        for conclusion in ["failure", "skipped", "cancelled", "timed_out", "neutral"]:
            with self.assertRaises(ValueError):
                checks_passed([check("Test", conclusion=conclusion)])

    def test_new_successful_rerun_supersedes_old_failure(self):
        latest = [check(name, 2) for name in CHECKS]
        self.assertTrue(checks_passed([check("Test", conclusion="failure")] + latest))

    def test_other_apps_cannot_supply_required_ci(self):
        checks = [check(name) for name in CHECKS]
        for run in checks:
            run["app"]["slug"] = "another-app"
        self.assertFalse(checks_passed(checks))


if __name__ == "__main__":
    unittest.main()
