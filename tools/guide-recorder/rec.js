// Recording harness: a signed-in, recording browser context against the local Worker, with every
// response rewritten to synthetic names, a visible cursor, and captions.
const { chromium } = require('playwright');
const fs = require('fs');
const path = require('path');

const os = require('os');
const BASE = process.env.LOCAL_WORKER || 'http://127.0.0.1:8787';
// Docs images go into the checkout; frames and frame lists stay in a scratch directory.
const IMAGES = path.resolve(__dirname, '../../docs/images');
const WORK = process.env.RECORD_DIR || path.join(os.tmpdir(), 'guide-recorder');
const W = 1440, H = 900;

const MASKS = [
  ['swack-tools/spot-github-runners', 'example-org/fleet-config'],
  ['swack-tools/spot-runners-config', 'example-org/fleet-config'],
  ['swack-tools/bt-to-all-devices', 'example-org/service-b'],
  ['swack-tools/oxidex', 'example-org/service-a'],
  ['xbmc4lyfe/newznab-tmux', 'example-user/app-one'],
  ['xbmc4lyfe/rustnzb', 'example-user/app-two'],
  ['xbmc4lyfe/nntp-proxy', 'example-user/app-three'],
  ['spot.swacktech.com', 'dashboard.example.com'],
  ['swacktech.com', 'example.com'],
  ['homelab-424523', 'example-ci-fleet'],
  ['swack-tools', 'example-org'],
  ['xbmc4lyfe', 'example-user'],
  ['swackhamer', 'ci-owner'],
  ['thedancingdeveloper', 'ci-admin'],
  ['619624', '100001'],
];
function mask(t) {
  for (const [a, b] of MASKS) t = t.split(a).join(b);
  return t;
}

// Cursor dot, click ripple and caption bar, added to every page.
const OVERLAY = `
(() => {
  const add = () => {
    if (document.getElementById('__cur')) return;
    const st = document.createElement('style');
    st.textContent = '#__cur{position:fixed;z-index:2147483647;width:18px;height:18px;margin:-9px 0 0 -9px;border-radius:50%;background:rgba(255,213,79,.85);border:2px solid #111;pointer-events:none;left:-40px;top:-40px;transition:transform .12s}' +
      '#__cur.down{transform:scale(.7)}' +
      '.__rip{position:fixed;z-index:2147483646;width:44px;height:44px;margin:-22px 0 0 -22px;border-radius:50%;border:3px solid rgba(255,213,79,.9);pointer-events:none;animation:__r .5s ease-out forwards}' +
      '@keyframes __r{from{transform:scale(.3);opacity:1}to{transform:scale(1.4);opacity:0}}' +
      '#__cap{position:fixed;z-index:2147483645;left:50%;bottom:22px;transform:translateX(-50%);max-width:900px;background:rgba(12,12,16,.92);color:#fff;font:600 19px/1.35 system-ui,-apple-system,Segoe UI,sans-serif;padding:11px 20px;border-radius:10px;box-shadow:0 6px 24px rgba(0,0,0,.45);border:1px solid rgba(255,255,255,.18);pointer-events:none;opacity:0;transition:opacity .25s;text-align:center}' +
      '#__cap.on{opacity:1}' +
      '#__card{position:fixed;inset:0;z-index:2147483644;display:none;align-items:center;justify-content:center;background:rgba(10,10,14,.94);color:#fff;font:700 34px/1.3 system-ui,-apple-system,Segoe UI,sans-serif;text-align:center}' +
      '#__card.on{display:flex}' +
      'body{padding-bottom:62vh !important}';
    document.documentElement.appendChild(st);
    const c = document.createElement('div'); c.id = '__cur'; document.documentElement.appendChild(c);
    const cap = document.createElement('div'); cap.id = '__cap'; document.documentElement.appendChild(cap);
    const card = document.createElement('div'); card.id = '__card'; document.documentElement.appendChild(card);
    document.addEventListener('mousemove', e => { c.style.left = e.clientX + 'px'; c.style.top = e.clientY + 'px'; }, true);
    document.addEventListener('mousedown', e => { c.classList.add('down'); const r = document.createElement('div'); r.className = '__rip'; r.style.left = e.clientX + 'px'; r.style.top = e.clientY + 'px'; document.documentElement.appendChild(r); setTimeout(() => r.remove(), 600); }, true);
    document.addEventListener('mouseup', () => c.classList.remove('down'), true);
  };
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', add); else add();
})();`;

async function storage(browser) {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto(BASE + '/api/health');
  const ok = await page.evaluate(async () => {
    const c = await (await fetch('/auth/device/code', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' })).json();
    await fetch('/api/test/authorize-device', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ device_code: c.device_code }) });
    const r = await fetch('/auth/device/token', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ device_handle: c.device_handle }) });
    return (await r.json()).status;
  });
  if (ok !== 'authorized') throw new Error('sign-in failed ' + ok);
  const s = await ctx.storageState();
  await ctx.close();
  return s;
}

