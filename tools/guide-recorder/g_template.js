// Guide: make a runner setup template.
const { open, close, BASE } = require('./rec');
(async () => {
  const r = await open('template');
  const { page, click, type, caption, wait, scrollTo, moveTo, shot } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  await moveTo(140, 400, 1);
  await caption('1. Open Cloud-Init & Templates', 900);
  await click('[data-tab="tab-cloudinit"]', 900);
  await caption('2. Click ＋ New runner setup script');
  await click('#ci-new-script', 1200);
  await page.locator('#ci-editor-textarea').scrollIntoViewIfNeeded();
  await caption('The starter script installs two packages. It runs as root while each host builds its runner image', 2600);
  await shot('make-a-runner-template', 'starter-script', '.panel:has(#ci-editor-textarea)', 4);
  await caption('3. Change the commands to install what your jobs need', 1400);
  await caption('4. Name it, then click Save As New Template & Assign');
  await type('#ci-editor-name', 'Protobuf and jq tools', 60);
  await wait(300);
  await click("button[onclick=\"saveCloudInit('save_as')\"]", 1200);
  await page.waitForSelector('#ci-pool-hint', { timeout: 10000 });
  await page.locator('#ci-pool-hint').scrollIntoViewIfNeeded();
  await caption('Saved. Runner pools can use it now', 2200);
  await shot('make-a-runner-template', 'saved-hint', '#ci-pool-hint', 10);
  await caption('', 300);
  console.log(await close(r, 'make-a-runner-template'));
})();
