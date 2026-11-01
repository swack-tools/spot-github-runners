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
    h("p", { class: "hint", text: "Setup on the new VM takes about 15 to 25 minutes (it installs Docker and builds the Rust 1.99 image). The panel shows the address as soon as Compute Engine assigns it." }),
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
function cargoRemoteScript() {
  return [
    "#!/usr/bin/env bash",
    "# cargo-remote: build this Cargo project on a Spot Rust builder and copy the binary back.",
    "# Usage: cargo remote build [--release] [--bin NAME] [other cargo args]",
    "set -euo pipefail",
    "[[ \"${1:-}\" == \"remote\" ]] && shift",
    "HOST=\"${SPOT_BUILDER_HOST:-spot-builder}\"",
    "LAUNCHER=/usr/local/bin/oxidex-remote-build",
    "ROOT=/mnt/runner-data/remote-build",
    "[[ -f Cargo.toml ]] || { echo \"cargo-remote: run this in the folder that has Cargo.toml\" >&2; exit 64; }",
    "pkg=$(awk -F'\"' '/^\\[package\\]/{p=1; next} /^\\[/{p=0} p && /^name *=/{print $2; exit}' Cargo.toml)",
    "PROJECT=\"${SPOT_BUILDER_PROJECT:-${pkg:-$(basename \"$PWD\")}}\"",
    "[[ \"$PROJECT\" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$ ]] || { echo \"cargo-remote: set SPOT_BUILDER_PROJECT to a simple name\" >&2; exit 64; }",
    "",
    "# Find the profile directory and binary name the build writes.",
    "profile_dir=debug; bin=\"$PROJECT\"; triple=\"\"; prev=\"\"",
    "set_profile() { case \"$1\" in dev|test) profile_dir=debug ;; release|bench) profile_dir=release ;; *) profile_dir=\"$1\" ;; esac; }",
    "for arg in \"$@\"; do",
    "  case \"$prev\" in",
    "    --profile) set_profile \"$arg\" ;;",
    "    --bin) bin=\"$arg\" ;;",
    "    --target) triple=\"$arg\" ;;",
    "  esac",
    "  case \"$arg\" in",
    "    --release|-r) profile_dir=release ;;",
    "    --profile=*) set_profile \"${arg#--profile=}\" ;;",
    "    --bin=*) bin=\"${arg#--bin=}\" ;;",
    "    --target=*) triple=\"${arg#--target=}\" ;;",
    "  esac",
    "  prev=\"$arg\"",
    "done",
    "out=\"${triple:+$triple/}$profile_dir\"",
    "",
    "echo \"[cargo-remote] syncing $PWD to $HOST ($PROJECT)\" >&2",
    "ssh \"$HOST\" \"sudo -n $LAUNCHER prepare $PROJECT\" >/dev/null",
    "# -rltz, not -a: keep the builder's own permissions so the build container can write Cargo.lock.",
    "rsync -rltz --delete --exclude target/ --exclude .git/ ./ \"$HOST:$ROOT/sources/$PROJECT/\"",
    "# Builders compile with Rust 1.99.0 and need the project to say so in rust-toolchain.toml.",
    "if [[ ! -f rust-toolchain.toml ]]; then",
    "  ssh \"$HOST\" \"printf '[toolchain]\\\\nchannel = \\\"1.99.0\\\"\\\\n' > $ROOT/sources/$PROJECT/rust-toolchain.toml\"",
    "elif ! grep -Eq '^channel *= *\"1\\.99\\.0\"' rust-toolchain.toml; then",
    "  echo 'cargo-remote: builders use Rust 1.99.0; set channel = \"1.99.0\" in rust-toolchain.toml' >&2; exit 64",
    "fi",
    "echo \"[cargo-remote] cargo $* on $HOST\" >&2",
    "ssh \"$HOST\" \"sudo -n $LAUNCHER $PROJECT cargo$(printf ' %q' \"$@\")\"",
    "",
    "if [[ \"${1:-}\" == build ]]; then",
    "  mkdir -p \"target/$out\"",
    "  rsync -tz \"$HOST:$ROOT/targets/$PROJECT/$out/$bin\" \"target/$out/$bin\"",
    "  echo \"[cargo-remote] copied target/$out/$bin (built on Linux ${triple:-x86_64})\" >&2",
    "fi",
    "",
  ].join("\n");
}

