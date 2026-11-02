// Guide: change a runner patch and open a pull request. The manifest and rollout come from the scene,
// because the recorder's masked names don't exist in a local Worker's config repository.
const { open, close, BASE } = require('./rec');
const { withFleet } = require('./demo-fleet');
const MANIFEST = {
  schema: 'spot-runner-patch-v1',
  repository: 'example-user/app-two',
  hardware: { arch: 'x86_64', extra_labels: ['app-two-ci'], isa: ['avx2'], min_ram_gib: 32, min_vcpu: 16 },
  container: { apt_packages: ['build-essential', 'pkg-config', 'libssl-dev'], dockerfile_run: '', files: [], mounts: [], sysctl: {} },
  host: { apt_packages: [], boot_mkdirs: [], boot_script: '', files: [], sysctl: {} },
};
const CONTENT = JSON.stringify(MANIFEST, null, 2);
const SHA = '3f2a91c7d4e5f60718293a4b5c6d7e8f90a1b2c3';
const DIGEST = '9e1c7a4f2b3d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7';
(async () => {
  const r = await open('patches', {
    transform(p, j, req) { return p === '/api/overview' ? withFleet(j, req) : j; },
    fulfill(p, req) {
      if (p === '/api/patches/manifest') return { body: { repository: 'example-user/app-two', path: 'repos/example-user/app-two.yaml', base_sha: SHA, blob_sha: SHA, content: CONTENT, exists: true, errors: [], effective: MANIFEST, patch_sha256: DIGEST } };
      if (p === '/api/patches/validate') return { body: { valid: true, errors: [], effective: JSON.parse(req.postDataJSON().content), patch_sha256: DIGEST } };
      if (p === '/api/patches/preview') return { body: { source: { draft: true }, hot_patch_allowed: false, patch_sha256: DIGEST, hosts: [
        { host: 'runners-pool-a1b2c3-demo01', name: 'runners-pool-a1b2c3-demo01', zone: 'us-central1-a', current_patch: '4b7d10e2', current_patch_sha256: '4b7d10e2', rollout: 'recreate_containers', classification: 'recreate_containers', detail: 'The runner image changes, so idle containers restart on it', reason: 'The runner image changes, so idle containers restart on it' },
        { host: 'runners-pool-a1b2c3-demo02', name: 'runners-pool-a1b2c3-demo02', zone: 'us-central1-b', current_patch: '4b7d10e2', current_patch_sha256: '4b7d10e2', rollout: 'recreate_containers', classification: 'recreate_containers', detail: 'The runner image changes, so idle containers restart on it', reason: 'The runner image changes, so idle containers restart on it' },
      ] } };
      if (p === '/api/patches/save') return { body: { branch: 'spot-dashboard/app-two-9e1c7a4f', pull_request: { number: 42, url: 'https://github.com/example-org/fleet-config/pull/42' } } };
      return null;
    },
  });
  const { page, click, type, caption, wait, moveTo, shot } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await moveTo(140, 460, 1);
  await caption('1. Open Runner Patches and click Load manifests', 900);
  await click('[data-tab="tab-patches"]', 700);
  await page.click('#sidebar-toggle-btn');
  await page.click('button:has-text("Hide KPIs")');
  await wait(400);
  await click('#rp-refresh', 1600);
  await caption('2. Pick the organization defaults or one repository', 800);
  await click('#tab-patches button:has-text("app-two"), #tab-patches a:has-text("app-two"), #tab-patches li:has-text("app-two")', 1600);
  await shot('edit-a-runner-patch', 'manifest');
  await caption('3. Edit the manifest. Here: one more package in the runner image');
  await page.locator('#rp-editor').click();
  await page.locator('#rp-editor').fill(CONTENT.replace('"libssl-dev"', '"libssl-dev",\n      "protobuf-compiler"'));
  await page.locator('#rp-editor').dispatchEvent('input');
  await wait(1800);
  await caption('4. Click Validate. It checks the schema and the host size');
  await click('#rp-validate', 1900);
  await shot('edit-a-runner-patch', 'validated');
  await caption('5. Click Preview rollout impact to see what each host would do');
  await click('#rp-preview', 800);
  await page.locator('#rp-hosts').evaluate(e => e.scrollIntoView({ behavior: 'smooth', block: 'center' }));
  await wait(2400);
  await shot('edit-a-runner-patch', 'preview');
  await caption('6. Name the change and click Save as pull request');
  await page.locator('#rp-message').evaluate(e => e.scrollIntoView({ behavior: 'smooth', block: 'center' }));
  await wait(500);
  await type('#rp-message', 'Add protobuf-compiler to app-two runners', 30);
  await click('#rp-save', 2000);
  await caption('It opens a pull request. The change rolls out after you merge it', 2800);
  await shot('edit-a-runner-patch', 'pr-opened');
  await caption('', 300);
  console.log(await close(r, 'edit-a-runner-patch'));
})();
