// Guide: sign in to the dashboard with a device code.
const { open, close, BASE } = require('./rec');
let deviceCode = null;
(async () => {
  const r = await open('signin', { anon: true, transform(p, j) { if (p === '/auth/device/code' && j.device_code) deviceCode = j.device_code; return j; } });
  const { page, click, caption, wait, moveTo, card, shot } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  await moveTo(720, 300, 1);
  await caption('Open the dashboard. Signed out, it shows only the sign-in page', 1800);
  await shot('sign-in', 'sign-in-page');
  await caption('1. Click Get a GitHub device code');
  await click('#device-start', 1500);
  await caption('2. The page shows a one-time code and the GitHub address to enter it', 2400);
  await shot('sign-in', 'device-code', 'section:has(#device-box)', 16);
  await card('3. On github.com/login/device, enter the code and click Authorize', 2400);
  await page.evaluate(async (code) => { await fetch('/api/test/authorize-device', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ device_code: code }) }); }, deviceCode);
  await caption('The page notices the approval and opens the dashboard');
  await page.waitForURL(BASE + '/', { timeout: 20000 });
  await page.waitForSelector('[data-tab="tab-sizing"]', { timeout: 20000 });
  await page.waitForLoadState('networkidle');
  await wait(1800);
  await shot('sign-in', 'signed-in');
  await caption('', 300);
  console.log(await close(r, 'sign-in'));
})();
