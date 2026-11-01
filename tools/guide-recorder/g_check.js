// Guide: check a pool: busy, spun down, and a launch error.
const { open, close, BASE } = require('./rec');
const NOW = Math.floor(Date.now() / 1000);
let phase = 'busy';
(async () => {
  const r = await open('check', { transform(p, j) {
    if (p.startsWith('/api/runner-pools') && Array.isArray(j.pools)) j.pools.forEach((pool) => {
      if (phase === 'busy') pool.status = { ...pool.status, hosts_running: 2, runners_online: 2, queued_jobs: 1, desired_hosts: 2, last_launch_host: `oxidex-runners-pool-${pool.id}-${NOW - 1200}-e5f6` };
      else if (phase === 'down') pool.status = { ...pool.status, hosts_running: 0, runners_online: 0, queued_jobs: 0, desired_hosts: 0, last_scale_down_at: NOW - 420 };
      else pool.status = { ...pool.status, hosts_running: 0, runners_online: 0, queued_jobs: 1, desired_hosts: 1, last_error: `VM create failed in ${pool.zone}: ZONE_RESOURCE_POOL_EXHAUSTED`, last_error_at: NOW - 60 };
    });
    return j; } });
  const { page, click, caption, wait, moveTo, card, box, shot } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  await page.click('[data-tab="tab-sizing"]');
  await page.click('#sidebar-toggle-btn');
  await page.locator('#rp-panel').evaluate(e => e.scrollIntoView({ block: 'start' }));
  await page.evaluate(() => window.scrollBy({ top: -100 }));
  await wait(600);
  await moveTo(700, 300, 1);
  const row = '#rp-body tr:first-child';
  const cell = async n => { const b = await box(`${row} td:nth-child(${n})`); await moveTo(b.x + b.width / 2, b.y + b.height / 2); };
  await caption('While jobs run: hosts up, runners online, queued jobs, and the host count it\'s moving to');
  await cell(7); await wait(2600);
  await shot('check-a-pool', 'status-busy', row, 6);
  await caption('Cost: what the running hosts cost now, and the most the pool can cost');
  await cell(8); await wait(2200);
  await card('Later, after the jobs finish', 1600);
  phase = 'down';
  await caption('Click Refresh. The panel also refreshes every minute');
  await click('#rp-pools-refresh', 1400);
  await caption('Hosts idle for 10 minutes were deleted: spun down, no hosts running');
  await cell(7); await wait(2600);
  await caption('An idle pool costs $0/hr. The next queued job starts a host again');
  await cell(8); await wait(2600);
  await shot('check-a-pool', 'status-spun-down', row, 6);
  phase = 'error';
  await page.evaluate(() => document.getElementById('rp-pools-refresh').click());
  await wait(1200);
  await shot('check-a-pool', 'status-error', row, 6);
  await caption('', 300);
  console.log(await close(r, 'check-a-pool'));
})();
