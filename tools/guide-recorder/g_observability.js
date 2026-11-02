// Guide: read host charts and search workflow-correlated logs. Telemetry comes from demo-fleet.
const { open, close, BASE } = require('./rec');
const { observability, withFleet } = require('./demo-fleet');
(async () => {
  const r = await open('observability', {
    transform(p, j, req) { return p === '/api/overview' ? withFleet(j, req) : j; },
    fulfill(p, req) { return p === '/api/observability' ? { body: observability(req.url()) } : null; },
  });
  const { page, click, type, caption, wait, moveTo, shot, scrollTo } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await moveTo(140, 200, 1);
  await caption('1. Open Observability & Logs', 900);
  await click('[data-tab="tab-observability"]', 1000);
  await page.click('#sidebar-toggle-btn');
  await wait(500);
  await scrollTo('#obs-role-filter', 'start');
  await page.evaluate(() => window.scrollBy({ top: -150 }));
  await caption('Four charts: CPU, memory, runner slots, and disk', 2200);
  await caption('2. Filter by role, organization, repository, host, or time window');
  await page.selectOption('#obs-role-filter', 'cicd_runner');
  await page.locator('#obs-role-filter').dispatchEvent('change');
  await wait(1000);
  await click('#obs-target-dd-btn', 1500);
  await page.keyboard.press('Escape');
  await click('#obs-target-dd-btn', 300);
  await wait(400);
  await page.selectOption('#obs-role-filter', 'all');
  await page.locator('#obs-role-filter').dispatchEvent('change');
  await wait(800);
  await scrollTo('#obs-grafana-grid', 'start');
  await page.evaluate(() => window.scrollBy({ top: -110 }));
  await wait(2000);
  await shot('read-observability-and-logs', 'charts');
  await caption('3. Dynamic Runner Density adds runner slots on hosts with room to spare');
  await scrollTo('#obs-density-cpu', 'center');
  await type('#obs-density-cpu', '75', 150);
  await click("button[onclick='saveDynamicDensityPolicy()'], button[onclick=\"saveDynamicDensityPolicy()\"]", 1700);
  await caption('4. Search the logs. Each line links to the workflow job that wrote it');
  await scrollTo('#obs-log-explorer', 'start');
  await page.evaluate(() => window.scrollBy({ top: -110 }));
  await wait(1000);
  await type('#obs-log-query', 'memory', 150);
  await wait(2200);
  await page.selectOption('#obs-log-severity', 'ERROR');
  await wait(2200);
  await shot('read-observability-and-logs', 'log-explorer', '#obs-log-explorer', 4);
  await page.locator('#obs-log-query').fill('');
  await page.selectOption('#obs-log-severity', { index: 0 });
  await wait(1800);
  await caption('', 300);
  console.log(await close(r, 'read-observability-and-logs'));
})();
