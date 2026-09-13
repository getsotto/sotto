"""One invariant across every script that scrapes something over the network.

Three scripts each decide what a network failure looks like, and each has to answer the same way:
a request that dies in transit is "could not tell", never "here is a verdict". They cannot share
an import, because each is a standalone executable meant to run from a checkout without a package
on the path, so the definition is copied three times.

Copies drift. `scripts/check-webhook-versions` carried `except OSError` alone for long enough to
ship, which meant a truncated Stripe response exited 1 and was published as a live billing
endpoint drifting. Naming the tuple in all three does not stop that happening again; this does.
"""

import http.client
import importlib.machinery
import importlib.util
import pathlib
import sys
import unittest
import urllib.error

ROOT = pathlib.Path(__file__).resolve().parents[2]

#: Every script that reaches over a network and must survive the network misbehaving.
SCRIPTS = {
    "status_probe": "scripts/status-probe",
    "check_webhook_versions": "scripts/check-webhook-versions",
    "check_deletion_metrics": "scripts/check-deletion-metrics",
}


def load(name, relative):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / relative))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(name, loader))
    # Registered before executing, because a dataclass in the module resolves its annotations
    # through sys.modules and a loader-only import leaves no entry there.
    sys.modules[name] = module
    loader.exec_module(module)
    return module


MODULES = {name: load(name, path) for name, path in SCRIPTS.items()}


class TransportFailures(unittest.TestCase):
    def test_every_networked_script_names_the_same_tuple(self):
        # The mechanism. Narrow one copy and this fails, naming which, rather than the narrowing
        # being found the next time a connection drops in production.
        tuples = {name: getattr(module, "TRANSPORT_FAILURES", None) for name, module in MODULES.items()}
        missing = sorted(name for name, value in tuples.items() if value is None)
        self.assertEqual(missing, [], "these scripts scrape but define no TRANSPORT_FAILURES")
        self.assertEqual(
            len(set(tuples.values())),
            1,
            f"the copies have drifted apart: {tuples}",
        )

    def test_each_copy_covers_both_halves_of_what_a_network_raises(self):
        # Asserting they agree is not enough on its own: three identical wrong answers agree.
        # IncompleteRead is the case that matters, because it is the only common transport
        # failure that is an HTTPException and not an OSError, so a tuple of `(OSError,)` catches
        # every other example anyone reaches for while letting this one escape as a traceback.
        for name, module in MODULES.items():
            with self.subTest(script=name):
                self.assertIsInstance(urllib.error.URLError("x"), module.TRANSPORT_FAILURES)
                self.assertIsInstance(http.client.IncompleteRead(b""), module.TRANSPORT_FAILURES)

    def test_the_case_that_makes_this_necessary_is_what_it_looks_like(self):
        # Pinned so the reasoning above stays checkable rather than remembered.
        self.assertNotIsInstance(http.client.IncompleteRead(b""), OSError)
        self.assertIsInstance(http.client.IncompleteRead(b""), http.client.HTTPException)
        self.assertIsInstance(urllib.error.URLError("x"), OSError)


if __name__ == "__main__":
    unittest.main()
