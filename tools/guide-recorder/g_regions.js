// Guide: choose the regions the fleet may use.
const { open, close, BASE } = require('./rec');
const { withFleet, REGIONS } = require('./demo-fleet');
const disabled = new Set();
(async () => {
  const r = await open('regions', {
    transform(p, j, req) { return p === '/api/overview' ? withFleet(j, req, disabled) : j; },
    fulfill(p, req) {
      if (p !== '/api/regions' || req.method() !== 'POST') return null;
      const b = req.postDataJSON();
      if (b.deselect_matching) REGIONS.filter(x => x.latency_ms > b.deselect_matching.min_latency_ms_to_disable).forEach(x => disabled.add(x.region));
      for (const [reg, on] of Object.entries(b.regions_enabled || {})) { if (on) disabled.delete(reg); else disabled.add(reg); }
      return { body: { status: 'ok' } };
    },
  });
  const { page, click, caption, wait, moveTo, shot, scrollTo } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await moveTo(140, 340, 1);
  await caption('1. Open Regions & Latency', 900);
  await click('[data-tab="tab-regions"]', 800);
  await page.click('#sidebar-toggle-btn');
  await wait(500);
  await caption('Every region with its latency, machine families, CPUs, and disk types', 2400);
  await shot('choose-regions', 'regions-table');
  await caption('2. Show one part of the world');
  await page.selectOption('#reg-continent-filter', { label: 'Europe' }).catch(() => page.selectOption('#reg-continent-filter', { index: 2 }));
  await page.locator('#reg-continent-filter').dispatchEvent('change');
  await wait(1900);
  await caption('3. Or only the regions with a machine family you need');
  await page.selectOption('#reg-continent-filter', { index: 0 });
  await page.locator('#reg-continent-filter').dispatchEvent('change');
  await page.selectOption('#reg-family-filter', { index: 1 });
  await page.locator('#reg-family-filter').dispatchEvent('change');
  await wait(1900);
  await page.selectOption('#reg-family-filter', { index: 0 });
  await page.locator('#reg-family-filter').dispatchEvent('change');
  await caption('4. Set the highest latency you accept');
  const s = await page.locator('#reg-max-latency').boundingBox();
  await moveTo(s.x + s.width * 0.75, s.y + s.height / 2);
  await page.locator('#reg-max-latency').fill('60');
  await page.locator('#reg-max-latency').dispatchEvent('input');
  await moveTo(s.x + s.width * 0.25, s.y + s.height / 2);
  await wait(1500);
  await shot('choose-regions', 'regions-low-latency');
  await caption('5. Click Deselect Regions Above Max Latency. The matrix stops offering them');
  await click("button[onclick='deselectHighLatencyRegions()'], button[onclick=\"deselectHighLatencyRegions()\"]", 1700);
  await page.locator('#reg-max-latency').fill('220');
  await page.locator('#reg-max-latency').dispatchEvent('input');
  await wait(1200);
  await caption('6. Click Enable All Shown to bring every region back');
  await click("button[onclick='setAllFilteredRegions(true)'], button[onclick=\"setAllFilteredRegions(true)\"]", 1700);
  await caption('', 300);
  console.log(await close(r, 'choose-regions'));
})();
