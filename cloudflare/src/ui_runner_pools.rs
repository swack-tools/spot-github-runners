//! Dashboard script for runner pools: the Launch dialog on the Qualifying Instances table and the
//! Runner pools panel under it. Served at `GET /ui/runner-pools.js` to administrators only, and
//! kept out of `DASHBOARD_HTML`, which is close to its size budget.
//!
//! It builds its DOM with `textContent` and attributes; the only HTML strings it writes go through
//! the dashboard's `esc()`.

/// Keep this well under the dashboard's own budget; the Worker bundle limit is far larger.
pub const RUNNER_POOLS_JS_MAX_BYTES: usize = 48 * 1024;

pub static RUNNER_POOLS_JS: &str = r##"(() => {
"use strict";
let POOLS = { pools: [], templates: [], targets: [], budget_max_hourly_usd: null, limits: {} };
let ROW = null;
const $ = (id) => document.getElementById(id);
const el = (tag, props = {}, ...kids) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "text") n.textContent = v;
    else if (k === "class") n.className = v;
    else if (k === "style") n.style.cssText = v;
    else n.setAttribute(k, v);
  }
  for (const kid of kids) if (kid !== null && kid !== undefined) n.append(kid);
  return n;
};
const field = (label, input, hint) => el("div", { class: "field" }, el("label", { for: input.id, text: label }), input, hint ? el("div", { class: "hint-sm", text: hint }) : null);
const num = (id, min, max, val) => el("input", { type: "number", id, min: String(min), max: String(max), value: String(val), step: "1" });
const usd = (x, digits = 4) => (typeof x === "number" && isFinite(x)) ? (x === 0 ? "$0" : `$${x.toFixed(digits)}`) : "—";
const archOf = (mt) => /^(c4a|n4a|t2a)-/.test(mt || "") ? "arm64" : "x86_64";

async function api(path, opts = {}) {
  const res = await fetch(path, { credentials: "same-origin", ...opts });
  let data = {};
  try { data = await res.json(); } catch (e) { /* empty body */ }
  if (!res.ok || data.error) throw new Error(data.message || data.error || res.statusText);
  return data;
}

// A matrix row's host, sized as the server sizes it: whole GiB, less what the host keeps.
function shapeOf(row) {
  const ram = Math.floor(row.ram_gib);
  return { vcpu: row.vcpu, ram_gib: ram, usable_ram_gib: Math.max(ram - ((POOLS.limits || {}).host_reserved_ram_gib || 2), 0) };
}

// The hourly cost of every pool that isn't being removed, against the budget cap.
function poolsHourly() {
  return (POOLS.pools || []).filter((p) => !p.removing).reduce((sum, p) => sum + (p.host_hourly_usd || 0) * p.hosts, 0);
}
function hourlyCap() {
  const cap = POOLS.budget_max_hourly_usd;
  return typeof cap === "number" && isFinite(cap) && cap > 0 ? cap : null;
}

// --- styles -------------------------------------------------------------------------------------
document.head.append(el("style", { text: `
#rp-dialog{margin:auto;max-width:720px;width:calc(100% - 32px);background:var(--bg-panel);color:var(--text-main);border:1px solid var(--border);border-radius:var(--radius-lg,12px);padding:var(--space-5,20px)}
#rp-dialog::backdrop{background:rgba(0,0,0,.6)}
#rp-dialog .form-grid{margin-top:var(--space-3,12px)}
#rp-dialog pre{max-height:160px;overflow:auto;background:var(--bg-sunken);border:1px solid var(--border);border-radius:8px;padding:8px;white-space:pre-wrap;font-size:12px}
#rp-check{margin:var(--space-3,12px) 0;padding:10px 12px;border-radius:8px;background:var(--bg-card);border:1px solid var(--border)}
#rp-check.bad{border-color:var(--danger-line);background:var(--danger-soft)}
#rp-actions,.rp-actions{display:flex;gap:8px;justify-content:flex-end}
.rp-err{color:var(--danger)}
.rp-ok{color:var(--success)}
.rp-snippet{display:flex;gap:6px;align-items:center;margin-top:4px}
.rp-snippet code{background:var(--bg-sunken);border:1px solid var(--border);border-radius:6px;padding:2px 6px;font-size:12px;white-space:nowrap}
#rp-done pre{max-height:none}
#rp-tpl-dialog{margin:auto;max-width:620px;width:calc(100% - 32px);background:var(--bg-panel);color:var(--text-main);border:1px solid var(--border);border-radius:var(--radius-lg,12px);padding:var(--space-5,20px)}
#rp-tpl-dialog::backdrop{background:rgba(0,0,0,.6)}
#rp-tpl-dialog pre{max-height:180px;overflow:auto;background:var(--bg-sunken);border:1px solid var(--border);border-radius:8px;padding:8px;white-space:pre-wrap;font-size:12px}
/* Keep the matrix's Launch column in view when the table scrolls sideways. */
.table-wrap:has(#qualification-table-body) :is(td,th):last-child{position:sticky;right:0}
.table-wrap:has(#qualification-table-body) td:last-child{background:var(--bg-panel);box-shadow:-10px 0 10px -10px rgba(0,0,0,.6)}
.table-wrap:has(#qualification-table-body) tr:hover td:last-child{background:var(--bg-card)}
#rp-qual-summary{display:flex;flex-wrap:wrap;gap:6px 16px;align-items:center;margin:0 0 var(--space-3,12px)}
.rp-launch-row td{position:static!important;box-shadow:none!important;white-space:normal;background:var(--bg-card)!important}
.rp-launch-box{position:sticky;left:0;max-width:640px;display:grid;gap:10px;padding:4px 0}
.rp-launch-line{display:flex;flex-wrap:wrap;gap:8px;align-items:center}
.rp-stepper input{width:64px;text-align:center}
.rp-bar{position:relative;display:flex;height:10px;border-radius:5px;overflow:hidden;background:var(--bg-sunken);border:1px solid var(--border)}
.rp-bar-used{background:var(--accent)}
.rp-bar-add{background:var(--success)}
.rp-bar.over .rp-bar-add{background:var(--warning)}
.rp-bar-cap{position:absolute;top:0;bottom:0;width:2px;background:var(--text-main)}
#ci-pool-hint{margin-top:12px;padding:10px 12px;border-radius:8px;background:var(--bg-card);border:1px solid var(--border)}
` }));

