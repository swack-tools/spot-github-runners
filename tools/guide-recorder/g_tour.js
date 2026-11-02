// Guide: a quick tour of every dashboard page.
const { open, close, BASE } = require('./rec');
const { withFleet, observability } = require('./demo-fleet');
(async () => {
  const r = await open('tour', {
    transform(p, j, req) { return p === '/api/overview' ? withFleet(j, req) : j; },
    fulfill(p, req) { return p === '/api/observability' ? { body: observability(req.url()) } : null; },
  });
  const { page, click, caption, moveTo, shot } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  r.frames.length = 0;
  await moveTo(700, 500, 1);
  await caption('The header shows the fleet at a glance: burn rate, online runners and builders, and scaling', 2600);
  await shot('tour-the-dashboard', 'overview');
  const stops = [
    ['tab-observability', 'Observability & Logs: host charts, runner density, and logs tied to workflow jobs'],
    ['tab-instances', 'VM Instances & Cost: every host, running or deleted, with what it cost'],
    ['tab-sizing', 'Machine Qualification: size runners, launch pools and Rust builders'],
    ['tab-regions', 'Regions & Latency: pick the parts of the world the fleet may use'],
    ['tab-scaling', 'Autoscaling & OOM: scaling triggers, budget caps, and out-of-memory runs'],
    ['tab-cloudinit', 'Cloud-Init & Templates: setup scripts and VM shapes'],
    ['tab-patches', 'Runner Patches: edit per-repository runner manifests through pull requests'],
    ['tab-rust', 'Rust Build Telemetry: remote build times, cache hits, and where builds came from'],
    ['tab-admins', 'Administrators: who else may sign in'],
  ];
  for (const [tab, text] of stops) {
    await caption(text);
    await click(`[data-tab="${tab}"]`, 2300);
  }
  await caption('', 300);
  console.log(await close(r, 'tour-the-dashboard'));
})();