// opts.transform(url, jsonOrText) lets a scene change responses (synthetic pool status).
async function open(name, opts = {}) {
  const browser = await chromium.launch();
  const state = opts.anon ? undefined : await storage(browser);
  const ctx = await browser.newContext({ viewport: { width: W, height: H }, storageState: state, permissions: ['clipboard-read', 'clipboard-write'] });
  await ctx.addInitScript(OVERLAY);
  await ctx.route('**/*', async route => {
    const req = route.request();
    if (!req.url().startsWith(BASE)) return route.continue();
    if (opts.fulfill) {
      const f = opts.fulfill(new URL(req.url()).pathname, req);
      if (f) return route.fulfill({ status: f.status || 200, contentType: 'application/json', body: JSON.stringify(f.body) });
    }
    const resp = await route.fetch();
    const ct = resp.headers()['content-type'] || '';
    if (!/json|html|javascript|text/.test(ct)) return route.fulfill({ response: resp });
    let body = mask(await resp.text());
    if (opts.transform && /json/.test(ct)) {
      try { body = JSON.stringify(opts.transform(new URL(req.url()).pathname, JSON.parse(body), req)); } catch (e) { /* keep */ }
    }
    if (opts.transformText) body = opts.transformText(new URL(req.url()).pathname, body);
    return route.fulfill({ response: resp, body, headers: { ...resp.headers(), 'content-length': String(Buffer.byteLength(body)) } });
  });
  const page = await ctx.newPage();
  // Lossless frames from the DevTools screencast: unchanged screen areas stay identical between
  // frames, so the GIF stays small (Playwright's video is lossy and noisy).
  const frames = [];
  const fdir = path.join(WORK, 'frames', name);
  fs.rmSync(fdir, { recursive: true, force: true });
  fs.mkdirSync(fdir, { recursive: true });
  const cdp = await ctx.newCDPSession(page);
  cdp.on('Page.screencastFrame', async f => {
    const buf = Buffer.from(f.data, 'base64');
    // Element screenshots show up as odd-sized frames; keep only full-viewport ones.
    if (buf.readUInt32BE(16) === W && buf.readUInt32BE(20) === H && page.url() !== 'about:blank') {
      const file = path.join(fdir, String(frames.length).padStart(5, '0') + '.png');
      frames.push({ file, t: f.metadata.timestamp });
      fs.writeFileSync(file, buf);
    }
    cdp.send('Page.screencastFrameAck', { sessionId: f.sessionId }).catch(() => {});
  });
  await cdp.send('Page.startScreencast', { format: 'png', maxWidth: W, maxHeight: H, everyNthFrame: 1 });
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  page.on('dialog', d => (opts.onDialog ? opts.onDialog(d) : d.accept()));
  const h = helpers(page);
  return { browser, ctx, page, errors, frames, cdp, ...h };
}

