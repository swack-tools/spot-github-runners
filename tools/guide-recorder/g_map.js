// Guide: explore the fleet on the world map.
const { open, close, BASE } = require('./rec');
const { withFleet } = require('./demo-fleet');
(async () => {
  const r = await open('map', {
    transform(p, j, req) { return p === '/api/overview' ? withFleet(j, req) : j; },
    fulfill(p, req) {
      if (p !== '/api/instances/scale-region') return null;
      const b = req.postDataJSON();
      return { body: { region: b.region, role: b.role, current_count: 0, planned_count: b.target_instances, would_launch: [`${b.region}-demo`], would_retire: [] } };
    },
  });
  const { page, click, caption, wait, moveTo, box, shot, scrollTo } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await page.click('#sidebar-toggle-btn');
  await wait(500);
  await moveTo(700, 400, 1);
  await caption('Green pins are regions with running hosts. The count is in the label', 2600);
  await shot('explore-the-fleet-map', 'world-map');
  const pin = reg => `.dc-pin[data-region="${reg}"]`;
  await caption('1. Click a pin to inspect that region');
  await click(pin('us-east1'), 1200);
  await scrollTo('#tab-map .map-layout > *:last-child', 'start');
  await page.evaluate(() => window.scrollBy({ top: -110 }));
  await caption('The inspector lists the region\'s hosts, their cost, and the SSH command', 2800);
  await shot('explore-the-fleet-map', 'inspector');
  await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'smooth' }));
  await wait(600);
  await caption('2. Click another pin. A region with no hosts shows what would run there');
  await click(pin('us-west2'), 1200);
  await scrollTo('#tab-map .map-layout > *:last-child', 'start');
  await page.evaluate(() => window.scrollBy({ top: -110 }));
  await wait(1400);
  await caption('3. Plan buttons check a change against budgets and caps. They change nothing');
  await click('#map-inspector button:has-text("Plan 1 CI/CD Runner Host Here"), button:has-text("Plan 1 CI/CD Runner Host Here")', 2800);
  await shot('explore-the-fleet-map', 'plan-only');
  await caption('', 300);
  console.log(await close(r, 'explore-the-fleet-map'));
})();
