// Guide: point a workflow job at a pool.
const { open, close, BASE, OVERLAY } = require('./rec');
(async () => {
  const r = await open('workflow');
  const { page, click, caption, wait, moveTo, shot } = r;
  await page.goto(BASE + '/', { waitUntil: 'networkidle' });
  await page.click('[data-tab="tab-sizing"]');
  await page.click('#sidebar-toggle-btn');
  await page.locator('#rp-panel').evaluate(e => e.scrollIntoView({ block: 'start' }));
  await page.evaluate(() => window.scrollBy({ top: -100 }));
  await wait(600);
  await moveTo(700, 500, 1);
  const row = '#rp-body tr:first-child';
  await caption('1. In Runner pools, find the pool\'s runs-on line', 1800);
  await shot('use-a-pool-in-a-workflow', 'runs-on-in-panel', row, 6);
  await caption('2. Click Copy');
  await click(`${row} button[data-copy]`, 1200);
  const line = await page.locator(`${row} button[data-copy]`).first().getAttribute('data-copy');
  const yaml = 'name: test\non: [push, pull_request]\n\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: cargo test\n';
  await page.setContent(`<!doctype html><html><head><style>
    body{margin:0;background:#16171a;color:#e6e6e6;font:21px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace;height:100vh;display:flex;flex-direction:column}
    .bar{background:#1f2125;border-bottom:1px solid #2c2f35;padding:10px 18px;font:600 14px system-ui,sans-serif;color:#cfd3da;display:flex;gap:18px;align-items:center}
    .tab{background:#16171a;border:1px solid #2c2f35;border-bottom:none;border-radius:6px 6px 0 0;padding:6px 14px;margin-bottom:-11px}
    .wrap{flex:1;display:flex;padding:22px 0}
    .gut{color:#5c6370;text-align:right;padding:0 14px 0 22px;user-select:none;white-space:pre}
    textarea{flex:1;background:transparent;color:#e6e6e6;border:none;outline:none;resize:none;font:inherit;line-height:inherit;padding:0;caret-color:#ffd54f}
    textarea::selection{background:#3e4c6b}
  </style></head><body><div class="bar"><span>example-org/service-a</span><span class="tab">.github/workflows/test.yml</span></div>
  <div class="wrap" id="wrap"><div class="gut">${yaml.split('\n').map((_, i) => i + 1).join('\n')}</div><textarea id="ed" spellcheck="false"></textarea></div></body></html>`);
  await page.evaluate(OVERLAY);
  await page.evaluate((y) => { document.getElementById('ed').value = y; }, yaml);
  await moveTo(700, 420, 1);
  await caption('3. Open the workflow file and find the job\'s runs-on', 1600);
  const start = yaml.indexOf('ubuntu-latest');
  const b = await page.locator('#ed').boundingBox();
  await moveTo(b.x + 220, b.y + 5 * 33.6 + 16);
  await page.evaluate(([s, e]) => { const t = document.getElementById('ed'); t.focus(); t.setSelectionRange(s, e); }, [start, start + 'ubuntu-latest'.length]);
  await caption('4. Replace its value with the pool\'s labels', 900);
  await moveTo(b.x + 800, b.y + 5 * 33.6 + 90, 10);
  await page.keyboard.type(line.replace(/^runs-on:\s*/, ''), { delay: 50 });
  await wait(1200);
  await shot('use-a-pool-in-a-workflow', 'workflow-edited', '#wrap', 0);
  await caption('5. Commit and push. The job waits in the queue until the pool starts a host', 2800);
  await caption('', 300);
  console.log(await close(r, 'use-a-pool-in-a-workflow'));
})();
