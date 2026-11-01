// Guide: roll a template onto a running pool.
const { open, close, BASE } = require('./rec');
const NOW = Math.floor(Date.now() / 1000);
(async () => {
  const r = await open('changetpl', { transform(p, j) {
    if (p.startsWith('/api/runner-pools') && Array.isArray(j.pools)) for (const pool of j.pools) pool.status = { ...pool.status, hosts_running: 2, runners_online: 2, queued_jobs: 1, desired_hosts: 2, last_launch_host: `oxidex-runners-pool-${pool.id}-${NOW - 900}-a1b2` };
    return j; } });
  const { page, click, caption, wait, moveTo, shot } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  await page.click('[data-tab="tab-sizing"]');
  await page.click('#sidebar-toggle-btn');
  await page.locator('#rp-panel').evaluate(e => e.scrollIntoView({ block: 'start' }));
  await page.evaluate(() => window.scrollBy({ top: -100 }));
  await wait(600);
  await moveTo(700, 500, 1);
  await caption('1. In Runner pools, click Change in the pool\'s Template column');
  await click('#rp-body tr:first-child button[data-template]', 900);
  await page.waitForSelector('#rp-tpl-dialog[open]');
  await caption('2. Pick the template', 600);
  await click('#rp-tpl-select', 300);
  await page.selectOption('#rp-tpl-select', { label: 'Protobuf and jq tools' });
  await wait(1400);
  await shot('change-a-pool-template', 'template-dialog', '#rp-tpl-dialog', 0);
  await caption('3. Click Use this template');
  await click('#rp-tpl-save', 1200);
  await caption('Idle hosts are replaced one at a time; busy hosts finish their jobs first', 3200);
  await shot('change-a-pool-template', 'pool-template-changed', '#rp-body tr:first-child', 6);
  await caption('', 300);
  console.log(await close(r, 'change-a-pool-template'));
})();
