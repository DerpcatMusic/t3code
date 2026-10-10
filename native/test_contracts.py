"""Catch removed T3 RPCs before building the native client."""
import re
import unittest
from pathlib import Path


def registered_methods(source):
    aliases = dict(re.findall(r'\b(\w+):\s*"([^"]+)"', source))
    methods = set(re.findall(r'\bRpc\.make\(\s*"([^"]+)"', source))
    for key in re.findall(r'\bRpc\.make\(\s*(?:WS_METHODS|ORCHESTRATION_V2_WS_METHODS)\.(\w+)', source):
        methods.add(aliases[key])
    return methods


class ContractTests(unittest.TestCase):
    def test_a_legacy_constant_is_not_a_registered_method(self):
        source = 'old: "projects.list", current: "assets.createUrl"; Rpc.make(WS_METHODS.current, {})'
        self.assertEqual(registered_methods(source), {"assets.createUrl"})

    def test_adapter_calls_are_registered_in_the_current_backend(self):
        root = Path(__file__).resolve().parents[1]
        source = (root / "packages/contracts/src/rpc.ts").read_text()
        source += (root / "packages/contracts/src/orchestrationV2.ts").read_text()
        methods = registered_methods(source)
        self.assertGreater(len(methods), 50, "RPC declarations changed; update the contract check")
        calls = set()
        for path in (root / "native/zeron/crates/t3/src").glob("*.rs"):
            source = path.read_text().split("#[cfg(test)]", 1)[0]
            calls.update(re.findall(r'\.(?:call|subscribe_checked|subscribe)\(\s*"([^"]+)"', source))
        self.assertGreater(len(calls), 10, "Adapter call syntax changed; update the contract check")
        self.assertFalse(calls - methods, f"Native adapter calls removed T3 RPCs: {sorted(calls - methods)}")
        http = (root / "packages/contracts/src/environmentHttp.ts").read_text()
        for route in ("/api/projects", "/api/projects/mutate", "/api/auth/session"):
            self.assertIn(f'"{route}"', http)


if __name__ == "__main__":
    unittest.main()