// --- Launch dialog ------------------------------------------------------------------------------
function buildDialog() {
  if ($("rp-dialog")) return $("rp-dialog");
  const target = el("select", { id: "rp-target" });
  const targetOther = el("input", { type: "text", id: "rp-target-other", placeholder: "org or owner/repo", autocomplete: "off" });
  const tpl = el("select", { id: "rp-template" });
  const labels = el("input", { type: "text", id: "rp-labels", placeholder: "e.g. rust, big-build", autocomplete: "off" });
  const form = el("form", { method: "dialog", id: "rp-form" },
    el("div", { class: "panel-title", id: "rp-title" }),
    el("div", { class: "panel-subtitle", id: "rp-subtitle" }),
    el("div", { class: "form-grid" },
      field("Organization or repository", target),
      field("Other organization or repository", targetOther, "Type org or owner/repo."),
      field("Runners on each host", num("rp-rph", 1, 8, 2)),
      field("vCPU per runner", num("rp-vcpu", 1, 64, 7)),
      field("RAM per runner (GiB)", num("rp-ram", 1, 256, 14)),
      field("Most hosts", num("rp-max-hosts", 1, 10, 1), "The pool never runs more Spot hosts than this."),
      field("Spin down when idle", el("input", { type: "checkbox", id: "rp-auto", checked: "" }), "Start hosts when jobs for these runners queue; delete hosts that sit idle."),
      field("Fewest hosts", num("rp-min", 0, 10, 0), "Hosts kept even when idle. 0 costs nothing while no jobs run."),
      field("Idle minutes before spin-down", num("rp-idle", 5, 240, 10)),
      field("Extra labels", labels, "Comma-separated. self-hosted, Linux, the architecture, spot, the machine type and the zone are always added."),
      field("Template", tpl, "Its shell commands run as root while the runner image builds, so jobs see what they install.")),
    el("pre", { id: "rp-tpl-preview", hidden: "" }),
    el("div", { id: "rp-check" }),
    el("div", { id: "rp-actions" },
      el("button", { type: "button", class: "btn btn-secondary", id: "rp-cancel", text: "Cancel" }),
      el("button", { type: "submit", class: "btn", id: "rp-submit", text: "Launch" })));
  const done = el("div", { id: "rp-done", hidden: "" },
    el("div", { class: "panel-title", text: "Runner pool saved" }),
    el("div", { class: "panel-subtitle", id: "rp-done-text" }),
    el("p", { class: "hint", text: "Point a workflow job at these runners with this runs-on line:" }),
    el("pre", { id: "rp-done-yaml", class: "mono" }),
    el("div", { class: "rp-actions" },
      el("button", { type: "button", class: "btn btn-secondary", id: "rp-done-copy", text: "Copy runs-on" }),
      el("button", { type: "button", class: "btn", id: "rp-done-close", text: "Done" })));
  const dlg = el("dialog", { id: "rp-dialog", "aria-labelledby": "rp-title" }, form, done);
  document.body.append(dlg);
  $("rp-cancel").addEventListener("click", () => dlg.close());
  $("rp-done-close").addEventListener("click", () => dlg.close());
  $("rp-done-copy").addEventListener("click", () => copyText($("rp-done-yaml").textContent));
  $("rp-auto").addEventListener("change", () => { autoFields(); check(); });
  targetOther.closest(".field").id = "rp-target-other-field";
  target.addEventListener("change", () => { $("rp-target-other-field").hidden = target.value !== "__other"; check(); });
  for (const id of ["rp-target-other", "rp-rph", "rp-vcpu", "rp-ram", "rp-max-hosts", "rp-labels", "rp-min", "rp-idle"]) $(id).addEventListener("input", check);
  tpl.addEventListener("change", () => { showTemplate(); check(); });
  form.addEventListener("submit", (ev) => { ev.preventDefault(); submitPool(); });
  return dlg;
}

function autoFields() {
  const on = $("rp-auto").checked;
  $("rp-min").closest(".field").hidden = !on;
  $("rp-idle").closest(".field").hidden = !on;
}

function runsOnLine(labels) {
  return `runs-on: [${(labels || []).join(", ")}]`;
}

async function copyText(text) {
  try { await navigator.clipboard.writeText(text); showToast("Copied to the clipboard."); }
  catch (e) { showToast("Copy didn't work here; select the text and copy it."); }
}

// The runs-on labels for the dialog's current settings, as the server builds them.
function dialogRunsOn() {
  const arch = archOf(ROW.machine_type) === "arm64" ? "ARM64" : "X64";
  const own = currentLabels();
  return ["self-hosted", arch, "spot", ...(own.length ? own : [ROW.machine_type])];
}

function fillTargets() {
  const sel = $("rp-target");
  sel.replaceChildren();
  for (const t of POOLS.targets || []) sel.append(el("option", { value: t, text: t.includes("/") ? `${t} (repository)` : `${t} (organization)` }));
  sel.append(el("option", { value: "__other", text: "Other organization or repository…" }));
  $("rp-target-other-field").hidden = sel.value !== "__other";
}

