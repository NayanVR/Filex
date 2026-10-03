import json
from pathlib import Path
import sys
import tempfile
import unittest
from preview_benchmark import case, percentile, stats, summarize, report, memory_summary, failure_details, failure_reason, required_case

class BenchmarkTests(unittest.TestCase):
    def test_percentiles_do_not_hide_slow_tail_or_invent_empty_timings(self):
        self.assertEqual(percentile(list(range(1, 101)), 95), 95)
        self.assertEqual(percentile([1, 2, 100], 99), 100)
        self.assertIsNone(stats([])['p50'])
        self.assertEqual(stats([None, float('nan'), 2])['n'], 1)

    def worker(self, body, timeout):
        with tempfile.TemporaryDirectory() as directory:
            directory=Path(directory)
            worker=directory/'worker.py'
            worker.write_text('import sys,json,time\nfrom pathlib import Path\n'+body)
            return case(Path(sys.executable), [worker], directory/'result.json', timeout)

    def test_timeout_preserves_checkpoint_but_never_claims_success(self):
        result=self.worker('Path(sys.argv[1]).write_text(json.dumps({"status":"ok","cycles":[1]}))\ntime.sleep(20)\n', .3)
        self.assertEqual(result['status'], 'timeout')
        self.assertEqual(result['cycles'], [1])

    def test_failed_process_overrides_success_checkpoint(self):
        result=self.worker('Path(sys.argv[1]).write_text("{\\"status\\":\\"ok\\"}")\nsys.exit(7)\n', 5)
        self.assertEqual(result['status'], 'process_failed')
        self.assertEqual(result['exit_code'], 7)

    def test_missing_output_is_not_a_pass(self):
        self.assertEqual(self.worker('pass\n', 5)['status'], 'missing_result')

    def test_decode_failures_are_not_mixed_into_success_latency(self):
        result=summarize({'samples_ms':[1,3], 'errors':[{'ms':50}]}, 'decode')
        self.assertEqual(result['latency_ms']['n'], 2)
        self.assertEqual(result['failures'], 1)

    def test_memory_retention_is_signed_and_missing_counters_stay_missing(self):
        result=memory_summary({'memory_before':{'private_bytes':100,'cpu_ms':20},
                               'memory_after_cleanup':{'private_bytes':80,'cpu_ms':35}})
        self.assertEqual(result['private_bytes_after_minus_before'],-20)
        self.assertEqual(result['cpu_ms'],15)
        self.assertIsNone(result['gdi_handle_delta'])
        self.assertIsNone(result['max_handler_working_set_bytes'])

    def test_com_apartment_failure_is_reported_as_an_invalid_required_case(self):
        error='Cannot change thread mode after it is set. (0x80010106)'
        details=failure_details({'loads':[{'ok':False,'error':error} for _ in range(820)]})
        self.assertEqual(details,[{'error':error,'count':820}])
        record={'kind':'scroll','status':'partial','result':'scroll-shell-1.json','failure_details':details}
        self.assertTrue(required_case(record))
        message=failure_reason(record)
        self.assertIn('scroll-shell-1.json',message)
        self.assertIn('820',message)
        self.assertIn('0x80010106',message)

    def test_empty_report_still_states_missing_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            report(Path(directory), [], {'video':'unavailable'})
            summary=Path(directory,'summary.md').read_text()
            self.assertIn('no Windows full-file viewer',summary)
            self.assertIn('not physical display FPS',summary)

if __name__=='__main__': unittest.main()