function helpers(page) {
  const wait = ms => page.waitForTimeout(ms);
  async function moveTo(x, y, steps = 18) {
    await page.mouse.move(x, y, { steps });
  }
  async function box(sel) {
    const el = typeof sel === 'string' ? page.locator(sel).first() : sel;
    await el.scrollIntoViewIfNeeded();
    await wait(150);
    return el.boundingBox();
  }
  async function click(sel, pause = 350) {
    const b = await box(sel);
    await moveTo(b.x + b.width / 2, b.y + b.height / 2);
    await wait(180);
    await page.mouse.down(); await wait(90); await page.mouse.up();
    await wait(pause);
  }
  async function type(sel, text, delay = 70) {
    await click(sel, 150);
    await page.locator(sel).first().fill('');
    await page.keyboard.type(text, { delay });
    await page.locator(sel).first().dispatchEvent('input');
    await wait(250);
  }
  async function caption(text, ms = 0) {
    await page.evaluate(t => { const c = document.getElementById('__cap'); if (!c) return; if (t) { c.textContent = t; c.classList.add('on'); } else c.classList.remove('on'); }, text);
    if (ms) await wait(ms);
  }
  async function card(text, ms = 1800) {
    await page.evaluate(t => { const c = document.getElementById('__card'); c.textContent = t; c.classList.add('on'); }, text);
    await wait(ms);
    await page.evaluate(() => document.getElementById('__card').classList.remove('on'));
  }
  async function scrollTo(sel, block = 'start') {
    await page.locator(sel).first().evaluate((e, b) => e.scrollIntoView({ behavior: 'smooth', block: b }), block);
    await wait(700);
  }
  // Step screenshot of an element (or the viewport) into docs/images/<guide>/<name>.png,
  // with the caption bar and cursor hidden.
  async function shot(guide, name, sel, pad = 12) {
    const dir = path.join(IMAGES, guide);
    fs.mkdirSync(dir, { recursive: true });
    await page.evaluate(() => { for (const id of ['__cap', '__cur']) { const e = document.getElementById(id); if (e) e.style.visibility = 'hidden'; } });
    const file = path.join(dir, `${name}.png`);
    if (sel) {
      const el = page.locator(sel).first();
      await el.scrollIntoViewIfNeeded();
      const b = await el.boundingBox();
      const x = Math.max(0, b.x - pad), y = Math.max(0, b.y - pad);
      await page.screenshot({ path: file, clip: { x, y, width: Math.min(W - x, b.width + 2 * pad), height: Math.min(H - y, b.height + 2 * pad) } });
    } else {
      await page.screenshot({ path: file });
    }
    await page.evaluate(() => { for (const id of ['__cap', '__cur']) { const e = document.getElementById(id); if (e) e.style.visibility = ''; } });
    return file;
  }
  // Screenshot from the top of `topSel` to the bottom of `bottomSel`, across `topSel`'s width.
  async function shotRange(guide, name, topSel, bottomSel, pad = 10) {
    const dir = path.join(IMAGES, guide);
    fs.mkdirSync(dir, { recursive: true });
    await page.locator(topSel).first().evaluate(e => { const y = e.getBoundingClientRect().top + window.scrollY - 110; window.scrollTo(0, y); });
    await wait(400);
    await page.evaluate(() => { for (const id of ['__cap', '__cur']) { const e = document.getElementById(id); if (e) e.style.visibility = 'hidden'; } });
    const t = await page.locator(topSel).first().boundingBox();
    const b = await page.locator(bottomSel).first().boundingBox();
    const x = Math.max(0, t.x - pad), y = Math.max(0, t.y - pad);
    const file = path.join(dir, `${name}.png`);
    await page.screenshot({ path: file, clip: { x, y, width: Math.min(W - x, t.width + 2 * pad), height: Math.min(H - y, b.y + b.height - t.y + 2 * pad) } });
    await page.evaluate(() => { for (const id of ['__cap', '__cur']) { const e = document.getElementById(id); if (e) e.style.visibility = ''; } });
    return file;
  }
  // Native prompt and confirm boxes never appear in the recording, so show a plain stand-in
  // with the same text first; the real box is then answered by opts.onDialog.
  async function fakeDialog(message, answer, ms = 2200) {
    await page.evaluate(([m, a]) => {
      const d = document.createElement('div'); d.id = '__dlg';
      d.style.cssText = 'position:fixed;inset:0;z-index:2147483600;background:rgba(0,0,0,.45);display:flex;align-items:flex-start;justify-content:center;padding-top:90px';
      const box = document.createElement('div');
      box.style.cssText = 'width:520px;background:#2b2d31;color:#e8e8e8;border:1px solid #4a4d55;border-radius:10px;padding:18px 20px;font:15px/1.45 system-ui,sans-serif;box-shadow:0 12px 40px rgba(0,0,0,.6)';
      const t = document.createElement('div'); t.style.cssText = 'font-weight:600;margin-bottom:6px'; t.textContent = location.host + ' says';
      const msg = document.createElement('div'); msg.textContent = m;
      box.append(t, msg);
      if (a !== null) { const i = document.createElement('div'); i.id = '__dlgin'; i.style.cssText = 'margin-top:12px;border:2px solid #5b8def;border-radius:6px;padding:6px 9px;background:#1e1f22;min-height:22px;font-family:ui-monospace,monospace'; box.append(i); }
      const b = document.createElement('div'); b.style.cssText = 'display:flex;justify-content:flex-end;gap:8px;margin-top:14px';
      for (const [label, bg] of [['Cancel', '#3a3c42'], ['OK', '#3b6fe0']]) { const x = document.createElement('span'); x.textContent = label; x.style.cssText = `background:${bg};padding:6px 16px;border-radius:6px;font-weight:600`; b.append(x); }
      box.append(b); d.append(box); document.documentElement.append(d);
    }, [message, answer]);
    if (answer !== null) for (const ch of answer) { await wait(220); await page.evaluate(c => { document.getElementById('__dlgin').textContent += c; }, ch); }
    await wait(ms);
    await page.evaluate(() => document.getElementById('__dlg').remove());
  }
  return { wait, moveTo, click, type, caption, card, scrollTo, box, shot, shotRange, fakeDialog };
}

async function close(r, outName) {
  await r.page.waitForTimeout(400);
  await r.cdp.send('Page.stopScreencast').catch(() => {});
  // Concat list: each frame shows until the next one arrived.
  const fr = r.frames;
  const end = Date.now() / 1000;
  const lines = [];
  for (let i = 0; i < fr.length; i++) {
    const d = Math.max(0.02, ((i + 1 < fr.length ? fr[i + 1].t : end) - fr[i].t));
    lines.push(`file '${fr[i].file}'`, `duration ${d.toFixed(3)}`);
  }
  if (fr.length) lines.push(`file '${fr[fr.length - 1].file}'`);
  fs.mkdirSync(path.join(WORK, 'out'), { recursive: true });
  fs.writeFileSync(path.join(WORK, 'out', `${outName}.txt`), lines.join('\n') + '\n');
  await r.ctx.close();
  await r.browser.close();
  if (r.errors.length) console.log('PAGE ERRORS', r.errors);
  return path.join(WORK, 'out', `${outName}.txt`);
}

module.exports = { open, close, BASE, mask, W, H, OVERLAY, WORK };