function fillTemplates() {
  const sel = $("rp-template");
  sel.replaceChildren(el("option", { value: "", text: "Built-in runner setup only" }));
  for (const t of POOLS.templates || []) {
    const o = el("option", { value: t.id, text: t.usable ? t.name : `${t.name} (can't be used: ${t.reason})` });
    if (!t.usable) o.disabled = true;
    sel.append(o);
  }
  showTemplate();
}

function showTemplate() {
  const pre = $("rp-tpl-preview");
  const t = (POOLS.templates || []).find((x) => x.id === $("rp-template").value);
  pre.hidden = !t;
  pre.textContent = t ? t.preview : "";
}

function currentTarget() {
  const v = $("rp-target").value;
  return (v === "__other" ? $("rp-target-other").value : v).trim();
}

function currentLabels() {
  return $("rp-labels").value.split(",").map((s) => s.trim()).filter(Boolean);
}

function check() {
  const box = $("rp-check");
  const shape = shapeOf(ROW);
  const rph = parseInt($("rp-rph").value, 10) || 0;
  const vcpu = parseInt($("rp-vcpu").value, 10) || 0;
  const ram = parseInt($("rp-ram").value, 10) || 0;
  const hosts = parseInt($("rp-max-hosts").value, 10) || 0;
  const auto = $("rp-auto").checked;
  const minHosts = parseInt($("rp-min").value, 10);
  const idle = parseInt($("rp-idle").value, 10);
  const problems = [];
  const target = currentTarget();
  if (!/^[A-Za-z0-9][A-Za-z0-9-]{0,38}(\/[A-Za-z0-9._-]{1,100})?$/.test(target)) problems.push("Pick an organization, or type org or owner/repo.");
  if (rph < 1 || rph > 8) problems.push("Runners on each host must be 1 to 8.");
  if (hosts < 1 || hosts > 10) problems.push("Most hosts must be 1 to 10.");
  if (auto && !(minHosts >= 0 && minHosts < hosts)) problems.push(`Fewest hosts must be 0 to ${Math.max(hosts - 1, 0)} (fewer than the most hosts).`);
  if (auto && !(idle >= 5 && idle <= 240)) problems.push("Idle minutes must be 5 to 240.");
  if (rph * vcpu > shape.vcpu) problems.push(`${rph} × ${vcpu} vCPU = ${rph * vcpu}, but the host has ${shape.vcpu}.`);
  if (rph * ram > shape.usable_ram_gib) problems.push(`${rph} × ${ram} GiB = ${rph * ram} GiB, but the host has ${shape.usable_ram_gib} GiB free for runners (${shape.ram_gib} GiB minus ${POOLS.limits.host_reserved_ram_gib || 2} for the host).`);
  const cap = hourlyCap();
  const total = poolsHourly() + (ROW.pool_host_hourly_usd || 0) * hosts;
  if (cap !== null && total > cap + 1e-9) problems.push(`All pools together would cost about ${usd(total)}/hr, above the ${usd(cap, 2)}/hr budget cap. Use fewer hosts or raise the cap.`);
  const badLabel = currentLabels().find((l) => !/^[A-Za-z0-9._-]{1,64}$/.test(l));
  if (badLabel) problems.push(`Label "${badLabel}" may use only letters, digits, ".", "_" and "-".`);
  box.replaceChildren();
  box.className = problems.length ? "bad" : "";
  if (problems.length) {
    for (const p of problems) box.append(el("div", { class: "rp-err", text: p }));
  } else {
    const host = ROW.evaluated_hourly_usd || 0;
    const each = `each running ${rph} runner${rph > 1 ? "s" : ""} with ${vcpu} vCPU and ${ram} GiB, for ${target}`;
    if (auto) {
      box.append(
        el("div", { text: `Up to ${hosts} × ${ROW.machine_type} in ${ROW.zone}, ${each}.` }),
        el("div", { class: "hint", text: `Starts a host when a job for ${runsOnLine(dialogRunsOn()).slice(9)} is queued, and deletes hosts idle for ${idle} minutes, down to ${minHosts}. About ${usd(host * minHosts)}/hr when idle, up to ${usd(host * hosts)}/hr with every host busy. A new host takes about 10 minutes to take its first job.` }));
    } else {
      box.append(
        el("div", { text: `${hosts} × ${ROW.machine_type} in ${ROW.zone}, ${each}, always running.` }),
        el("div", { class: "hint", text: `About ${usd(host * hosts)}/hr (${usd(host * hosts * 24, 2)}/day) at the listed Spot price. The reconciler starts the first host within 5 minutes; runners usually come online 5 to 10 minutes after that.` }));
    }
  }
  $("rp-submit").disabled = problems.length > 0;
}

async function openRunnerLaunch(row, hosts) {
  ROW = row;
  const dlg = buildDialog();
  try { POOLS = await api("/api/runner-pools"); } catch (e) { showToast(`Couldn't load runner pools: ${e.message}`); return; }
  $("rp-title").textContent = `Launch runners on ${row.machine_type} (${archOf(row.machine_type) === "arm64" ? "ARM64" : "x86_64"})`;
  $("rp-subtitle").textContent = `${row.display_location || ""} ${row.zone} · ${row.cpu_platform || ""} · ${row.vcpu} vCPU / ${row.ram_gib} GiB`;
  fillTargets();
  fillTemplates();
  $("rp-rph").value = String(Math.min(row.units_per_instance || 1, 8));
  $("rp-vcpu").value = String(row.unit_vcpu || 7);
  $("rp-ram").value = String(row.unit_ram_gib || 14);
  $("rp-max-hosts").value = String(hosts || 2);
  $("rp-labels").value = "";
  $("rp-auto").checked = true;
  $("rp-min").value = "0";
  $("rp-idle").value = String((POOLS.limits || {}).default_idle_minutes || 10);
  $("rp-form").hidden = false;
  $("rp-done").hidden = true;
  autoFields();
  check();
  dlg.showModal();
}