function openConnect(b) {
  const host = b.external_ip || "<address appears once the VM is running>";
  const user = b.ssh_user || "<your user>";
  const sshConfig = [
    "Host spot-builder",
    `    HostName ${host}`,
    `    User ${user}`,
    "    IdentityFile ~/.ssh/id_ed25519",
    "    IdentitiesOnly yes",
    "    StrictHostKeyChecking accept-new",
    "    ControlMaster auto",
    "    ControlPath ~/.ssh/cm-%r@%h:%p",
    "    ControlPersist 10m",
    "",
  ].join("\n");
  const install = [
    "mkdir -p ~/.local/bin",
    "cat > ~/.local/bin/cargo-remote <<'CARGO_REMOTE'",
    cargoRemoteScript() + "CARGO_REMOTE",
    "chmod 0755 ~/.local/bin/cargo-remote",
    "# Put ~/.local/bin on your PATH (zsh), then open a new terminal:",
    "grep -q '.local/bin' ~/.zshrc || echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc",
  ].join("\n");
  const alias = [
    "[alias]",
    "rb = [\"remote\", \"build\", \"--release\"]",
    "rbd = [\"remote\", \"build\"]",
    "rt = [\"remote\", \"test\"]",
    "",
  ].join("\n");
  const tryIt = [
    "cargo new hello-spot && cd hello-spot",
    "cargo rb          # same as: cargo remote build --release",
    "file target/release/hello-spot   # ELF 64-bit LSB pie executable, x86-64",
  ].join("\n");
  const docs = safeDocsUrl(S.data && S.data.docs_url);
  const body = h("div", {}, [
    h("p", { class: "hint", text: `${b.name} in ${b.zone}. Your Mac sends the source, the builder compiles it in the pinned Rust 1.99 container, and the Linux x86_64 binary comes back into target/. You don't log in to the builder yourself.` }),
    h("h4", { text: "1. Tell SSH how to reach the builder (~/.ssh/config)" }),
    codeBlock(sshConfig),
    h("h4", { text: "2. Check that setup has finished" }),
    codeBlock("ssh spot-builder sudo -n /usr/local/bin/oxidex-remote-build status"),
    h("p", { class: "hint", text: "OK 0 … means ready. UNREADY means setup is still running, and \"Connection refused\" means SSH isn't up yet: wait a few minutes and try again." }),
    h("h4", { text: "3. Install the cargo-remote helper (paste this whole block into Terminal)" }),
    codeBlock(install),
    h("h4", { text: "4. Add short aliases (~/.cargo/config.toml for every project, or .cargo/config.toml in one)" }),
    codeBlock(alias),
    h("h4", { text: "5. Try it" }),
    codeBlock(tryIt),
    h("p", { class: "hint" }, [
      "A new builder gets a new address, so update HostName in ~/.ssh/config after you launch another one. If SSH warns that the host key changed (Spot addresses get reused), run ssh-keygen -R <address> and connect again. More options (idle-builder selection, just, VS Code): ",
      h("a", { href: docs, target: "_blank", rel: "noopener noreferrer", text: "client workstation setup guide" }), "."
    ]),
  ]);
  modal("Build on this builder from your Mac", body, []);
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
        "Spot VMs that compile your Rust projects. Launch one, then point cargo on your Mac at it (",
        h("a", { href: safeDocsUrl(d.docs_url), target: "_blank", rel: "noopener noreferrer", text: "setup guide" }),
        ")."
      ]),
    ]),
    h("div", { style: "display:flex; gap:8px; align-items:center;" }, [
      h("button", { class: "btn btn-sm btn-secondary", type: "button", onclick: refresh }, [S.loading ? "Loading…" : "Refresh"]),
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
        h("button", { class: "btn btn-sm", type: "button", onclick: () => openConnect(b) }, ["Use from my Mac"]),
        " ",
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
