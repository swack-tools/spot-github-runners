// Guide: launch a Rust builder, connect to it, and delete it. GCP calls are answered by the scene.
const { open, close, BASE } = require('./rec');
const { withFleet } = require('./demo-fleet');
const NOW = new Date().toISOString();
const KEY = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleExampleExampleExampleExampleExample you@your-mac';
const S = { builders: [], leftover: [] };
const NAME = 'builder-us-east1-b-demo01';
const view = () => ({
  configured: true, can_manage: true, builders: S.builders, leftover_cache_disks: S.leftover, machine_type: 'n4-highcpu-16', max_live_builders: 4,
  docs_url: 'https://swack-tools.github.io/spot-github-runners/client-workstation-setup/',
  install_command: 'curl -fsSL https://swack-tools.github.io/spot-github-runners/cargo-remote.sh | bash -s -- install --project example-ci-fleet',
});
(async () => {
  const r = await open('builder', {
    transform(p, j, req) { return p === '/api/overview' ? withFleet(j, req) : j; },
    onDialog(d) { d.accept(); },
    fulfill(p, req) {
      if (p === '/api/builders') return { body: view() };
      if (p === '/api/builders/launch') {
        S.builders = [{ name: NAME, zone: 'us-east1-b', status: 'PROVISIONING', external_ip: null, ssh_user: 'ci-owner', machine_type: 'n4-highcpu-16', created_at: NOW, launched_by_dashboard: true }];
        return { body: { launched: true, name: NAME, zone: 'us-east1-b', machine_type: 'n4-highcpu-16', cache_disk: `${NAME}-cache`, ssh_user: 'ci-owner' } };
      }
      if (p === '/api/builders/delete') { S.builders = []; S.leftover = [{ name: `${NAME}-cache`, zone: 'us-east1-b', size_gb: '375', created_at: NOW }]; return { body: { deleting: NAME } }; }
      if (p === '/api/builders/delete-cache-disk') { S.leftover = []; return { body: { deleting: `${NAME}-cache` } }; }
      return null;
    },
  });
  const { page, click, type, caption, wait, moveTo, shot, fakeDialog } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await moveTo(140, 300, 1);
  await caption('1. Open Machine Qualification', 900);
  await click('[data-tab="tab-sizing"]', 700);
  await page.click('#sidebar-toggle-btn');
  await page.click('button:has-text("Hide KPIs")');
  await page.waitForSelector('#builders-panel');
  await wait(1200);
  await page.locator('#builders-panel').evaluate(e => e.scrollIntoView({ block: 'start' }));
  await page.evaluate(() => window.scrollBy({ top: -110 }));
  await wait(900);
  await caption('2. In Your Rust builders, click Launch builder');
  await click('#builders-panel button:has-text("Launch builder")', 800);
  const dlg = 'dialog:has(textarea[aria-label="SSH public key"])';
  await caption('3. Builders use one reviewed shape. Enter the zone', 1200);
  await type(`${dlg} input[aria-label="Zone"]`, 'us-east1-b', 70);
  await caption('4. Paste your Ed25519 public key. Only the public key leaves your Mac');
  await page.locator(`${dlg} textarea`).focus();
  await page.locator(`${dlg} textarea`).fill(KEY);
  await wait(1800);
  await shot('launch-a-builder', 'launch-dialog', dlg, 0);
  await caption('5. Click Launch builder');
  await click(`${dlg} button.btn-purple`, 1400);
  await caption('The VM starts now. Setup takes 15 to 25 minutes', 2000);
  S.builders[0] = { ...S.builders[0], status: 'RUNNING', external_ip: '203.0.113.20' };
  await click('#builders-panel button:has-text("Refresh")', 900);
  await caption('When it runs, the panel shows its address and your SSH user', 2200);
  await shot('launch-a-builder', 'builder-running', '#builders-panel', 4);
  await caption('6. Click Use from my Mac for the one-line install');
  await click('#builders-panel button:has-text("Use from my Mac")', 800);
  await caption('Install cargo-remote once, then run cargo remote build in any Cargo project', 2800);
  await shot('launch-a-builder', 'use-from-my-mac', 'dialog[open]', 0);
  await click('dialog[open] button:has-text("Close")', 500);
  await caption('7. When you\'re done, click Delete');
  await moveTo(1200, 300);
  await click('#builders-panel tbody button:has-text("Delete")', 200);
  await fakeDialog(`Delete ${NAME}? Its 375 GB cache disk stays (and keeps costing about $30/month) until you delete it under "Leftover cache disks".`, null);
  await wait(800);
  await caption('The cache disk stays, so a later builder could reuse its Cargo cache', 2400);
  await shot('launch-a-builder', 'leftover-disk', '#builders-panel', 4);
  await caption('8. Click Delete disk to stop paying for it');
  await click('#builders-panel button:has-text("Delete disk")', 200);
  await fakeDialog(`Delete cache disk ${NAME}-cache? Its Cargo cache and build outputs are gone for good.`, null);
  await wait(1000);
  await caption('', 300);
  console.log(await close(r, 'launch-a-builder'));
})();