async function submitPool() {
  const btn = $("rp-submit");
  btn.disabled = true;
  try {
    const body = {
      target: currentTarget(),
      machine_type: ROW.machine_type,
      zone: ROW.zone,
      hosts: parseInt($("rp-max-hosts").value, 10),
      runners_per_host: parseInt($("rp-rph").value, 10),
      runner_vcpu: parseInt($("rp-vcpu").value, 10),
      runner_ram_gib: parseInt($("rp-ram").value, 10),
      labels: currentLabels(),
      cloud_init_template_id: $("rp-template").value,
    };
    if ($("rp-auto").checked) {
      body.min_hosts = parseInt($("rp-min").value, 10);
      body.idle_minutes = parseInt($("rp-idle").value, 10);
    }
    POOLS = await api("/api/runner-pools", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    renderPools();
    const pool = (POOLS.pools || []).find((p) => p.id === POOLS.created) || {};
    $("rp-done-text").textContent = pool.autoscales
      ? `Pool ${POOLS.created} has no hosts until a job asks for these runners. The reconciler checks every 5 minutes, then starts a host; its runners take the job about 10 minutes later.`
      : `Pool ${POOLS.created}: the reconciler starts its first host within 5 minutes; runners usually come online 5 to 10 minutes after that.`;
    $("rp-done-yaml").textContent = runsOnLine(pool.runs_on || dialogRunsOn());
    $("rp-form").hidden = true;
    $("rp-done").hidden = false;
  } catch (e) {
    const box = $("rp-check");
    box.className = "bad";
    box.replaceChildren(el("div", { class: "rp-err", text: e.message }));
  } finally {
    btn.disabled = false;
  }
}

// --- Runner pools panel -------------------------------------------------------------------------
function buildPanel() {
  if ($("rp-panel")) return $("rp-panel");
  const anchor = $("qualification-table-body") && $("qualification-table-body").closest(".panel");
  if (!anchor) return null;
  const panel = el("div", { class: "panel", id: "rp-panel" },
    el("div", { class: "panel-header" },
      el("div", {},
        el("div", { class: "panel-title", text: "Runner pools" }),
        el("div", { class: "panel-subtitle", text: "Spot hosts launched with the Launch button above. The reconciler keeps each pool at its host count and replaces hosts Spot takes back. Remove deletes the hosts and their runner registrations." })),
      el("button", { type: "button", class: "btn btn-sm btn-secondary", id: "rp-pools-refresh", text: "Refresh" })),
    el("div", { class: "table-wrap" },
      el("table", {},
        el("thead", {}, el("tr", {}, ...["Pool", "Org or repository", "Machine / zone", "Runners", "Labels", "Template", "Status", "Cost", ""].map((h) => el("th", { text: h })))),
        el("tbody", { id: "rp-body" }))));
  anchor.after(panel);
  $("rp-pools-refresh").addEventListener("click", refreshPools);
  $("rp-body").addEventListener("click", (ev) => {
    const b = ev.target.closest("button[data-remove]");
    if (b) removePool(b.getAttribute("data-remove"));
    const r = ev.target.closest("button[data-resize]");
    if (r) resizePool(r.getAttribute("data-resize"));
    const c = ev.target.closest("button[data-copy]");
    if (c) copyText(c.getAttribute("data-copy"));
    const t = ev.target.closest("button[data-template]");
    if (t) openTemplateDialog(t.getAttribute("data-template"));
  });
  return panel;
}

function statusCell(p) {
  const s = p.status || {};
  const td = el("td");
  if (p.removing) {
    td.append(el("span", { class: "badge badge-deleted", text: "Removing" }), el("div", { class: "hint-sm", text: `${s.hosts_running || 0} host(s) left` }));
    return td;
  }
  const online = s.runners_online || 0;
  const running = s.hosts_running || 0;
  if (p.autoscales) {
    const want = running * p.runners_per_host;
    if (running === 0 && !s.queued_jobs) td.append(el("span", { class: "rp-ok", text: "Spun down · no hosts running" }));
    else td.append(el("span", { class: running && online >= want ? "rp-ok" : "", text: `${running} host${running === 1 ? "" : "s"} running · ${online}/${want} runners online` }));
    if (s.queued_jobs) td.append(el("div", { class: "hint-sm", text: `${s.queued_jobs} queued job${s.queued_jobs > 1 ? "s" : ""} for these runners; scaling to ${s.desired_hosts} host${s.desired_hosts === 1 ? "" : "s"}` }));
    else if (s.queued_jobs === null || s.queued_jobs === undefined) { if (s.desired_hosts !== undefined) td.append(el("div", { class: "hint-sm", text: "Couldn't read GitHub's job queue; keeping the hosts it has." })); }
    if (s.last_scale_down_at) td.append(el("div", { class: "hint-sm", text: `Last spin-down ${new Date(s.last_scale_down_at * 1000).toLocaleString()}` }));
    if (s.last_error) td.append(el("div", { class: "rp-err hint-sm", text: s.last_error }));
    return td;
  }
  const want = p.hosts * p.runners_per_host;
  td.append(el("span", { class: online >= want ? "rp-ok" : "", text: `${running}/${p.hosts} hosts · ${online}/${want} runners online` }));
  if (s.last_error) td.append(el("div", { class: "rp-err hint-sm", text: s.last_error }));
  else if (s.last_launch_host) td.append(el("div", { class: "hint-sm mono", text: `Last launch: ${s.last_launch_host}` }));
  else td.append(el("div", { class: "hint-sm", text: "Waiting for the next reconciler run (every 5 minutes)." }));
  return td;
}

function runnersCell(p) {
  const size = `${p.runners_per_host} runner${p.runners_per_host > 1 ? "s" : ""}/host (${p.runner_vcpu}c/${p.runner_ram_gib}G each)`;
  if (!p.autoscales) return el("td", { class: "mono" }, el("div", { text: `${p.hosts} hosts, always on` }), el("div", { class: "hint-sm", text: size }));
  return el("td", { class: "mono" },
    el("div", { text: `${p.min_hosts}–${p.hosts} hosts` }),
    el("div", { class: "hint-sm", text: size }),
    el("div", { class: "hint-sm", text: `Spins down after ${p.idle_minutes} idle min` }));
}

function snippet(p) {
  const line = runsOnLine(p.runs_on);
  const copy = el("button", { type: "button", class: "btn btn-sm btn-secondary", "data-copy": line, text: "Copy" });
  return el("div", { class: "rp-snippet" }, el("code", { text: line }), copy);
}

function templateCell(p) {
  const change = el("button", { type: "button", class: "btn btn-sm btn-secondary", "data-template": p.id, text: "Change" });
  if (p.removing) change.disabled = true;
  return el("td", {}, el("div", { text: p.cloud_init_template_name || "Built-in" }), change);
}

function renderPools() {
  if (!buildPanel()) return;
  const body = $("rp-body");
  body.replaceChildren();
  const pools = POOLS.pools || [];
  if (!pools.length) {
    body.append(el("tr", {}, el("td", { colspan: "9", class: "hint", text: "No runner pools yet. Use Launch on a row in the table above." })));
    return;
  }
  for (const p of pools) {
    const remove = el("button", { type: "button", class: "btn btn-sm btn-secondary", "data-remove": p.id, text: p.removing ? "Removing…" : "Remove" });
    if (p.removing) remove.disabled = true;
    const resize = el("button", { type: "button", class: "btn btn-sm btn-secondary", "data-resize": p.id, text: "Resize" });
    if (p.removing) resize.disabled = true;
    body.append(el("tr", {},
      el("td", { class: "mono", text: p.id }),
      el("td", { class: "mono", text: p.target }),
      el("td", {}, el("div", { class: "mono", text: `${p.machine_type} (${p.arch === "arm64" ? "ARM64" : "x86_64"})` }), el("div", { class: "hint-sm mono", text: p.zone })),
      runnersCell(p),
      el("td", {}, el("div", { class: "mono hint-sm", text: (p.labels || []).join(", ") }), snippet(p)),
      templateCell(p),
      statusCell(p),
      costCell(p),
      el("td", {}, resize, " ", remove)));
  }
  markLaunchable();
}

function costCell(p) {
  if (p.host_hourly_usd == null) return el("td", { class: "mono", text: "—" });
  const now = usd(p.host_hourly_usd * ((p.status || {}).hosts_running || 0));
  if (!p.autoscales) return el("td", { class: "mono", text: `${usd(p.host_hourly_usd * p.hosts)}/hr` });
  return el("td", { class: "mono" }, el("div", { text: `${now}/hr now` }), el("div", { class: "hint-sm", text: `up to ${usd(p.host_hourly_usd * p.hosts)}/hr` }));
}

// --- Change a pool's template ---------------------------------------------------------------------
function buildTemplateDialog() {
  if ($("rp-tpl-dialog")) return $("rp-tpl-dialog");
  const sel = el("select", { id: "rp-tpl-select" });
  const dlg = el("dialog", { id: "rp-tpl-dialog", "aria-labelledby": "rp-tpl-title" },
    el("form", { method: "dialog", id: "rp-tpl-form" },
      el("div", { class: "panel-title", id: "rp-tpl-title" }),
      el("div", { class: "panel-subtitle", text: "New hosts use this template. Idle hosts started with the old one are replaced one at a time; hosts running a job finish it first." }),
      el("div", { class: "form-grid" }, field("Template", sel)),
      el("pre", { id: "rp-tpl-preview2", hidden: "" }),
      el("div", { class: "rp-actions" },
        el("button", { type: "button", class: "btn btn-secondary", id: "rp-tpl-cancel", text: "Cancel" }),
        el("button", { type: "submit", class: "btn", id: "rp-tpl-save", text: "Use this template" }))));
  document.body.append(dlg);
  $("rp-tpl-cancel").addEventListener("click", () => dlg.close());
  sel.addEventListener("change", showTemplate2);
  $("rp-tpl-form").addEventListener("submit", (ev) => { ev.preventDefault(); saveTemplate(); });
  return dlg;
}

function showTemplate2() {
  const pre = $("rp-tpl-preview2");
  const t = (POOLS.templates || []).find((x) => x.id === $("rp-tpl-select").value);
  pre.hidden = !t;
  pre.textContent = t ? t.preview : "";
}

let TPL_POOL = null;
async function openTemplateDialog(id) {
  try { POOLS = await api("/api/runner-pools"); } catch (e) { showToast(`Couldn't load runner pools: ${e.message}`); return; }
  const p = (POOLS.pools || []).find((x) => x.id === id);
  if (!p) return;
  TPL_POOL = p;
  const dlg = buildTemplateDialog();
  $("rp-tpl-title").textContent = `Template for pool ${p.id}`;
  const sel = $("rp-tpl-select");
  sel.replaceChildren(el("option", { value: "", text: "Built-in runner setup only" }));
  for (const t of POOLS.templates || []) {
    const o = el("option", { value: t.id, text: t.usable ? t.name : `${t.name} (can't be used: ${t.reason})` });
    if (!t.usable) o.disabled = true;
    sel.append(o);
  }
  sel.value = p.cloud_init_template_id || "";
  showTemplate2();
  dlg.showModal();
}

async function saveTemplate() {
  const p = TPL_POOL;
  if (!p) return;
  const tplId = $("rp-tpl-select").value;
  try {
    POOLS = await api("/api/runner-pools/template", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id: p.id, cloud_init_template_id: tplId }) });
    $("rp-tpl-dialog").close();
    renderPools();
    const name = ((POOLS.templates || []).find((t) => t.id === tplId) || {}).name || "the built-in setup";
    showToast(`Pool ${p.id} now uses ${name}. New hosts start with it; idle hosts are replaced one at a time.`);
  } catch (e) { showToast(`Couldn't change the template: ${e.message}`); }
}

