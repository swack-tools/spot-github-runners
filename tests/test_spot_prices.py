"""Pricing regression: shared-core vCPUs are not dedicated billable cores."""
import contextlib
import csv
import datetime
import io
import json
from pathlib import Path
import runpy
import subprocess
import sys
import unittest
from unittest.mock import patch


class SpotPriceTests(unittest.TestCase):
    def test_shared_core_shapes_are_reported_unsupported(self):
        module = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'tools/spot_prices.py'), run_name='price_control')
        effective = datetime.datetime.now(datetime.timezone.utc).replace(year=2020).isoformat()
        def sku(kind):
            return {'category': {'usageType': 'Spot'}, 'description': 'E2 Instance ' + kind,
                    'serviceRegions': ['us-test1'], 'pricingInfo': [{'effectiveTime': effective,
                    'pricingExpression': {'usageUnit': 'h', 'tieredRates': [{'startUsageAmount': 0,
                    'unitPrice': {'currencyCode': 'USD', 'units': '1', 'nanos': 0}}]}}]}
        billing = {'skus': [sku('Core'), sku('RAM')]}
        inventory = {'items': {'zones/us-test1-a': {'machineTypes': [
            {'name': 'e2-micro', 'guestCpus': 2, 'memoryMb': 1024, 'zone': 'us-test1-a'},
            {'name': 'e2-standard-4', 'guestCpus': 4, 'memoryMb': 16384, 'zone': 'us-test1-a'}]}}}
        responses = [io.BytesIO(json.dumps(value).encode()) for value in (billing, inventory)]
        def gcloud(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 0, stdout='synthetic-token' if 'print-access-token' in argv else 'synthetic-project', stderr='')
        output, diagnostic = io.StringIO(), io.StringIO()
        with patch.object(sys, 'argv', ['spot_prices.py', '--min-cpu', '1', '--min-ram', '1', '--price-per-day', '10000']), patch('subprocess.run', side_effect=gcloud), patch('urllib.request.urlopen', side_effect=responses), contextlib.redirect_stdout(output), contextlib.redirect_stderr(diagnostic):
            self.assertEqual(module['main'](), 0)
        rows = list(csv.DictReader(io.StringIO(output.getvalue())))
        self.assertEqual([row['machine_type'] for row in rows], ['e2-standard-4'])
        self.assertIn('1 additional shapes lack supported', diagnostic.getvalue())
        self.assertEqual(rows[0]['USD_day'], '480')


if __name__ == '__main__':
    unittest.main()
