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
let POOLS = { pools: [], templates: [], targets: [], machine_types: [], budget_max_hourly_usd: null, limits: {} };
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

function shapeOf(mt) {
  return (POOLS.machine_types || []).find((m) => m.machine_type === mt) || null;
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
  const shape = shapeOf(ROW.machine_type);
  const rph = parseInt($("rp-rph").value, 10) || 0;
  const vcpu = parseInt($("rp-vcpu").value, 10) || 0;
  const ram = parseInt($("rp-ram").value, 10) || 0;
  const hosts = parseInt($("rp-max-hosts").value, 10) || 0;
  const auto = $("rp-auto").checked;
  const minHosts = parseInt($("rp-min").value, 10);
  const idle = parseInt($("rp-idle").value, 10);
  const problems = [];
  if (!shape) problems.push(`${ROW.machine_type} can't be launched as a runner host yet.`);
  const target = currentTarget();
  if (!/^[A-Za-z0-9][A-Za-z0-9-]{0,38}(\/[A-Za-z0-9._-]{1,100})?$/.test(target)) problems.push("Pick an organization, or type org or owner/repo.");
  if (rph < 1 || rph > 8) problems.push("Runners on each host must be 1 to 8.");
  if (hosts < 1 || hosts > 10) problems.push("Most hosts must be 1 to 10.");
  if (auto && !(minHosts >= 0 && minHosts < hosts)) problems.push(`Fewest hosts must be 0 to ${Math.max(hosts - 1, 0)} (fewer than the most hosts).`);
  if (auto && !(idle >= 5 && idle <= 240)) problems.push("Idle minutes must be 5 to 240.");
  if (shape && rph * vcpu > shape.vcpu) problems.push(`${rph} × ${vcpu} vCPU = ${rph * vcpu}, but the host has ${shape.vcpu}.`);
  if (shape && rph * ram > shape.usable_ram_gib) problems.push(`${rph} × ${ram} GiB = ${rph * ram} GiB, but the host has ${shape.usable_ram_gib} GiB free for runners (${shape.ram_gib} GiB minus ${POOLS.limits.host_reserved_ram_gib || 2} for the host).`);
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

async function openRunnerLaunch(row) {
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
  $("rp-max-hosts").value = "2";
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
  const fn = QUAL_ACTIVE_ROLE === "rust_builder" ? window.openBuilderLaunch : window.openRunnerLaunch;
  if (typeof fn === "function") fn(row);
  else showToast("Launching from this table isn't available for this role yet.");
};

// Grey out Launch on CI/CD rows whose machine type can't host runners yet.
function markLaunchable() {
  if (QUAL_ACTIVE_ROLE !== "cicd_runner" || !(POOLS.machine_types || []).length) return;
  const rows = window.QUAL_ROWS || [];
  // Header sorting reorders the rows, so read each button's own row index.
  document.querySelectorAll("#qualification-table-body button[onclick^='launchQualRow(']").forEach((b) => {
    const m = /launchQualRow\((\d+)\)/.exec(b.getAttribute("onclick") || "");
    const row = m ? rows[Number(m[1])] : null;
    if (!row) return;
    const ok = !!shapeOf(row.machine_type);
    b.disabled = !ok;
    b.title = ok ? "Launch runners on this machine" : `${row.machine_type} can't host runners yet`;
  });
}
const tbody = $("qualification-table-body");
if (tbody) new MutationObserver(markLaunchable).observe(tbody, { childList: true });

refreshPools().then(markLaunchable);
setInterval(() => { if (!document.hidden && $("rp-panel")) refreshPools(); }, 60000);
})();
"##;