// --- Templates tab: shell-script runner templates ----------------------------------------------
const STARTER_SCRIPT = `#!/bin/bash
# Runs as root while each runner host builds its runner image,
# so every job on the pool can use what it installs.
set -euo pipefail
apt-get update
apt-get install -y --no-install-recommends protobuf-compiler jq
rm -rf /var/lib/apt/lists/*
`;

function setupTemplateTab() {
  const saveBtn = document.querySelector("#tab-cloudinit button[onclick^=\"saveCloudInit('save')\"]");
  if (!saveBtn || $("ci-new-script")) return;
  const row = saveBtn.parentElement;
  const btn = el("button", { type: "button", class: "btn btn-secondary", id: "ci-new-script", text: "＋ New runner setup script" });
  btn.addEventListener("click", () => {
    // A new script isn't any saved template: clear the picker so Save Current Template can't
    // overwrite the template that was selected before.
    $("ci-editor-select").value = "";
    $("ci-editor-role").value = "cicd_runner";
    $("ci-editor-name").value = "Runner tools";
    $("ci-editor-textarea").value = STARTER_SCRIPT;
    const assign = $("ci-assign-active");
    if (assign) assign.checked = false;
    if (typeof liveParseCloudInit === "function") liveParseCloudInit();
    $("ci-editor-name").focus();
    $("ci-editor-name").select();
  });
  row.prepend(btn);
  const orig = window.saveCloudInit;
  if (typeof orig !== "function") return;
  window.saveCloudInit = async function (mode) {
    const role = $("ci-editor-role").value;
    const script = $("ci-editor-textarea").value.trimStart().startsWith("#!");
    try { await orig(mode); } catch (e) { console.error(e); }
    if (role !== "cicd_runner" || !script) return;
    await refreshPools();
    let hint = $("ci-pool-hint");
    if (!hint) {
      const go = el("button", { type: "button", class: "btn btn-sm", id: "ci-pool-go", text: "Go to runner pools" });
      go.addEventListener("click", () => { switchTab("tab-sizing"); const panel = $("rp-panel"); if (panel) panel.scrollIntoView({ behavior: "smooth", block: "start" }); });
      hint = el("div", { id: "ci-pool-hint" },
        el("div", { text: "Runner pools can use this template. Pick it in the Launch dialog on Machine Qualification, or click Change in the Template column of Runner pools to roll it onto a running pool." }),
        el("div", { style: "margin-top:8px" }, go));
      row.after(hint);
    }
  };
}
setupTemplateTab();

