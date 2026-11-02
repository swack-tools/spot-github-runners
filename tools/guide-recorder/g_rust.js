// Guide: read Rust build telemetry and record a build by hand.
const { open, close, BASE } = require('./rec');
const { withFleet } = require('./demo-fleet');
(async () => {
  const r = await open('rust', { transform(p, j, req) { return p === '/api/overview' ? withFleet(j, req) : j; } });
  const { page, click, type, caption, wait, moveTo, shot, scrollTo } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  const seed = [
    { status: 'DELIVERED', project_name: 'service-a', commit_sha: '3f2a91c', compile_duration_seconds: 214, delivery_duration_seconds: 9, instance_name: 'builder-us-east1-b-demo01', zone: 'us-east1-b', machine_type: 'n4-highcpu-16', cpu_platform: 'Intel Emerald Rapids', vcpu: 16, origin_ip: '203.0.113.10', cache_hits: 182, total_crates: 214, peak_ram_gib: 21.4, cpu_time_seconds: 2860 },
    { status: 'DELIVERED', project_name: 'service-a', commit_sha: '9c1d7e4', compile_duration_seconds: 48, delivery_duration_seconds: 6, instance_name: 'builder-us-east1-b-demo01', zone: 'us-east1-b', machine_type: 'n4-highcpu-16', cpu_platform: 'Intel Emerald Rapids', vcpu: 16, origin_ip: '203.0.113.10', cache_hits: 211, total_crates: 214, peak_ram_gib: 12.8, cpu_time_seconds: 410 },
    { status: 'OOM_KILLED', project_name: 'app-one', commit_sha: '77b0e12', compile_duration_seconds: 392, delivery_duration_seconds: 0, instance_name: 'builder-us-east1-b-demo01', zone: 'us-east1-b', machine_type: 'n4-highcpu-16', cpu_platform: 'Intel Emerald Rapids', vcpu: 16, origin_ip: '198.51.100.7', cache_hits: 40, total_crates: 388, peak_ram_gib: 27.9, cpu_time_seconds: 5120 },
  ];
  for (const b of seed) {
    await page.evaluate(async b => { await fetch('/api/rust-builds', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(b) }); }, b);
  }
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await moveTo(140, 560, 1);
  await caption('1. Open Rust Build Telemetry', 900);
  await click('[data-tab="tab-rust"]', 900);
  await page.click('#sidebar-toggle-btn');
  await wait(500);
  await caption('Totals, cache hit rate, peak RAM, and every remote build', 2600);
  await shot('track-rust-builds', 'builds');
  await caption('2. Filter by project, status, address, or network');
  await type('#rust-search-input', 'app-one', 150);
  await wait(1800);
  await page.locator('#rust-search-input').fill('');
  await page.locator('#rust-search-input').dispatchEvent('input');
  await wait(400);
  await caption('3. Builds report themselves. To add one by hand, fill in the form');
  await scrollTo('#rb-new-project', 'center');
  await type('#rb-new-project', 'service-b', 90);
  await type('#rb-new-compile', '131', 120);
  await type('#rb-new-delivery', '7', 120);
  await type('#rb-new-instance', 'builder-us-east1-b-demo01', 12);
  await type('#rb-new-hits', '96', 120);
  await type('#rb-new-total', '120', 120);
  await caption('4. Click Record Rust Build');
  await click("button[onclick='submitNewRustBuild()'], button[onclick=\"submitNewRustBuild()\"]", 1800);
  await scrollTo('#rust-builds-table-body', 'start');
  await page.evaluate(() => window.scrollBy({ top: -180 }));
  await caption('The build joins the table and the totals', 2600);
  await shot('track-rust-builds', 'build-recorded');
  await caption('', 300);
  console.log(await close(r, 'track-rust-builds'));
})();
