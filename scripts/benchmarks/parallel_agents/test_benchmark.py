"""Small checks for the benchmark's delivery and failure boundary."""
import unittest

from benchmark import run_case


class BenchmarkContract(unittest.TestCase):
    def test_delivers_all_receipts(self):
        result = run_case(5, 1000)
        self.assertTrue(result["queueDrained"])
        self.assertEqual(result["receipts"]["usage"], 5)
        self.assertEqual(result["receipts"]["monitorCompleted"], 5)
        self.assertEqual(result["receipts"]["answerItems"], 15)

    def test_missing_callback_fails(self):
        with self.assertRaisesRegex(AssertionError, "lost callbacks"):
            run_case(5, 1000, inject="drop_last")

    def test_monitor_wakes_start_exactly_once(self):
        result = run_case(5, 1000, mode="wakes")
        self.assertEqual(result["wakeStarts"], 5)
        self.assertEqual(result["receipts"]["monitorCompleted"], 5)


if __name__ == "__main__":
    unittest.main()