async function refreshPools() {
  try { POOLS = await api("/api/runner-pools"); renderPools(); }
  catch (e) { showToast(`Couldn't load runner pools: ${e.message}`); }
}

async function removePool(id) {
  const p = (POOLS.pools || []).find((x) => x.id === id);
  if (!p) return;
  if (!confirm(`Remove pool ${p.id}? This deletes its ${p.hosts} host(s) for ${p.target} and their runner registrations. Running jobs on them stop.`)) return;
  try {
    POOLS = await api("/api/runner-pools/delete", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id }) });
    renderPools();
    showToast(`Pool ${id} is being removed. Its hosts go away within 5 minutes.`);
  } catch (e) { showToast(`Couldn't remove pool ${id}: ${e.message}`); }
}

async function resizePool(id) {
  const p = (POOLS.pools || []).find((x) => x.id === id);
  if (!p) return;
  const max = (POOLS.limits || {}).max_hosts_per_pool || 10;
  const answer = prompt(`Most hosts for pool ${p.id} (1 to ${max}, now ${p.hosts}). Scaling down deletes idle hosts first; a host running a job stays until the job ends.`, String(p.hosts));
  if (answer === null) return;
  const hosts = Number(answer.trim());
  if (!Number.isInteger(hosts) || hosts < 1 || hosts > max) { showToast(`Enter a whole number from 1 to ${max}.`); return; }
  const body = { id, hosts };
  if (p.autoscales) {
    const low = prompt(`Fewest hosts for pool ${p.id} (0 to ${hosts}, now ${p.min_hosts}). The pool keeps these hosts even when no jobs run; ${hosts} turns spin-down off.`, String(Math.min(p.min_hosts, hosts)));
    if (low === null) return;
    const min = Number(low.trim());
    if (!Number.isInteger(min) || min < 0 || min > hosts) { showToast(`Enter a whole number from 0 to ${hosts}.`); return; }
    body.min_hosts = min;
  }
  try {
    POOLS = await api("/api/runner-pools/resize", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    renderPools();
    showToast(`Pool ${id} now runs at most ${hosts} host(s). The reconciler adjusts it within 5 minutes.`);
  } catch (e) { showToast(`Couldn't resize pool ${id}: ${e.message}`); }
}

// --- Qualifying Instances table hook ------------------------------------------------------------
// ui.rs renders a Launch button per row that calls launchQualRow(<row index>) and keeps the shown
// rows in window.QUAL_ROWS. CI/CD rows open this dialog; Rust builder rows go to builders.js.
window.openRunnerLaunch = openRunnerLaunch;
window.launchQualRow = function (i) {
  const row = (window.QUAL_ROWS || [])[i];
  if (!row) return;
  if (QUAL_ACTIVE_ROLE !== "rust_builder") { toggleLaunchPanel(row); return; }
  if (typeof window.openBuilderLaunch === "function") window.openBuilderLaunch(row);
  else showToast("Launching from this table isn't available for this role yet.");
};

const rowKey = (row) => `${row.machine_type}|${row.zone}`;

// Every CI/CD row the matrix lists can host runners. What can still stop a launch is the state of
// the pools, so each row's chip says where it stands.
function poolsFull() {
  const max = (POOLS.limits || {}).max_pools;
  return !!max && (POOLS.pools || []).filter((p) => !p.removing).length >= max;
}
function overCap(row, hosts) {
  const cap = hourlyCap();
  const total = poolsHourly() + (row.pool_host_hourly_usd || 0) * hosts;
  return cap !== null && total > cap + 1e-9 ? Math.ceil(total * 100) / 100 : null;
}
const chip = (cls, text) => el("span", { class: `badge ${cls}`, text });

// --- Inline launch panel: one row under the machine, open for one machine at a time ----------------
let PANEL = null; // { key, row, hosts, target, busy, error, done }

function toggleLaunchPanel(row) {
  const open = PANEL && PANEL.key === rowKey(row);
  PANEL = open ? null : { key: rowKey(row), row, hosts: 1, target: (POOLS.targets || [])[0] || "", busy: false, error: "", done: null };
  const old = $("rp-launch-row");
  if (old) old.remove();
  syncLaunchPanel();
}

function costBar(row, hosts) {
  const cap = hourlyCap();
  if (cap === null) return null;
  const used = poolsHourly();
  const add = (row.pool_host_hourly_usd || 0) * hosts;
  const scale = Math.max(cap, used + add) || 1;
  const pct = (x) => `${(x / scale) * 100}%`;
  const bar = el("div", { class: `rp-bar${used + add > cap + 1e-9 ? " over" : ""}`, role: "img", "aria-label": `Runner pools would cost ${usd(used + add)} per hour against a ${usd(cap, 2)} per hour cap` },
    el("div", { class: "rp-bar-used", style: `width:${pct(used)}` }),
    el("div", { class: "rp-bar-add", style: `width:${pct(add)}` }),
    el("div", { class: "rp-bar-cap", style: `left:calc(${pct(cap)} - 2px)` }));
  return bar;
}

function fillLaunchPanel(box) {
  const p = PANEL;
  const row = p.row;
  box.replaceChildren();
  if (p.done) {
    const line = runsOnLine(p.done.runs_on || []);
    const copy = el("button", { type: "button", class: "btn btn-sm btn-secondary", text: "Copy runs-on" });
    copy.addEventListener("click", () => copyText(line));
    const close = el("button", { type: "button", class: "btn btn-sm", text: "Done" });
    close.addEventListener("click", () => toggleLaunchPanel(row));
    box.append(
      el("div", { class: "rp-ok", text: `Pool ${p.done.id} saved. It starts a host when a job asks for these runners; the reconciler checks every 5 minutes.` }),
      el("code", { class: "mono", text: line }),
      el("div", { class: "rp-launch-line" }, copy, close));
    return;
  }
  const max = (POOLS.limits || {}).max_hosts_per_pool || 10;
  const setHosts = (n) => { p.hosts = Math.min(Math.max(n, 1), max); p.error = ""; fillLaunchPanel(box); };
  const minus = el("button", { type: "button", class: "btn btn-sm btn-secondary", "aria-label": "Fewer hosts", text: "−" });
  const plus = el("button", { type: "button", class: "btn btn-sm btn-secondary", "aria-label": "More hosts", text: "+" });
  const count = el("input", { type: "number", min: "1", max: String(max), step: "1", value: String(p.hosts), "aria-label": "Most hosts" });
  minus.addEventListener("click", () => setHosts(p.hosts - 1));
  plus.addEventListener("click", () => setHosts(p.hosts + 1));
  count.addEventListener("change", () => setHosts(parseInt(count.value, 10) || 1));
  const target = el("select", { "aria-label": "Organization or repository" });
  for (const t of POOLS.targets || []) target.append(el("option", { value: t, text: t }));
  target.value = p.target;
  target.addEventListener("change", () => { p.target = target.value; });
  const raiseTo = overCap(row, p.hosts);
  const hostWord = `${p.hosts} host${p.hosts > 1 ? "s" : ""}`;
  const go = el("button", { type: "button", class: "btn btn-sm", text: raiseTo ? `Raise cap to ${usd(raiseTo, 2)}/hr and launch` : `Launch up to ${hostWord}` });
  go.disabled = p.busy || !p.target;
  go.addEventListener("click", () => launchFromPanel(box));
  const more = el("button", { type: "button", class: "btn btn-sm btn-secondary", text: "More options" });
  more.addEventListener("click", () => { const r = row; const h = p.hosts; toggleLaunchPanel(r); openRunnerLaunch(r, h); });
  const cancel = el("button", { type: "button", class: "btn btn-sm btn-secondary", text: "Cancel" });
  cancel.addEventListener("click", () => toggleLaunchPanel(row));
  const each = row.pool_host_hourly_usd || 0;
  const cap = hourlyCap();
  const total = poolsHourly() + each * p.hosts;
  box.append(...[
    el("div", { class: "rp-launch-line rp-stepper" }, el("span", { text: "Hosts" }), minus, count, plus, el("span", { text: "for" }), target),
    costBar(row, p.hosts),
    el("div", { class: "hint-sm", text: `Adds up to ${usd(each * p.hosts)}/hr. All pools: ${usd(total)}/hr${cap !== null ? ` of the ${usd(cap, 2)}/hr cap` : ""}. Hosts start when jobs queue and are deleted after sitting idle, so an idle pool costs nothing.` }),
    p.error ? el("div", { class: "rp-err", text: p.error }) : null,
    el("div", { class: "rp-launch-line" }, go, more, cancel),
  ].filter(Boolean));
}

function launchBody(row, hosts, target) {
  return {
    target,
    machine_type: row.machine_type,
    zone: row.zone,
    hosts,
    runners_per_host: Math.min(row.units_per_instance || 1, (POOLS.limits || {}).max_runners_per_host || 8),
    runner_vcpu: row.unit_vcpu,
    runner_ram_gib: Math.max(Math.floor(row.unit_ram_gib), 1),
    labels: [],
    cloud_init_template_id: "",
    min_hosts: 0,
    idle_minutes: (POOLS.limits || {}).default_idle_minutes || 10,
  };
}

async function launchFromPanel(box) {
  const p = PANEL;
  p.busy = true;
  p.error = "";
  fillLaunchPanel(box);
  const raiseTo = overCap(p.row, p.hosts);
  let raised = false;
  try {
    if (raiseTo) {
      await api("/api/budget", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ ...APP_STATE.global_budget, max_hourly_usd: raiseTo }) });
      raised = true;
    }
    const data = await api("/api/runner-pools", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(launchBody(p.row, p.hosts, p.target)) });
    POOLS = data;
    p.done = (POOLS.pools || []).find((x) => x.id === POOLS.created) || { id: POOLS.created };
    if (raised) await loadOverview();
    renderPools();
  } catch (e) {
    p.error = raised ? `The cap is now ${usd(raiseTo, 2)}/hr, but the pool wasn't saved: ${e.message}` : `Nothing changed: ${e.message}`;
    if (raised) await refreshPools();
  } finally {
    p.busy = false;
    if (PANEL === p) fillLaunchPanel(box);
  }
}

