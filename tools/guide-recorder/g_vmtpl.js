// Guide: save a VM template from a running host.
const { open, close, BASE } = require('./rec');
const { withFleet, INSTANCES } = require('./demo-fleet');
(async () => {
  const saved = [];
  const r = await open('vmtpl', {
    transform(p, j, req) {
      if (p !== '/api/overview') return j;
      withFleet(j, req);
      j.vm_templates = [...saved, ...(j.vm_templates || [])];
      return j;
    },
    fulfill(p, req) {
      if (p !== '/api/vm-templates/from-instance') return null;
      const b = req.postDataJSON();
      const src = INSTANCES.find(i => i.id === b.instance_id);
      saved.push({ id: 'tpl-small-x86-demo', name: b.template_name, role: src.role, arch: src.arch, machine_type: src.machine_type, vcpu: src.vcpu, ram_gib: src.ram_gib, cpu_platform: src.cpu_platform, benchmark_score: src.benchmark_score, boot_disk_gb: 40, boot_disk_type: 'pd-balanced', data_disk_gb: src.data_disk_gb, data_disk_type: src.data_disk_type, disk_category: src.disk_category });
      return { body: { status: 'ok' } };
    },
  });
  const { page, click, type, caption, wait, moveTo, shot, scrollTo } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await moveTo(140, 420, 1);
  await caption('1. Open Cloud-Init & Templates', 900);
  await click('[data-tab="tab-cloudinit"]', 900);
  await page.click('#sidebar-toggle-btn');
  await page.click('button:has-text("Hide KPIs")');
  await wait(500);
  await caption('2. To copy a host\'s shape, pick a running host');
  await scrollTo('#vmtpl-source-instance', 'center');
  await wait(500);
  await page.selectOption('#vmtpl-source-instance', { index: 1 });
  await wait(1300);
  await caption('3. Name the template');
  await type('#vmtpl-new-name', 'Small x86 runner', 70);
  await caption('4. Click Create VM Template from Instance');
  await click("button[onclick='createVmTemplateFromSelected()'], button[onclick=\"createVmTemplateFromSelected()\"]", 1900);
  await caption('The new template is listed, ready for a sizing profile to use', 2800);
  await shot('save-a-vm-template', 'template-created');
  await caption('', 300);
  console.log(await close(r, 'save-a-vm-template'));
})();
