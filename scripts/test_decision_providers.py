import unittest

from decision_providers import GliclassProvider, JevProvider, safe_prediction


class ProviderTests(unittest.TestCase):
    def test_gliclass_adapter_uses_cpu_pipeline_and_two_questions(self):
        calls = []
        def pipeline(text, labels, **kwargs):
            calls.append((text, labels, kwargs))
            if "CONSENSUS" in labels:
                return [[{"label": "CONSENSUS", "score": 0.8}, {"label": "UNKNOWN", "score": 0.2}]]
            return [[{"label": "YES", "score": 0.9}, {"label": "NO", "score": 0.1}]]
        provider = GliclassProvider(pipeline=pipeline, model_version="local-test")
        result = provider.predict({"text": "finality stalled", "source": "node"})
        self.assertEqual(result["label"], "CONSENSUS")
        self.assertEqual(result["need_escalation"], "YES")
        self.assertEqual(len(calls), 2)

    def test_model_failure_is_explicit_unknown_and_escalated(self):
        result = safe_prediction(lambda _: (_ for _ in ()).throw(RuntimeError("offline")), {"text": "x"})
        self.assertEqual(result["label"], "UNKNOWN")
        self.assertEqual(result["need_escalation"], "YES")
        self.assertEqual(result["error"], "RuntimeError")


if __name__ == "__main__":
    unittest.main()