// Puts the open panel under its row again after the table redraws, or closes it when its row has
// gone. `data-after` makes the table's sorter keep the panel with its row.
function syncLaunchPanel() {
  const old = $("rp-launch-row");
  if (!PANEL) { if (old) old.remove(); return; }
  const rows = window.QUAL_ROWS || [];
  let host = null;
  document.querySelectorAll("#qualification-table-body button[onclick^='launchQualRow(']").forEach((b) => {
    const m = /launchQualRow\((\d+)\)/.exec(b.getAttribute("onclick") || "");
    const row = m ? rows[Number(m[1])] : null;
    if (row && rowKey(row) === PANEL.key && !b.hidden) { host = b.closest("tr"); PANEL.row = row; }
  });
  if (!host) { PANEL = null; if (old) old.remove(); return; }
  if (old && host.nextElementSibling === old) {
    if (!PANEL.busy && !old.contains(document.activeElement)) fillLaunchPanel(old.querySelector(".rp-launch-box"));
    return;
  }
  const tr = old || el("tr", { id: "rp-launch-row", class: "rp-launch-row", "data-after": "1" }, el("td", { colspan: String(host.cells.length) }, el("div", { class: "rp-launch-box" })));
  host.after(tr);
  if (!old) fillLaunchPanel(tr.querySelector(".rp-launch-box"));
}

