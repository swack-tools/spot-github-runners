#!/usr/bin/env python3
"""List current GCP Spot compute prices using your logged-in gcloud account.

Example: python3 spot_prices.py --min-cpu 4 --min-ram 8 --price-per-day 5 --list cheapest --results all
Prices exclude disks, GPUs, networking and taxes. Availability is not a capacity guarantee.
"""
import argparse
import csv
import datetime as dt
import json
import math
import re
import subprocess
import sys
import urllib.parse
import urllib.request
from decimal import Decimal

# Reported vCPU counts do not represent dedicated billable cores for these shapes.
SHARED_CORE_SHAPES = frozenset(('e2-micro', 'e2-small', 'e2-medium', 'f1-micro', 'g1-small'))


def positive(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError('must be a positive finite number')
    return number


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--min-cpu', type=int, required=True)
    p.add_argument('--min-ram', type=positive, required=True, help='minimum RAM in GiB')
    p.add_argument('--price-per-day', type=positive, required=True, help='maximum USD per 24 hours, compute only')
    p.add_argument('--list', choices=['cheapest', 'expensive'], default='cheapest')
    p.add_argument('--results', default='all', help='all or a positive result count')
    p.add_argument('--project')
    p.add_argument('--csv', help='also save all displayed results to this CSV path')
    a = p.parse_args()
    if a.min_cpu < 1 or (a.results != 'all' and (not a.results.isdigit() or int(a.results) < 1)):
        p.error('CPU and result count must be positive integers')
    def gcloud(*args):
        return subprocess.run(['gcloud', *args], capture_output=True, text=True, check=True, timeout=60).stdout.strip()
    project = a.project or gcloud('config', 'get', 'project')
    if not project or project == '(unset)':
        p.error('set a gcloud project or pass --project')
    token = gcloud('auth', 'print-access-token')
    def pages(url, query):
        seen = set()
        while True:
            request = urllib.request.Request(url+'?'+urllib.parse.urlencode(query), headers={'Authorization': 'Bearer '+token})
            with urllib.request.urlopen(request, timeout=60) as response:
                data = json.load(response)
            yield data
            page = data.get('nextPageToken')
            if not page:
                return
            if page in seen:
                raise RuntimeError('repeated pagination token')
            seen.add(page)
            query['pageToken'] = page
    print('Reading current official Billing Catalog prices...', file=sys.stderr, flush=True)
    now = dt.datetime.now(dt.timezone.utc)
    rates = {}
    for page in pages('https://cloudbilling.googleapis.com/v1/services/6F81-5844-456A/skus', {'currencyCode':'USD','pageSize':5000}):
        for sku in page.get('skus', []):
            cat = sku.get('category', {})
            if cat.get('usageType','').lower() not in ('spot','preemptible'):
                continue
            match = re.fullmatch(r'(?:Spot Preemptible\s+)?([A-Za-z][0-9A-Za-z]*)\s+(?:(?:AMD|Intel|Arm)\s+)?Instance\s+(Core|RAM)(?:\s+.*)?', sku.get('description',''), re.I)
            if not match:
                continue
            infos = [i for i in sku.get('pricingInfo',[]) if i.get('effectiveTime') and dt.datetime.fromisoformat(i['effectiveTime'].replace('Z','+00:00')) <= now]
            if not infos:
                continue
            expr = max(infos,key=lambda i:i['effectiveTime']).get('pricingExpression',{})
            unit = expr.get('usageUnit','')
            tiers = expr.get('tieredRates',[])
            if not (unit == 'h' or unit.endswith(('.h','-h','_h'))) or len(tiers)!=1 or Decimal(str(tiers[0].get('startUsageAmount',0)))!=0:
                continue
            price = tiers[0].get('unitPrice',{})
            if price.get('currencyCode') != 'USD':
                continue
            amount = Decimal(str(price.get('units',0)))+Decimal(str(price.get('nanos',0)))/10**9
            if amount <= 0:
                continue
            for region in sku.get('serviceRegions',[]):
                key=(region,match[1].lower(),match[2].lower())
                if key in rates and rates[key] != amount:
                    raise RuntimeError('ambiguous catalog rates for '+str(key))
                rates[key]=amount
    print('Reading machine types in all zones...',file=sys.stderr,flush=True)
    shapes = {}
    url='https://compute.googleapis.com/compute/v1/projects/'+urllib.parse.quote(project,safe='')+'/aggregated/machineTypes'
    for page in pages(url, {'maxResults':500,'returnPartialSuccess':'false','filter':f'guestCpus >= {a.min_cpu} AND memoryMb >= {math.ceil(a.min_ram*1024)}'}):
        if page.get('unreachables'):
            raise RuntimeError('incomplete machine inventory')
        for scope in page.get('items',{}).values():
            warning=scope.get('warning')
            if warning and warning.get('code') != 'NO_RESULTS_ON_PAGE':
                raise RuntimeError('inventory warning: '+str(warning))
            for m in scope.get('machineTypes',[]):
                name=m['name']; zone=m['zone'].rsplit('/',1)[-1]; region=zone.rsplit('-',1)[0]
                if name.endswith('-metal') or (name not in SHARED_CORE_SHAPES and name.startswith(('ct','tpu','a','g'))):
                    continue  # Accelerator shapes require separate device pricing.
                key=(region,name,m['guestCpus'],m['memoryMb'])
                shapes.setdefault(key,set()).add(zone)
    rows=[]; missing=[]
    for (region,name,cpu,mb),zones in shapes.items():
        if name in SHARED_CORE_SHAPES:
            missing.append(region+'/'+name)
            continue  # Dedicated CPU/RAM rates cannot price shared-core shapes.
        core=rates.get((region,name.split('-')[0],'core')); ram=rates.get((region,name.split('-')[0],'ram'))
        if core is None or ram is None:
            missing.append(region+'/'+name); continue
        hourly=core*cpu+ram*Decimal(mb)/1024; daily=hourly*24
        if daily <= Decimal(str(a.price_per_day)):
            rows.append([region,name,cpu,mb/1024, str(hourly),str(daily),','.join(sorted(zones))])
    rows.sort(key=lambda r:(Decimal(r[5]),r[0],r[1]),reverse=a.list=='expensive')
    if a.results!='all': rows=rows[:int(a.results)]
    headers=['region','machine_type','vCPU','RAM_GiB','USD_hour','USD_day','zones']
    writer=csv.writer(sys.stdout); writer.writerow(headers); writer.writerows(rows)
    if a.csv:
        with open(a.csv,'w',newline='') as output:
            writer=csv.writer(output);writer.writerow(headers);writer.writerows(rows)
    print(f'{len(rows)} matching priced offers. Compute only. Queried {now.isoformat()}.',file=sys.stderr)
    if missing:
        print(f'{len(missing)} additional shapes lack supported CPU/RAM catalog prices; not included. No capacity-history prices substituted.',file=sys.stderr)
    return 0

if __name__=='__main__':
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.SubprocessError, OSError, ValueError) as error:
        print('Error: '+str(error),file=sys.stderr)
        raise SystemExit(1)
