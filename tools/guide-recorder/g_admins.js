// Guide: add a dashboard administrator. The local Worker has no GitHub token to look the user
// up, so the add answer is the one the live Worker gives for a public example account.
const { open, close, BASE } = require('./rec');
const NOW = Math.floor(Date.now() / 1000);
const added = { login: 'octocat', uid: 583231, override_target_check: false, added_by: 'ci-owner', added_at: new Date(NOW * 1000).toISOString() };
let done = false;
(async () => {
  const r = await open('admins', {
    fulfill(p, req) {
      if (p === '/api/admins/add' && req.method() === 'POST') { done = true; return { body: { owners: [{ global_admin: true, login: 'ci-owner', uid: 100001 }], admins: [added], history: [], added_admins_exempt: true, max_admins: 50, admin: added } }; }
      if (p === '/api/admins' && done) return { body: { owners: [{ global_admin: true, login: 'ci-owner', uid: 100001 }], admins: [added], history: [], added_admins_exempt: true, max_admins: 50 } };
      return null;
    },
  });
  const { page, click, type, caption, wait, moveTo, shot, fakeDialog } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  await moveTo(140, 500, 1);
  await caption('1. Open Administrators. Only the owner can add or remove administrators', 1400);
  await click('[data-tab="tab-admins"]', 1200);
  await shot('add-an-administrator', 'administrators', '#tab-admins', 0);
  await caption('2. Type their GitHub username');
  await type('#adm-login', 'octocat', 110);
  await caption('3. Click Add administrator, then confirm');
  await click('#adm-add', 200);
  await fakeDialog('Add GitHub user octocat as a dashboard administrator? They will be able to sign in and change the fleet.', null, 2000);
  await wait(1500);
  await caption('They can sign in now with their own GitHub account', 2600);
  await shot('add-an-administrator', 'administrator-added', '#tab-admins', 0);
  await caption('', 300);
  console.log(await close(r, 'add-an-administrator'));
})();