function renderQualSummary(ready, needBudget) {
  const anchor = $("qualification-table-body") && $("qualification-table-body").closest(".table-wrap");
  if (!anchor) return;
  let strip = $("rp-qual-summary");
  if (!strip) { strip = el("div", { id: "rp-qual-summary" }); anchor.before(strip); }
  strip.hidden = QUAL_ACTIVE_ROLE !== "cicd_runner";
  const cap = hourlyCap();
  strip.replaceChildren(
    chip("badge-running", `${ready} ready`),
    chip(needBudget ? "badge-warn" : "badge-runner", `${needBudget} need a budget change`),
    el("span", { class: "mono", text: `Runner pools: ${usd(poolsHourly())}/hr${cap !== null ? ` of ${usd(cap, 2)}/hr cap` : ""}` }));
}

function markLaunchable() {
  if (QUAL_ACTIVE_ROLE !== "cicd_runner") { renderQualSummary(0, 0); return; }
  const rows = window.QUAL_ROWS || [];
  let ready = 0;
  let needBudget = 0;
  const full = poolsFull();
  // Header sorting reorders the rows, so read each button's own row index.
  document.querySelectorAll("#qualification-table-body button[onclick^='launchQualRow(']").forEach((b) => {
    const m = /launchQualRow\((\d+)\)/.exec(b.getAttribute("onclick") || "");
    const row = m ? rows[Number(m[1])] : null;
    if (!row) return;
    const over = overCap(row, 1) !== null;
    b.hidden = full;
    const status = b.closest("tr").cells[10];
    if (full) status.replaceChildren(chip("badge-warn", "Pools full"));
    else if (over) { needBudget += 1; status.replaceChildren(chip("badge-warn", "Over cap")); }
    else { ready += 1; status.replaceChildren(chip("badge-running", "Ready")); }
    if (row.price_qualifies === false) status.append(" ", chip("badge-deleted", "Over profile budget"));
    const cell = b.parentElement;
    const old = cell.querySelector(".rp-obstacle");
    if (old) old.remove();
    if (full) cell.append(el("div", { class: "rp-obstacle hint-sm", text: `All ${(POOLS.limits || {}).max_pools} runner pools are in use. Remove one in Runner pools.` }));
  });
  renderQualSummary(ready, needBudget);
  syncLaunchPanel();
}

const tbody = $("qualification-table-body");
// An empty matrix says what to change. The page's own script has no room for this: see its size test.
const drawMatrix = window.renderQualificationTable;
window.renderQualificationTable = function () {
  drawMatrix();
  if (tbody && !tbody.rows.length) tbody.append(el("tr", {}, el("td", { colspan: "12", class: "hint", text: "No machine fits right now. Uncheck Show Only Spot-Price-Qualifying, raise the budget, lower the hardware or benchmark floor, or widen the filters." })));
};
if (tbody) new MutationObserver(markLaunchable).observe(tbody, { childList: true });

refreshPools();
setInterval(() => { if (!document.hidden && $("rp-panel")) refreshPools(); }, 60000);
})();
"##;
