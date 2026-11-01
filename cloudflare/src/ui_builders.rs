//! Dashboard script for dedicated Rust builders, served at `/ui/builders.js` to administrators.
//!
//! It adds a "Your Rust builders" panel to the Machine Qualification page (launch, connect, delete) and defines
//! `window.openBuilderLaunch(row)`, which takes an optional qualification row (zone, machine type, price) so a
//! table button can open the same dialog. Every value from the server reaches the page through `textContent`
//! or form values, never through HTML strings.

pub const BUILDERS_JS: &str = r#####"(function () {
"use strict";
const KEY_STORE = "spot.builder.sshPublicKey";
const S = { data: null, loading: false };

function h(tag, attrs, children) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "class") n.className = v;
    else if (k === "text") n.textContent = String(v);
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else if (v !== undefined && v !== null && v !== false) n.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of [].concat(children || [])) {
    if (c === null || c === undefined || c === false) continue;
    n.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return n;
}
function toast(msg) {
  if (typeof window.showToast === "function") window.showToast(msg);
  else console.log(msg);
}
async function api(url, init) {
  let res;
  try { res = await fetch(url, init || {}); } catch (e) { return [{ ok: false, status: 0 }, { message: String(e) }]; }
  let data = {};
  try { data = await res.json(); } catch (_e) { data = {}; }
  return [res, data || {}];
}
function postJson(url, body) {
  return api(url, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
}
function errText(d, res) { return (d && (d.message || d.error)) || ("HTTP " + (res && res.status)); }
function safeDocsUrl(u) { return /^https:\/\//i.test(String(u || "")) ? String(u) : "#"; }

function codeBlock(text) {
  const pre = h("pre", { class: "mono", style: "white-space:pre; overflow:auto; background:var(--bg-base, #131314); border:1px solid var(--border, #3c4043); border-radius:8px; padding:10px; margin:6px 0 4px; font-size:12px; max-height:320px;" }, [text]);
  const copy = h("button", { class: "btn btn-sm btn-secondary", type: "button", onclick: async () => {
    try { await navigator.clipboard.writeText(text); toast("Copied"); } catch (_e) { toast("Copy failed: select the text and copy it"); }
  } }, ["Copy"]);
  return h("div", {}, [pre, copy]);
}

function modal(title, body, actions) {
  const dlg = h("dialog", { style: "max-width:760px; width:92vw; background:var(--bg-panel, #1e1f20); color:var(--text-main, #e8eaed); border:1px solid var(--border, #3c4043); border-radius:12px; padding:18px;" });
  const close = h("button", { class: "btn btn-sm btn-secondary", type: "button", onclick: () => dlg.close() }, ["Close"]);
  dlg.appendChild(h("div", { style: "display:flex; justify-content:space-between; align-items:center; gap:12px; margin-bottom:8px;" }, [
    h("div", { class: "panel-title", text: title }), close
  ]));
  dlg.appendChild(body);
  if (actions && actions.length) dlg.appendChild(h("div", { style: "display:flex; gap:8px; margin-top:14px; flex-wrap:wrap;" }, actions));
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  return dlg;
}

// ---------------------------------------------------------------- launch
function openBuilderLaunch(row) {
  row = row || {};
  const zoneInput = h("input", { type: "text", value: row.zone || "us-central1-a", style: "width:100%;", spellcheck: "false", "aria-label": "Zone" });
  const keyInput = h("textarea", { rows: "3", style: "width:100%; font-family:monospace; font-size:12px;", spellcheck: "false", placeholder: "ssh-ed25519 AAAA... you@your-mac", "aria-label": "SSH public key" });
  try { keyInput.value = localStorage.getItem(KEY_STORE) || ""; } catch (_e) {}
  const status = h("div", { class: "hint", role: "status" });
  const mt = (S.data && S.data.machine_type) || "n4-highcpu-16";
  const shapeNote = row.machine_type && row.machine_type !== mt
    ? `This row is ${row.machine_type}. Builders use the reviewed ${mt} shape, so this launches an ${mt} in the same zone.`
    : `Shape: ${mt} (16 vCPU, 32 GiB RAM) as a Spot VM, with one build container capped at 16 CPUs and 28 GiB.`;
  const price = (row.machine_type === mt && typeof row.evaluated_hourly_usd === "number" && isFinite(row.evaluated_hourly_usd))
    ? `About $${row.evaluated_hourly_usd.toFixed(3)}/hour while it runs (this row's estimate). `
    : "";
  const body = h("div", {}, [
    h("p", { class: "hint", text: shapeNote }),
    h("p", { class: "hint", text: price + "It also creates a 375 GB hyperdisk-balanced cache disk (roughly $30/month) that stays until you delete it here, so later builds keep their Cargo cache." }),
    h("label", { class: "field" }, [h("span", { text: "Zone" }), zoneInput]),
    h("label", { class: "field", style: "display:block; margin-top:10px;" }, [
      h("span", { text: "Your SSH public key (Ed25519)" }), keyInput
    ]),
    h("p", { class: "hint" }, [
      "On your Mac run ", h("code", { text: "cat ~/.ssh/id_ed25519.pub" }),
      " and paste the line. No key yet? Run ", h("code", { text: "ssh-keygen -t ed25519" }),
      " first. Only the public key leaves your Mac. It signs in to the builder as your own unprivileged account, which can run builds through the locked-down launcher but has no other sudo rights."
    ]),
    h("p", { class: "hint", text: "Setup on the new VM takes about 15 to 25 minutes (it installs Docker and builds the Rust 1.99 image). Builds wait for it automatically; cargo remote status shows when it is ready." }),
    status,
  ]);
  const launch = h("button", { class: "btn btn-purple", type: "button" }, ["Launch builder"]);
  const dlg = modal("Launch a Rust builder", body, [launch]);
  launch.addEventListener("click", async () => {
    const zone = zoneInput.value.trim();
    const key = keyInput.value.trim();
    if (!zone || !key) { status.textContent = "Enter a zone and paste your public key."; return; }
    launch.disabled = true;
    status.textContent = "Launching…";
    const [res, d] = await postJson("/api/builders/launch", { zone, ssh_public_key: key });
    launch.disabled = false;
    if (!res.ok) { status.textContent = "Not launched: " + errText(d, res); return; }
    try { localStorage.setItem(KEY_STORE, key); } catch (_e) {}
    toast(`Launching ${d.name} in ${d.zone}`);
    dlg.close();
    refresh();
  });
}

// ---------------------------------------------------------------- connect
function openConnect() {
  const d = S.data || {};
  const install = d.install_command || "";
  const docs = safeDocsUrl(d.docs_url);
  const body = h("div", {}, [
    h("h4", { text: "1. Install once (paste into Terminal)" }),
    install ? codeBlock(install) : h("p", { class: "hint", text: "The install line appears once the dashboard can reach GCP." }),
    h("h4", { text: "2. Build any Rust project" }),
    codeBlock("cargo remote build --release"),
    h("p", { class: "hint" }, [
      "The Linux x86_64 binary lands in target/release/. It finds your running builders with gcloud, gives each project one in turn and keeps it there so its cache stays warm, and moves on if a builder is down. It doesn't edit ~/.ssh/config. ",
      h("a", { href: docs, target: "_blank", rel: "noopener noreferrer", text: "More in the setup guide" }), "."
    ]),
  ]);
  modal("Build on your builders from your Mac", body, []);
}

// ---------------------------------------------------------------- delete
async function deleteBuilder(b) {
  if (!confirm(`Delete ${b.name}? Its 375 GB cache disk stays (and keeps costing about $30/month) until you delete it under "Leftover cache disks".`)) return;
  const [res, d] = await postJson("/api/builders/delete", { name: b.name, zone: b.zone });
  if (!res.ok) { toast("Not deleted: " + errText(d, res)); return; }
  toast(`Deleting ${d.deleting}`);
  refresh();
}
async function deleteDisk(disk) {
  if (!confirm(`Delete cache disk ${disk.name}? Its Cargo cache and build outputs are gone for good.`)) return;
  const [res, d] = await postJson("/api/builders/delete-cache-disk", { name: disk.name, zone: disk.zone });
  if (!res.ok) { toast("Not deleted: " + errText(d, res)); return; }
  toast(`Deleting ${d.deleting}`);
  refresh();
}

// ---------------------------------------------------------------- panel
function ensurePanel() {
  let panel = document.getElementById("builders-panel");
  if (panel) return panel;
  const anchorBtn = document.getElementById("qual-role-btn-builder");
  const anchor = anchorBtn && anchorBtn.closest(".panel");
  if (!anchor || !anchor.parentNode) return null;
  panel = h("div", { class: "panel", id: "builders-panel" });
  anchor.parentNode.insertBefore(panel, anchor);
  return panel;
}

function render() {
  const panel = ensurePanel();
  if (!panel) return;
  while (panel.firstChild) panel.removeChild(panel.firstChild);
  const d = S.data || {};
  const canManage = !!d.can_manage;
  const launchBtn = h("button", { class: "btn btn-purple", type: "button", onclick: () => openBuilderLaunch({}) }, ["Launch builder"]);
  if (!canManage) { launchBtn.disabled = true; launchBtn.title = "Only the dashboard owner can launch builders"; }
  panel.appendChild(h("div", { class: "panel-header" }, [
    h("div", {}, [
      h("div", { class: "panel-title", text: "🦀 Your Rust builders" }),
      h("div", { class: "panel-subtitle" }, [
        "Spot VMs that compile your Rust projects. Launch one, install cargo-remote once with Use from my Mac, then run cargo remote build (",
        h("a", { href: safeDocsUrl(d.docs_url), target: "_blank", rel: "noopener noreferrer", text: "setup guide" }),
        ")."
      ]),
    ]),
    h("div", { style: "display:flex; gap:8px; align-items:center;" }, [
      h("button", { class: "btn btn-sm btn-secondary", type: "button", onclick: refresh }, [S.loading ? "Loading…" : "Refresh"]),
      h("button", { class: "btn btn-sm", type: "button", onclick: openConnect }, ["Use from my Mac"]),
      launchBtn,
    ]),
  ]));
  if (d.configured === false) {
    panel.appendChild(h("p", { class: "hint", text: "Builders are unavailable: " + (d.message || "GCP is not configured") }));
    return;
  }
  if (d.error) {
    panel.appendChild(h("p", { class: "hint", text: "Could not list builders: " + d.error }));
  }
  const builders = Array.isArray(d.builders) ? d.builders : [];
  if (!builders.length) {
    panel.appendChild(h("p", { class: "hint", text: "No builders yet. Press Launch builder to start one." }));
  } else {
    const rows = builders.map(b => h("tr", {}, [
      h("td", { class: "mono emph", text: b.name }),
      h("td", { class: "mono", text: b.zone || "" }),
      h("td", {}, [h("span", { class: "badge " + (String(b.status).toUpperCase() === "RUNNING" ? "badge-running" : "badge-deleted"), text: b.status || "?" })]),
      h("td", { class: "mono", text: b.external_ip || "—" }),
      h("td", { class: "mono", text: b.ssh_user || "—" }),
      h("td", {}, [
        canManage ? h("button", { class: "btn btn-sm btn-secondary", type: "button", onclick: () => deleteBuilder(b) }, ["Delete"]) : null,
      ]),
    ]));
    panel.appendChild(h("div", { class: "table-wrap" }, [h("table", {}, [
      h("thead", {}, [h("tr", {}, ["Name", "Zone", "Status", "Address", "SSH user", ""].map(t => h("th", { text: t })))]),
      h("tbody", {}, rows),
    ])]));
  }
  const disks = Array.isArray(d.leftover_cache_disks) ? d.leftover_cache_disks : [];
  if (disks.length) {
    panel.appendChild(h("div", { class: "panel-subtitle", style: "margin-top:12px;", text: "Leftover cache disks (their builder is gone; each still costs money until deleted)" }));
    panel.appendChild(h("div", { class: "table-wrap" }, [h("table", {}, [
      h("tbody", {}, disks.map(k => h("tr", {}, [
        h("td", { class: "mono", text: k.name }),
        h("td", { class: "mono", text: k.zone || "" }),
        h("td", { class: "mono", text: (k.size_gb ? k.size_gb + " GB" : "") }),
        h("td", {}, [canManage ? h("button", { class: "btn btn-sm btn-secondary", type: "button", onclick: () => deleteDisk(k) }, ["Delete disk"]) : null]),
      ]))),
    ])]));
  }
}

async function refresh() {
  if (S.loading) return;
  S.loading = true;
  render();
  const [res, d] = await api("/api/builders");
  S.loading = false;
  S.data = res.ok ? d : Object.assign({}, S.data || {}, { error: errText(d, res) });
  render();
}

window.openBuilderLaunch = openBuilderLaunch;
window.refreshBuilders = refresh;
function start() { if (ensurePanel()) refresh(); }
if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
else start();
})();
"#####;
