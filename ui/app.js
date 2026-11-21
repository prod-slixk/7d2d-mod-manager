"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// Everything shown here comes from mod files or the web, so the DOM is only
// ever built through h(), which assigns text and never parses HTML.
function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2).toLowerCase(), v);
    else if (["value", "checked", "disabled", "selected", "hidden"].includes(k)) el[k] = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  append(el, kids);
  return el;
}

function append(el, kids) {
  for (const kid of kids.flat(Infinity)) {
    if (kid === null || kid === undefined || kid === false) continue;
    el.append(kid instanceof Node ? kid : String(kid));
  }
}

// Unlike replaceChildren(), this skips the null/false left by conditional kids.
function fill(el, ...kids) {
  el.replaceChildren();
  append(el, kids);
}

const ICONS = {
  mods: "M21 8 12 3 3 8v8l9 5 9-5V8ZM3 8l9 5 9-5M12 13v8",
  profiles: "m12 3 9 5-9 5-9-5 9-5ZM3 13l9 5 9-5M3 17.5l9 5 9-5",
  browse: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18ZM15.5 8.5l-2 5-5 2 2-5 5-2Z",
  worlds: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18ZM3.5 9h17M3.5 15h17M12 3c2.4 2.5 3.6 5.5 3.6 9s-1.2 6.5-3.6 9c-2.4-2.5-3.6-5.5-3.6-9S9.6 5.5 12 3Z",
  conflicts: "M12 3 2 20h20L12 3ZM12 10v5M12 17.5v.5",
  backups: "M3 4h18v5H3zM5 9v11h14V9M10 13h4",
  logs: "M3 4h18v16H3zM7 9l3 3-3 3M13 15h4",
  settings: "M4 6h9M17 6h3M4 12h3M11 12h9M4 18h11M19 18h1M15 4v4M9 10v4M17 16v4",
  search: "m20 20-4.5-4.5M17 10.5a6.5 6.5 0 1 1-13 0 6.5 6.5 0 0 1 13 0Z",
  folder: "M3 6h6l2 2h10v11H3V6Z",
  plus: "M12 5v14M5 12h14",
  refresh: "M20 5v5h-5M4 19v-5h5M19 10a7.5 7.5 0 0 0-13-3L4 9M5 14a7.5 7.5 0 0 0 13 3l2-2",
  link: "M14 4h6v6M20 4l-9 9M18 14v6H4V6h6",
  trash: "M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13",
};

function icon(name, size = 16) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", size);
  svg.setAttribute("height", size);
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", ICONS[name]);
  svg.append(path);
  return svg;
}

function btn(label, onclick, opts = {}) {
  return h("button", { class: `btn ${opts.kind || ""} ${opts.small ? "small" : ""}`, onclick, disabled: opts.disabled, title: opts.title, "aria-label": label ? null : opts.title },
    opts.icon && icon(opts.icon, opts.small ? 14 : 16), label);
}

const fmtSize = (n) => {
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1073741824) return `${(n / 1048576).toFixed(1)} MB`;
  return `${(n / 1073741824).toFixed(2)} GB`;
};
const fmtDate = (s) => new Date(s * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
const fmtMs = (ms) => fmtDate(ms / 1000);
const MODE_LABEL = { vanilla: "Vanilla", modded: "Modded" };
const otherMode = (m) => (m === "vanilla" ? "modded" : "vanilla");
const worldName = (s) => `${s.game} (${s.world})`;
const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
const nexusUrl = (id) => `https://www.nexusmods.com/7daystodie/mods/${id}`;

// ---- state -----------------------------------------------------------------

const S = {
  boot: null,
  view: "mods",
  mods: [],
  status: null,
  report: null,
  launch: { eac: "auto", args: "" },
  filter: "all",
  search: "",
  selected: null,
  detected: null,
  log: { files: [], path: null, data: null, level: "all", query: "", follow: true },
};

const instId = () => S.boot?.active;
const inst = () => S.boot?.instances.find((i) => i.id === S.boot.active);
const call = (cmd, args = {}) => invoke(cmd, { inst: instId(), ...args });

// ---- feedback --------------------------------------------------------------

function toast(message, kind = "ok") {
  const el = h("div", { class: `toast ${kind}`, role: kind === "error" ? "alert" : "status" }, message);
  const stack = document.getElementById("toasts");
  stack.append(el);
  while (stack.children.length > 3) stack.firstChild.remove();
  setTimeout(() => el.remove(), kind === "error" ? 9000 : 4500);
}

const fail = (e) => toast(String(e), "error");

// Built once and updated in place: progress arrives many times a second, and
// rebuilding would swallow clicks on the Cancel button.
let busyEl = null, busyText, busyCancelBtn, busyFill, busyCancellable = false;
function setBusy(message, fraction) {
  if (message === null) {
    busyEl?.remove();
    busyEl = null;
    return;
  }
  if (!busyEl) {
    busyText = h("span", {});
    busyCancelBtn = btn("Cancel", () => {
      busyCancelBtn.disabled = true;
      invoke("cancel_download");
    }, { small: true });
    busyFill = h("div", { class: "busy-fill" });
    busyEl = h("div", { class: "busy", role: "status" }, busyText, busyCancelBtn, h("div", { class: "busy-track" }, busyFill));
    document.body.append(busyEl);
  }
  busyCancelBtn.hidden = !busyCancellable;
  busyText.textContent = message;
  const indeterminate = fraction === null || fraction === undefined;
  busyFill.className = `busy-fill ${indeterminate ? "indet" : ""}`;
  busyFill.style.width = indeterminate ? "" : `${Math.round(fraction * 100)}%`;
}

async function withBusy(message, fn, { cancel = false } = {}) {
  busyCancellable = cancel;
  setBusy(message);
  try {
    return await fn();
  } finally {
    busyCancellable = false;
    setBusy(null);
  }
}

const CANCELLED = "Download cancelled.";
const downloadFail = (e) => (String(e) === CANCELLED ? toast("Download cancelled", "warn") : fail(e));

function dialog({ title, body, actions, onKey }) {
  return new Promise((resolve) => {
    const previous = document.activeElement;
    const close = (value) => {
      overlay.remove();
      document.removeEventListener("keydown", key);
      previous?.focus?.();
      resolve(value);
    };
    const key = (e) => {
      if (e.key === "Escape") close(undefined);
      else onKey?.(e, close);
    };
    const buttons = actions.map((a) => btn(a.label, () => close(a.get ? a.get() : a.value), { kind: a.kind }));
    const overlay = h("div", { class: "overlay", role: "dialog", "aria-modal": "true", "aria-label": title, onmousedown: (e) => e.target === overlay && close(undefined) },
      h("div", { class: "dialog" },
        h("div", { class: "dialog-head" }, title),
        h("div", { class: "dialog-body" }, body),
        h("div", { class: "dialog-foot" }, buttons)));
    document.getElementById("modal-root").append(overlay);
    document.addEventListener("keydown", key);
    (overlay.querySelector("input[type=text]") || buttons[buttons.length - 1]).focus();
  });
}

const confirmBox = (title, text, okLabel, danger) =>
  dialog({
    title,
    body: text,
    actions: [{ label: "Cancel", value: false }, { label: okLabel, value: true, kind: danger ? "solid-danger" : "primary" }],
  });

function promptBox(title, label, value = "", okLabel = "Save") {
  const input = h("input", { type: "text", value, spellcheck: "false" });
  return dialog({
    title,
    body: [h("label", { class: "field" }, label), input],
    actions: [{ label: "Cancel", value: undefined }, { label: okLabel, kind: "primary", get: () => input.value }],
    onKey: (e, close) => e.key === "Enter" && close(input.value),
  });
}

// ---- data ------------------------------------------------------------------

async function refreshMods({ conflicts = true } = {}) {
  if (!instId()) return;
  S.mods = await call("list_mods");
  if (conflicts) S.report = await call("check_conflicts");
  if (S.selected && !S.mods.some((m) => m.id === S.selected)) S.selected = null;
}

async function refreshStatus() {
  if (!instId() || document.hidden) return;
  try {
    S.status = await call("get_status");
    renderTop();
    S.status.play.mismatches.forEach(queueMismatch);
  } catch (_) { /* The next poll retries. */ }
}

// ---- play mode ---------------------------------------------------------------

const shownMismatches = new Set();
let mismatchQueue = Promise.resolve();

function queueMismatch(m) {
  const key = `${m.save}@${m.when}`;
  if (shownMismatches.has(key)) return;
  shownMismatches.add(key);
  mismatchQueue = mismatchQueue.then(() => showMismatch(m)).catch(fail);
}

async function showMismatch(m) {
  const expected = MODE_LABEL[m.expected].toLowerCase();
  const playedWith = m.played === "modded" ? "with mods on" : "with mods off";
  const choice = await dialog({
    title: `${MODE_LABEL[m.expected]} world played ${playedWith}`,
    body: [
      h("p", {}, `${worldName(m)} is one of your ${expected} worlds, but it changed while you were playing ${playedWith}.`),
      m.played === "modded"
        ? h("p", {}, "Modded items or blocks may now be in it, and it may not work in crossplay with your friend.")
        : h("p", {}, "Without its mods, modded items and blocks in this world may have been lost."),
      m.backup
        ? h("p", {}, "The manager backed up this world before that session started, so you can put it back.")
        : h("p", {}, "There's no backup of it from before that session."),
    ],
    actions: [
      { label: "Decide later", value: undefined },
      { label: `Keep, mark as ${MODE_LABEL[m.played].toLowerCase()}`, value: "relabel" },
      m.backup && { label: "Restore backup", value: "restore", kind: "primary" },
    ].filter(Boolean),
  });
  if (!choice) return;
  try {
    await withBusy(choice === "restore" ? "Restoring world" : "Saving", () => call("resolve_mismatch", { save: m.save, action: choice }));
    toast(choice === "restore" ? `${m.game} restored to before that session` : `${m.game} is now a ${MODE_LABEL[m.played].toLowerCase()} world`);
  } catch (e) {
    fail(e);
  }
  await afterChange();
}

async function switchMode(mode) {
  let ok = false;
  try {
    const r = await withBusy(mode === "vanilla" ? "Turning mods off" : "Turning mods back on", () => call("set_play_mode", { mode }));
    toast(mode === "vanilla"
      ? `Vanilla: ${plural(r.disabled, "mod")} turned off. Ready for crossplay.`
      : `Modded: ${plural(r.enabled, "mod")} turned back on`);
    if (r.missing.length) await showMissing(r.missing);
    ok = true;
  } catch (e) {
    fail(e);
  }
  await afterChange();
  return ok;
}

/// Asks before launching if the world played last belongs to the other mode.
async function confirmLaunchMode() {
  const play = S.status?.play;
  const last = play?.saves[0];
  if (!play || !last?.mode || last.mode === play.mode) return true;
  const mode = play.mode;
  const other = otherMode(mode);
  const matching = play.saves.filter((s) => s.mode === mode).slice(0, 4).map(worldName);
  const choice = await dialog({
    title: `Last world was ${other}`,
    body: [
      h("p", {}, `You last played ${worldName(last)}, which is a ${other} world. Mods are ${mode === "vanilla" ? "off" : "on"} right now, so don't load it this time.`),
      matching.length
        ? h("p", {}, `Your ${mode} worlds: ${matching.join(", ")}.`)
        : h("p", {}, `You don't have a ${mode} world yet. Start a new game from the game's menu.`),
    ],
    actions: [
      { label: "Cancel", value: undefined },
      { label: `Launch ${mode}`, value: "go" },
      { label: `Switch to ${MODE_LABEL[other]}`, value: "switch", kind: "primary" },
    ],
  });
  if (choice === "switch") return switchMode(other);
  return choice === "go";
}

async function reload() {
  await refreshMods();
  renderShell();
  await refreshStatus();
}

// Selecting a mod whose folder moved (toggle) keeps the row selected.
function reselect(old, enabled) {
  if (S.selected === old) S.selected = old.replace(/:(on|off):/, `:${enabled ? "on" : "off"}:`);
}

function issuesFor(folder) {
  return (S.report?.issues || []).filter((i) => i.parts.some((p) => p.folder === folder));
}

function topSeverity(folder) {
  const found = issuesFor(folder).map((i) => i.severity);
  return ["high", "medium", "low"].find((s) => found.includes(s));
}

const issueCount = (sev) => (S.report?.issues || []).filter((i) => i.severity === sev).length;

// ---- shell -----------------------------------------------------------------

const NAV = [
  ["mods", "Mods", "mods"],
  ["browse", "Browse Nexus", "browse"],
  ["profiles", "Profiles", "profiles"],
  ["worlds", "Worlds", "worlds"],
  ["conflicts", "Conflicts", "conflicts"],
  ["backups", "Backups", "backups"],
  ["logs", "Logs", "logs"],
  ["settings", "Settings", "settings"],
];

let topEl, navEl, viewEl, topDrawn;

function renderShell() {
  const root = document.getElementById("app");
  if (!S.boot.instances.length) {
    root.replaceChildren(welcome());
    return;
  }
  if (!S.mods) S.mods = [];
  topEl = h("header", { class: "topbar" });
  navEl = h("nav", { class: "nav" });
  viewEl = h("div", { class: "view" });
  root.replaceChildren(
    h("div", { class: "shell" },
      h("div", { class: "brand" },
        h("div", { class: "brand-mark" }, "7"),
        h("div", {}, h("div", { class: "brand-name" }, "Mod Manager"), h("div", { class: "brand-sub" }, "7 Days to Die"))),
      topEl,
      h("aside", { class: "side" }, navEl, h("div", { class: "side-foot" })),
      h("main", { class: "main" }, viewEl)));
  renderTop();
  renderNav();
  renderView();
}

function renderTop() {
  if (!topEl) return;
  const i = inst();
  if (!i) return;
  const st = S.status;
  // The status poll runs constantly. Redrawing unchanged would close the open dropdown.
  const drawn = JSON.stringify([st, S.launch, S.boot.active, S.boot.instances]);
  if (drawn === topDrawn && topEl.firstChild) return;
  topDrawn = drawn;
  const select = h("select", {
    "aria-label": "Game folder",
    onchange: async (e) => {
      await invoke("set_active", { inst: e.target.value });
      S.boot.active = e.target.value;
      S.selected = null;
      S.status = null;
      S.launch = await call("get_launch");
      await reload().catch(fail);
    },
  }, S.boot.instances.map((x) => h("option", { value: x.id, selected: x.id === i.id }, x.name)));

  const dlls = st?.eac_needed.length || 0;
  const isServer = i.kind === "server";
  const mode = st?.play.mode;
  let eacLine;
  if (isServer) eacLine = dlls && st?.server_eac ? "EAC on: DLL mods blocked" : "Runs startdedicated.bat";
  else if (mode === "vanilla") eacLine = "EAC on: crossplay ready";
  else if (S.launch.eac === "on") eacLine = "EAC forced on";
  else if (S.launch.eac === "off") eacLine = "EAC forced off";
  else eacLine = dlls ? `EAC off: ${plural(dlls, "DLL mod")}` : "EAC on: no DLL mods";

  const running = !!st?.running;
  const pathBad = st && !st.path_ok;
  topEl.replaceChildren(
    select,
    h("div", { class: "top-status" },
      h("span", {}, h("i", { class: `dot ${pathBad ? "warn" : running ? "live" : "ok"}` }),
        pathBad ? "Game folder missing" : running ? "Game running" : "Ready"),
      st?.game_version && h("span", {}, st.game_version),
      !isServer && st && !st.steam_running && h("span", {}, h("i", { class: "dot warn" }), "Steam closed")),
    h("div", { class: "spacer" }),
    mode && h("div", { class: "seg mode-seg", role: "group", "aria-label": "Play mode" },
      [["vanilla", "Vanilla"], ["modded", "Modded"]].map(([id, label]) => h("button", {
        class: `${id} ${mode === id ? "on" : ""}`,
        "aria-pressed": mode === id ? "true" : "false",
        disabled: running,
        title: id === "vanilla" ? "Turn every mod off for vanilla and crossplay" : "Turn your mods back on",
        onclick: () => mode !== id && switchMode(id),
      }, label))),
    h("button", { class: "launch", disabled: running || pathBad, onclick: launch },
      h("b", {}, isServer ? "Start server" : "Launch"), h("small", {}, eacLine)));
}

async function launch() {
  if (!(await confirmLaunchMode())) return;
  try {
    const r = await withBusy("Starting", () => call("launch_game"));
    if (r.protected) toast(`Backed up ${plural(r.protected, "world")} from your other mode first`);
    toast(`Started ${r.target}${inst().kind === "client" ? (r.eac ? " with EAC" : " without EAC") : ""}`);
    r.warnings.forEach((w) => toast(w, "warn"));
    setTimeout(refreshStatus, 2500);
  } catch (e) {
    fail(e);
  }
}

function renderNav() {
  if (!navEl) return;
  const high = issueCount("high");
  const medium = issueCount("medium");
  navEl.replaceChildren(...NAV.map(([id, label, ic]) => {
    let count = null;
    if (id === "mods") count = h("span", { class: "count" }, S.mods.filter((m) => m.enabled && !m.core && !m.problem).length);
    if (id === "conflicts" && high + medium) count = h("span", { class: `count ${high ? "high" : "medium"}` }, high + medium);
    return h("button", { class: S.view === id ? "on" : "", "aria-current": S.view === id ? "page" : null, onclick: () => go(id) }, icon(ic, 18), label, count);
  }));
}

function go(view) {
  S.view = view;
  renderNav();
  renderView();
}

function renderView() {
  if (!viewEl) return;
  const view = { mods: viewMods, browse: viewBrowse, profiles: viewProfiles, worlds: viewWorlds, conflicts: viewConflicts, backups: viewBackups, logs: viewLogs, settings: viewSettings }[S.view];
  Promise.resolve().then(view).catch(fail);
}

// ---- welcome ---------------------------------------------------------------

async function addFolder(path, name) {
  try {
    const added = await invoke("add_instance", { path, name });
    if (!S.boot.instances.some((i) => i.id === added.id)) S.boot.instances.push(added);
    S.boot.active = added.id;
    S.launch = await call("get_launch");
    S.view = "mods";
    await reload();
    toast(`Added ${added.name}`);
  } catch (e) {
    fail(e);
  }
}

async function browseFolder() {
  const path = await invoke("pick_game_folder");
  if (path) await addFolder(path);
}

function foundList(found) {
  const known = S.boot.instances.map((i) => i.path.toLowerCase());
  return found.filter((f) => !known.includes(f.path.toLowerCase())).map((f) =>
    h("div", { class: "found" },
      h("div", {}, h("b", {}, f.name), h("div", { class: "mono muted" }, f.path)),
      btn("Add", () => addFolder(f.path, f.name), { kind: "primary", small: true })));
}

function welcome() {
  const results = h("div", {});
  const detect = async () => {
    results.replaceChildren(h("p", { class: "muted" }, "Looking through your Steam libraries…"));
    const found = await invoke("detect_games");
    results.replaceChildren(
      ...(found.length ? foundList(found) : [h("p", { class: "muted" }, "No install found automatically. Browse to the folder that holds 7DaysToDie.exe or 7DaysToDieServer.exe.")]));
  };
  detect().catch(fail);
  return h("div", { class: "welcome" },
    h("div", { class: "welcome-box" },
      h("div", { class: "welcome-inner" },
        h("h1", {}, "Find your game"),
        h("p", { class: "lede" }, "Pick the 7 Days to Die or dedicated server folder you want to manage."),
        results,
        h("div", { class: "actions", style: "margin-top:18px" },
          btn("Browse for folder", browseFolder, { kind: "primary", icon: "folder" }),
          btn("Search again", () => detect().catch(fail), { icon: "refresh" })))));
}

// ---- mods ------------------------------------------------------------------

const FILTERS = [["all", "All"], ["on", "Enabled"], ["off", "Disabled"], ["issues", "Issues"], ["updates", "Updates"]];

function visibleMods() {
  const q = S.search.trim().toLowerCase();
  return S.mods
    .filter((m) => {
      if (S.filter === "on") return m.enabled;
      if (S.filter === "off") return !m.enabled;
      if (S.filter === "issues") return !!m.problem || !!topSeverity(m.folder);
      if (S.filter === "updates") return m.update;
      return true;
    })
    .filter((m) => !q || [m.title, m.folder, m.author].some((s) => s.toLowerCase().includes(q)));
}

let listEl, detailEl;

function viewMods() {
  listEl = h("div", { class: "list" });
  detailEl = h("aside", { class: "detail" });
  viewEl.replaceChildren(
    h("div", { class: "toolbar" },
      btn("Install", installFromPicker, { kind: "primary", icon: "plus" }),
      h("div", { class: "search" }, icon("search"), h("input", {
        type: "text", placeholder: "Search mods", "aria-label": "Search mods", value: S.search,
        oninput: (e) => { S.search = e.target.value; renderList(); },
      })),
      h("div", { class: "seg", role: "group", "aria-label": "Filter" }, FILTERS.map(([id, label]) =>
        h("button", { class: S.filter === id ? "on" : "", onclick: () => { S.filter = id; viewMods(); } }, label))),
      h("div", { class: "spacer" }),
      btn("Enable all", () => bulk(true), { small: true, title: "Enable every mod in this list" }),
      btn("Disable all", () => bulk(false), { small: true, title: "Disable every mod in this list" }),
      btn("Updates", checkUpdates, { small: true, icon: "refresh", title: "Check Nexus for new versions" }),
      btn("", () => call("open_mods_root").catch(fail), { small: true, icon: "folder", title: "Open the Mods folder" })),
    h("div", { class: "mods-split" }, listEl, detailEl));
  renderList();
  renderDetail();
}

function renderList() {
  const items = visibleMods();
  if (!S.mods.length) {
    listEl.replaceChildren(h("div", { class: "empty" },
      h("b", {}, "No mods yet"),
      h("div", {}, "Drag a mod archive anywhere onto this window."),
      h("div", { class: "actions" }, btn("Install from file", installFromPicker, { kind: "primary", icon: "plus" }))));
    return;
  }
  if (!items.length) {
    listEl.replaceChildren(h("div", { class: "empty" }, h("b", {}, "Nothing here"), h("div", {}, "No mods match this search or filter.")));
    return;
  }
  listEl.replaceChildren(
    h("div", { class: "list-head" }, h("span", {}), h("span", {}, "Mod"), h("span", {}, "Version"), h("span", { class: "num", title: "Load order" }, "#"), h("span", { class: "num col-size" }, "Size")),
    ...items.map(modRow));
}

function modBadges(m) {
  const sev = topSeverity(m.folder);
  return [
    m.core && h("span", { class: "badge" }, "Core"),
    m.problem && h("span", { class: "badge high" }, "Broken"),
    m.update && h("span", { class: "badge amber" }, "Update"),
    m.has_dll && !m.core && h("span", { class: "badge blue", title: "Contains code, so the game has to run without EAC" }, "DLL"),
    !m.problem && m.enabled && sev && sev !== "low" && h("span", { class: `badge ${sev}` }, sev === "high" ? "Conflict" : "Overlap"),
  ];
}

function modRow(m) {
  const toggle = h("label", { class: "toggle", title: m.core ? "Ships with the game" : m.enabled ? "Disable" : "Enable", onclick: (e) => e.stopPropagation() },
    h("input", {
      type: "checkbox", checked: m.enabled, disabled: m.core, "aria-label": `Enable ${m.title}`,
      onchange: () => toggleMod(m, !m.enabled),
    }), h("i", {}));
  return h("div", { class: `row ${S.selected === m.id ? "sel" : ""} ${m.enabled ? "" : "off"}`, onclick: () => selectMod(m.id) },
    toggle,
    h("div", {},
      h("div", { class: "row-title" }, h("span", { class: "row-title-text" }, m.title), modBadges(m)),
      h("div", { class: "row-sub" }, m.author ? `${m.author}  ·  ${m.folder}` : m.folder)),
    h("div", { class: "ver" }, m.version || "—"),
    h("div", { class: "num" }, m.load_index ?? "—"),
    h("div", { class: "num col-size" }, fmtSize(m.size)));
}

function selectMod(id) {
  S.selected = id;
  renderList();
  renderDetail();
}

async function toggleMod(m, enabled) {
  try {
    await call("set_mods_enabled", { ids: [m.id], enabled });
    reselect(m.id, enabled);
  } catch (e) {
    fail(e);
  }
  await afterChange();
}

async function bulk(enabled) {
  const ids = visibleMods().filter((m) => !m.core && !m.problem && m.enabled !== enabled).map((m) => m.id);
  if (!ids.length) return;
  try {
    await call("set_mods_enabled", { ids, enabled });
    toast(`${enabled ? "Enabled" : "Disabled"} ${plural(ids.length, "mod")}`);
  } catch (e) {
    fail(e);
  }
  await afterChange();
}

async function afterChange() {
  try {
    await refreshMods();
    // Views read the status too, so it has to be current before they redraw.
    await refreshStatus();
    renderNav();
    renderView();
  } catch (e) {
    fail(e);
  }
}

async function checkUpdates() {
  try {
    const r = await withBusy("Checking Nexus for updates", () => call("nexus_check_updates"));
    if (!r.checked && !r.errors.length) toast("No mods are linked to Nexus yet.", "warn");
    else if (r.updates.length) toast(`Updates available: ${r.updates.join(", ")}`);
    else toast(`All ${plural(r.checked, "linked mod")} up to date`);
    r.errors.slice(0, 3).forEach((x) => toast(x, "error"));
    await afterChange();
  } catch (e) {
    fail(e);
  }
}

function renderDetail() {
  const m = S.mods.find((x) => x.id === S.selected);
  if (!m) {
    const dlls = S.mods.filter((x) => x.enabled && x.has_dll && !x.core).length;
    const total = S.mods.reduce((n, x) => n + x.size, 0);
    fill(detailEl,
      h("h2", {}, "Overview"),
      h("dl", { class: "meta" },
        h("dt", {}, "Enabled"), h("dd", {}, S.mods.filter((x) => x.enabled && !x.core && !x.problem).length),
        h("dt", {}, "Disabled"), h("dd", {}, S.mods.filter((x) => !x.enabled).length),
        h("dt", {}, "DLL mods"), h("dd", {}, dlls),
        h("dt", {}, "Disk"), h("dd", {}, fmtSize(total))),
      h("p", { class: "muted" }, "Mods load in folder-name order. Where two mods change the same value, the later one wins. The # column shows each mod's position."),
      h("p", { class: "muted" }, "Select a mod for details."));
    return;
  }

  const problem = m.problem === "nested"
    ? h("div", { class: "callout high" }, h("b", {}, "Installed one folder too deep"), "The game won't see this mod until it's moved up a level.",
        h("div", {}, btn("Fix folder", async () => {
          try { await call("fix_nested", { id: m.id }); toast("Folder fixed"); } catch (e) { fail(e); }
          S.selected = null;
          await afterChange();
        }, { kind: "primary", small: true })))
    : m.problem === "missing"
      ? h("div", { class: "callout high" }, h("b", {}, "No ModInfo.xml"), "The game ignores this folder because it has no ModInfo.xml.")
      : null;

  const issues = m.enabled ? issuesFor(m.folder) : [];
  fill(detailEl,
    h("div", { class: "detail-title" }, m.title),
    h("div", { class: "badges" }, h("span", { class: `badge ${m.enabled ? "green" : ""}` }, m.enabled ? "Enabled" : "Disabled"), modBadges(m)),
    problem,
    m.update && h("div", { class: "callout medium" }, h("b", {}, `Version ${m.nexus.latest} is out`), "Open the Nexus page and use Mod Manager Download to update.",
      h("div", {}, btn("Open Nexus page", () => call("open_url", { url: nexusUrl(m.nexus.mod_id) }).catch(fail), { small: true, icon: "link" }))),
    m.legacy && h("div", { class: "callout low" }, "This mod uses the old ModInfo layout, so it was likely built for an older game version."),
    m.malformed && h("div", { class: "callout high" }, "ModInfo.xml has broken XML. The game may refuse to load this mod."),
    m.description && h("p", { class: "desc" }, m.description),
    h("dl", { class: "meta" },
      h("dt", {}, "Version"), h("dd", {}, m.version || "Not stated"),
      m.author && [h("dt", {}, "Author"), h("dd", {}, m.author)],
      h("dt", {}, "Folder"), h("dd", {}, m.folder),
      h("dt", {}, "Location"), h("dd", {}, m.root === "user" ? "AppData" : "Game folder"),
      m.load_index !== null && [h("dt", {}, "Load #"), h("dd", {}, m.load_index)],
      h("dt", {}, "Size"), h("dd", {}, `${fmtSize(m.size)}, ${plural(m.files, "file")}`)),
    h("div", { class: "actions" },
      btn("Open folder", () => call("open_mod_folder", { id: m.id }).catch(fail), { small: true, icon: "folder" }),
      /^https?:\/\//i.test(m.website) && btn("Website", () => call("open_url", { url: m.website }).catch(fail), { small: true, icon: "link" }),
      m.nexus
        ? [btn("Nexus", () => call("open_url", { url: nexusUrl(m.nexus.mod_id) }).catch(fail), { small: true, icon: "link" }),
           btn("Unlink", async () => { await call("nexus_unlink_mod", { folder: m.folder }).catch(fail); await afterChange(); }, { small: true })]
        : btn("Link Nexus", () => linkNexus(m), { small: true, title: "Connect this mod to its Nexus page for update checks" }),
      !m.core && btn("Uninstall", () => uninstall(m), { small: true, kind: "danger", icon: "trash" })),
    issues.length > 0 && h("div", { class: "detail-section" },
      h("h3", {}, plural(issues.length, "issue")),
      issues.slice(0, 6).map((i) => h("div", { class: "mini-issue" }, h("span", { class: `badge ${i.severity}` }, i.severity), " ", i.title,
        i.file && h("div", { class: "mono muted" }, i.file))),
      btn("See all conflicts", () => go("conflicts"), { small: true })));
}

async function linkNexus(m) {
  const reference = await promptBox("Link to Nexus", "Nexus mod page URL or mod number", "", "Link");
  if (!reference) return;
  try {
    await call("nexus_link_mod", { folder: m.folder, reference });
    toast("Linked to Nexus");
    await afterChange();
  } catch (e) {
    fail(e);
  }
}

async function uninstall(m) {
  const yes = await confirmBox(`Uninstall ${m.title}?`, "The folder is deleted from disk. A copy is kept in Backups first, so you can restore it.", "Uninstall", true);
  if (!yes) return;
  try {
    await withBusy(`Removing ${m.title}`, () => call("uninstall_mod", { id: m.id }));
    toast(`Removed ${m.title}`);
  } catch (e) {
    fail(e);
  }
  await afterChange();
}

// ---- installing ------------------------------------------------------------

async function installFromPicker() {
  const paths = await invoke("pick_mod_files");
  if (paths.length) await installPaths(paths);
}

async function installPaths(paths) {
  if (!instId()) return;
  try {
    const staged = await withBusy("Unpacking", () => call("prepare_install", { paths }));
    await reviewInstall(staged);
  } catch (e) {
    fail(e);
  }
}

async function reviewInstall(staged) {
  const boxes = new Map();
  const rows = staged.candidates.map((c) => {
    const box = h("input", { type: "checkbox", checked: true, "aria-label": c.title });
    boxes.set(c.id, box);
    const change = c.exists ? (c.existing_version && c.version && c.existing_version !== c.version ? `Update ${c.existing_version} → ${c.version}` : "Replace") : "New";
    return h("label", { class: "cand" }, box,
      h("div", { class: "cand-main" },
        h("div", {}, h("b", {}, c.title), " ", h("span", { class: `badge ${c.exists ? "yellow" : "green"}` }, change), " ", c.has_dll && h("span", { class: "badge blue" }, "DLL")),
        h("div", { class: "cand-sub" }, [c.version && `v${c.version}`, c.author, c.folder].filter(Boolean).join("  ·  "))));
  });
  const replaced = staged.candidates.some((c) => c.exists);
  const pack = staged.import;
  const result = await dialog({
    title: pack ? `Import ${pack.name}` : "Review install",
    body: [
      pack && h("p", {}, `Creates a profile named "${pack.name}" with ${plural(pack.mods.length, "mod")}.`),
      !staged.candidates.length && pack && h("p", {}, "This pack lists mods but doesn't include their files."),
      staged.errors.map((x) => h("div", { class: "callout high" }, x)),
      rows,
      replaced && h("p", {}, "Existing versions are backed up before being replaced."),
    ],
    actions: [
      { label: "Cancel", value: undefined },
      { label: pack ? "Import" : "Install", kind: "primary", get: () => [...boxes].filter(([, b]) => b.checked).map(([id]) => id) },
    ],
  });
  if (!result) {
    await invoke("cancel_install", { staging: staged.id });
    return;
  }
  try {
    const r = await withBusy("Installing", () => call("commit_install", { staging: staged.id, selected: result }));
    if (r.installed.length) toast(`Installed ${r.installed.join(", ")}`);
    if (r.profile) toast(`Created profile ${r.profile}`);
    r.failed.forEach((x) => toast(x, "error"));
    if (r.missing.length) await showMissing(r.missing);
  } catch (e) {
    fail(e);
  }
  await afterChange();
}

function showMissing(missing) {
  return dialog({
    title: "Missing mods",
    body: [
      h("p", {}, "These mods are in the profile but aren't installed:"),
      missing.map((m) => h("div", { class: "found" },
        h("div", {}, h("b", {}, m.name), h("div", { class: "cand-sub" }, m.version && `v${m.version}`)),
        m.nexus_id && btn("Nexus", () => call("open_url", { url: nexusUrl(m.nexus_id) }).catch(fail), { small: true, icon: "link" }))),
    ],
    actions: [{ label: "Close", value: true, kind: "primary" }],
  });
}

let nxmBusy = false;
async function handleNxm() {
  if (nxmBusy) return;
  nxmBusy = true;
  try {
    for (const url of await invoke("take_pending_nxm")) {
      if (!instId()) {
        toast("Add a game folder before downloading from Nexus.", "warn");
        continue;
      }
      try {
        const staged = await withBusy("Contacting Nexus", () => call("nexus_download", { url }), { cancel: true });
        await reviewInstall(staged);
      } catch (e) {
        downloadFail(e);
      }
    }
  } finally {
    nxmBusy = false;
  }
}

// ---- browse Nexus ----------------------------------------------------------

const SORTS = [["endorsements", "Most endorsed"], ["downloads", "Most downloaded"], ["updated", "Recently updated"], ["newest", "Newest"]];
const B = { query: "", sort: "endorsements", mods: [], total: 0, loading: false, error: null, selected: null, files: {}, token: 0, timer: null };
let browseGrid, browseDetail, browseCount;

const fmtCount = (n) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${(n / 1e3).toFixed(1)}K` : String(n));
const fmtDay = (iso) => (iso ? new Date(iso).toLocaleDateString([], { dateStyle: "medium" }) : "");
const installedFrom = (id) => S.mods.find((m) => m.nexus?.mod_id === id);
const nexusFileUrl = (modId, fileId) => `https://www.nexusmods.com/7daystodie/mods/${modId}?tab=files&file_id=${fileId}&nmm=1`;

function viewBrowse() {
  const input = h("input", {
    type: "text", placeholder: "Search 7 Days to Die mods on Nexus", "aria-label": "Search Nexus Mods", value: B.query, spellcheck: "false",
    oninput: (e) => {
      B.query = e.target.value;
      clearTimeout(B.timer);
      B.timer = setTimeout(() => runSearch(true), 450);
    },
    onkeydown: (e) => {
      if (e.key !== "Enter") return;
      clearTimeout(B.timer);
      B.query = e.target.value;
      runSearch(true);
    },
  });
  browseGrid = h("div", { class: "list" });
  browseDetail = h("aside", { class: "detail" });
  browseCount = h("span", { class: "muted" });
  viewEl.replaceChildren(
    h("div", { class: "toolbar" },
      h("div", { class: "search", style: "max-width:420px" }, icon("search"), input),
      h("div", { class: "seg", role: "group", "aria-label": "Sort" }, SORTS.map(([id, label]) =>
        h("button", { class: B.sort === id ? "on" : "", onclick: () => { B.sort = id; viewBrowse(); runSearch(true); } }, label))),
      h("div", { class: "spacer" }),
      browseCount),
    h("div", { class: "mods-split" }, browseGrid, browseDetail));
  renderBrowse();
  renderBrowseDetail();
  if (!B.mods.length && !B.loading && !B.error) runSearch(true);
}

async function runSearch(reset) {
  const token = ++B.token;
  if (reset) B.mods = [];
  B.loading = true;
  B.error = null;
  if (S.view === "browse") renderBrowse();
  try {
    const page = await invoke("nexus_search", { query: B.query, sort: B.sort, offset: B.mods.length });
    if (token !== B.token) return;
    // A page can overlap the last one when Nexus reorders between requests.
    const seen = new Set(B.mods.map((m) => m.id));
    B.mods = B.mods.concat(page.mods.filter((m) => !seen.has(m.id)));
    B.total = page.total;
  } catch (e) {
    if (token !== B.token) return;
    B.error = String(e);
  }
  B.loading = false;
  if (S.view === "browse") renderBrowse();
}

function browseCard(m) {
  const pick = () => selectNexusMod(m);
  return h("div", {
    class: `bcard ${B.selected?.id === m.id ? "sel" : ""}`, role: "button", tabindex: "0", "aria-label": m.name,
    onclick: pick, onkeydown: (e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), pick()),
  },
    h("div", { class: "bthumb" }, m.picture ? h("img", { src: m.picture, alt: "", loading: "lazy" }) : h("span", {}, "No image")),
    h("div", { class: "bbody" },
      h("div", { class: "btitle" }, m.name),
      h("div", { class: "bsub" }, m.author),
      h("div", { class: "bstats" }, `${fmtCount(m.endorsements)} endorsements  ·  ${fmtCount(m.downloads)} downloads`),
      h("div", { class: "bfoot" },
        installedFrom(m.id) && h("span", { class: "badge green" }, "Installed"),
        h("span", { class: "bdate" }, `Updated ${fmtDay(m.updated)}`))));
}

function renderBrowse() {
  if (!browseGrid?.isConnected) return;
  browseCount.textContent = B.total ? `${B.total.toLocaleString()} mods` : "";
  if (B.error && !B.mods.length) {
    fill(browseGrid, h("div", { class: "empty" }, h("b", {}, "Couldn't reach Nexus"), h("div", {}, B.error),
      h("div", { class: "actions" }, btn("Try again", () => runSearch(true), { icon: "refresh" }))));
    return;
  }
  if (!B.mods.length) {
    fill(browseGrid, h("div", { class: "empty" }, B.loading ? h("b", {}, "Searching…") : [h("b", {}, "No mods found"), h("div", {}, "Try a shorter or different search.")]));
    return;
  }
  fill(browseGrid,
    h("div", { class: "bgrid" }, B.mods.map(browseCard)),
    h("div", { class: "bmore" },
      B.error && h("div", { class: "muted" }, B.error),
      B.mods.length < B.total && btn(B.loading ? "Loading…" : "Load more", () => runSearch(false), { disabled: B.loading })));
}

async function selectNexusMod(m) {
  B.selected = m;
  renderBrowse();
  renderBrowseDetail();
  if (B.files[m.id]) return;
  try {
    B.files[m.id] = await invoke("nexus_files", { modId: m.id });
  } catch (e) {
    B.files[m.id] = { error: String(e) };
  }
  if (S.view === "browse" && B.selected?.id === m.id) renderBrowseDetail();
}

function renderBrowseDetail() {
  if (!browseDetail?.isConnected) return;
  const m = B.selected;
  if (!m) {
    fill(browseDetail,
      h("h2", {}, "Browse Nexus"),
      h("p", { class: "muted" }, "Search and sorting work without an account. Pick a mod to see its files."),
      h("p", { class: "muted" }, "Many mods were made for older game versions. Check a mod's page says it supports your version before installing it."),
      h("p", { class: "muted" }, "Mods turn off crossplay. Use the Vanilla switch to play with console friends."));
    return;
  }
  const files = B.files[m.id];
  const have = installedFrom(m.id);
  fill(browseDetail,
    m.picture && h("div", { class: "dthumb" }, h("img", { src: m.picture, alt: "" })),
    h("div", { class: "detail-title" }, m.name),
    h("div", { class: "badges" },
      have ? h("span", { class: "badge green" }, `Installed ${have.version}`.trim()) : null,
      m.version && h("span", { class: "badge" }, `Nexus v${m.version}`)),
    m.summary && h("p", { class: "desc" }, m.summary),
    h("dl", { class: "meta" },
      h("dt", {}, "Author"), h("dd", {}, m.author),
      h("dt", {}, "Updated"), h("dd", {}, fmtDay(m.updated)),
      h("dt", {}, "Endorsed"), h("dd", {}, m.endorsements.toLocaleString()),
      h("dt", {}, "Downloads"), h("dd", {}, m.downloads.toLocaleString())),
    h("div", { class: "actions" }, btn("Open on Nexus", () => call("open_url", { url: nexusUrl(m.id) }).catch(fail), { small: true, icon: "link" })),
    h("div", { class: "detail-section" },
      h("h3", {}, "Files"),
      !files && h("p", { class: "muted" }, "Loading files…"),
      files?.error && h("p", { class: "muted" }, files.error),
      Array.isArray(files) && !files.length && h("p", { class: "muted" }, "No current files. Check the mod page."),
      Array.isArray(files) && files.map((f) => h("div", { class: "nfile" },
        h("div", { class: "nfile-main" },
          h("div", {}, h("b", {}, f.name), " ", h("span", { class: `badge ${f.category === "MAIN" ? "amber" : ""}` }, f.category.toLowerCase())),
          h("div", { class: "cand-sub" }, [f.version && `v${f.version}`, fmtSize(f.size), f.date && fmtDate(f.date)].filter(Boolean).join("  ·  "))),
        btn("Download", () => nexusGet(m, f), { small: true, kind: f.category === "MAIN" ? "primary" : "" })))));
}

async function nexusGet(m, f) {
  if (!instId()) return;
  if (!S.boot.has_key) {
    const open = await dialog({
      title: "Connect Nexus first",
      body: [h("p", {}, "Downloads need your free Nexus API key, so the manager can fetch files for you. Add it once in Settings.")],
      actions: [{ label: "Cancel", value: false }, { label: "Open Settings", value: true, kind: "primary" }],
    });
    if (open) go("settings");
    return;
  }
  if (S.boot.nexus_user?.premium) {
    try {
      const staged = await withBusy(`Downloading ${f.name}`, () => call("nexus_download_file", { modId: m.id, fileId: f.id }), { cancel: true });
      await reviewInstall(staged);
    } catch (e) {
      downloadFail(e);
    }
    return;
  }
  // Free accounts: Nexus only hands files to managers through its own download page.
  const handler = await invoke("nexus_handler_status");
  if (handler.state !== "this") {
    const ok = await dialog({
      title: "One-time setup",
      body: [
        h("p", {}, "Free Nexus accounts download through the site's Mod Manager Download button. Let this app receive those links, and each file comes straight back here for you to review."),
        handler.state === "other" && h("p", {}, "Another app, such as Vortex, receives them right now. This switches them to this app."),
      ],
      actions: [{ label: "Cancel", value: false }, { label: "Use this app", value: true, kind: "primary" }],
    });
    if (!ok) return;
    try {
      await invoke("nexus_set_handler", { on: true });
    } catch (e) {
      fail(e);
      return;
    }
  }
  try {
    await call("open_url", { url: nexusFileUrl(m.id, f.id) });
    toast("Nexus opened in your browser. Click Slow download there, and the file comes back here to review.");
  } catch (e) {
    fail(e);
  }
}

// ---- profiles --------------------------------------------------------------

async function viewProfiles() {
  const { profiles, active } = await call("list_profiles");
  if (S.view !== "profiles") return;
  const current = S.mods.filter((m) => m.enabled && !m.core && !m.problem).map((m) => m.folder.toLowerCase());

  const differs = (p) => {
    const want = p.mods.map((m) => m.folder.toLowerCase());
    return want.filter((f) => !current.includes(f)).length + current.filter((f) => !want.includes(f)).length;
  };

  const card = (p) => {
    const diff = differs(p);
    const isActive = p.id === active;
    return h("div", { class: `card ${isActive ? "active" : ""}` },
      h("div", { class: "card-row" },
        h("div", { style: "flex:1;min-width:200px" },
          h("div", { class: "card-title" }, p.name, " ", isActive && h("span", { class: "badge amber" }, "Active")),
          h("div", { class: "card-sub" }, `${plural(p.mods.length, "mod")}  ·  ${diff ? `${plural(diff, "difference")} from what's enabled now` : "Matches what's enabled now"}`)),
        h("div", { class: "actions" },
          btn("Apply", () => applyProfile(p), { kind: diff ? "primary" : "", small: true, disabled: !diff && isActive }),
          btn("Update", () => updateProfile(p), { small: true, title: "Replace this profile's list with what's enabled now" }),
          btn("Export", () => exportProfile(p), { small: true }),
          btn("Rename", () => renameProfile(p), { small: true }),
          btn("", () => deleteProfile(p), { small: true, kind: "danger", icon: "trash", title: "Delete profile" }))));
  };

  viewEl.replaceChildren(h("div", { class: "page" }, h("div", { class: "page-narrow" },
    h("div", { class: "page-head" }, h("h1", {}, "Profiles"), h("div", { class: "spacer" }),
      btn("Import modpack", importPack, {}),
      btn("Save current as new", newProfile, { kind: "primary", icon: "plus" })),
    h("p", { class: "lede" }, "A profile remembers which mods are enabled. Applying one moves the others to Mods_Disabled without deleting anything."),
    profiles.length ? profiles.map(card) : h("div", { class: "empty" }, h("b", {}, "No profiles yet"), h("div", {}, "Set up your mods, then save them as a profile.")))));
}

const profileAction = (fn) => async (...args) => {
  try {
    await fn(...args);
  } catch (e) {
    fail(e);
  }
  await afterChange();
};

const newProfile = profileAction(async () => {
  const name = await promptBox("New profile", "Profile name");
  if (name?.trim()) await call("save_profile", { name });
});

const applyProfile = profileAction(async (p) => {
  const r = await withBusy(`Applying ${p.name}`, () => call("apply_profile", { id: p.id }));
  toast(`${p.name} applied: ${r.enabled} enabled, ${r.disabled} disabled`);
  if (r.missing.length) await showMissing(r.missing);
});

const updateProfile = profileAction(async (p) => {
  if (await confirmBox(`Update ${p.name}?`, "This replaces the profile's mod list with what's enabled right now.", "Update")) {
    await call("update_profile", { id: p.id });
    toast("Profile updated");
  }
});

const renameProfile = profileAction(async (p) => {
  const name = await promptBox("Rename profile", "Profile name", p.name);
  if (name?.trim()) await call("rename_profile", { id: p.id, name });
});

const deleteProfile = profileAction(async (p) => {
  if (await confirmBox(`Delete ${p.name}?`, "Only the profile is deleted. Your mods stay installed.", "Delete", true)) await call("delete_profile", { id: p.id });
});

async function exportProfile(p) {
  const files = h("input", { type: "checkbox", checked: true });
  const go = await dialog({
    title: `Export ${p.name}`,
    body: [
      h("label", { class: "check" }, files, h("span", { style: "color:var(--text)" }, "Include the mod files")),
      h("p", {}, "With files, friends can install everything from one .7dmp file. Without them, the pack is a small list with Nexus links."),
    ],
    actions: [{ label: "Cancel", value: false }, { label: "Choose location", value: true, kind: "primary" }],
  });
  if (!go) return;
  try {
    const saved = await withBusy("Exporting", () => call("export_profile", { id: p.id, files: files.checked }));
    if (saved) toast("Modpack exported");
  } catch (e) {
    fail(e);
  }
}

async function importPack() {
  try {
    const staged = await withBusy("Opening modpack", () => call("import_modpack"));
    if (staged) await reviewInstall(staged);
  } catch (e) {
    fail(e);
  }
}

// ---- worlds ----------------------------------------------------------------

async function viewWorlds() {
  if (!S.status) await refreshStatus();
  if (S.view !== "worlds") return;
  const play = S.status?.play;
  const running = !!S.status?.running;
  const mode = play?.mode;

  const setTag = async (s, tag) => {
    try {
      await call("set_save_mode", { save: s.rel, mode: tag });
    } catch (e) {
      fail(e);
    }
    await refreshStatus();
    viewWorlds();
  };

  const worldCard = (s) => {
    const m = play.mismatches.find((x) => x.save === s.rel);
    return h("div", { class: `card world ${s.mismatch ? "issue high" : ""}` },
      h("div", { class: "card-row" },
        h("div", { style: "flex:1;min-width:200px" },
          h("div", { class: "card-title", style: "font-size:19px" }, s.game),
          h("div", { class: "card-sub" }, [s.world, `Last played ${fmtMs(s.played)}`, s.backup && "Backed up"].filter(Boolean).join("  ·  "))),
        h("div", { class: "seg tag-seg", role: "group", "aria-label": `Mode for ${s.game}` },
          [["vanilla", "Vanilla"], ["modded", "Modded"]].map(([id, label]) => h("button", {
            class: `${id} ${s.mode === id ? "on" : ""}`,
            "aria-pressed": s.mode === id ? "true" : "false",
            onclick: () => setTag(s, s.mode === id ? null : id),
          }, label)))),
      m && h("div", { class: "callout high", style: "margin:12px 0 0" },
        h("b", {}, `Played ${m.played === "modded" ? "with mods on" : "with mods off"} on ${fmtDate(m.when)}`),
        "This world changed in the wrong mode.",
        h("div", { class: "actions" },
          m.backup && btn("Restore backup", () => showMismatch(m), { small: true, kind: "primary", disabled: running }),
          !m.backup && btn("Review", () => showMismatch(m), { small: true }))));
  };

  const untagged = play?.saves.filter((s) => !s.mode).length || 0;
  viewEl.replaceChildren(h("div", { class: "page" }, h("div", { class: "page-narrow" },
    h("div", { class: "page-head" }, h("h1", {}, "Worlds")),
    h("p", { class: "lede" }, "Mark each world as vanilla or modded. The manager notes which worlds change while you play. It warns you before launching if the last world doesn't match your mods, and backs up the other mode's worlds first."),
    mode && h("div", { class: `card mode-card ${mode}` }, h("div", { class: "card-row" },
      h("div", { style: "flex:1;min-width:220px" },
        h("div", { class: "card-title" }, mode === "vanilla" ? "Mods are off" : "Mods are on"),
        h("div", { class: "card-sub" }, mode === "vanilla"
          ? "Vanilla play with EAC on, ready for crossplay with console players."
          : "Load only your modded worlds. Crossplay isn't available with mods.")),
      btn(mode === "vanilla" ? "Switch to Modded" : "Switch to Vanilla", () => switchMode(otherMode(mode)), { kind: "primary", disabled: running }))),
    untagged > 0 && h("div", { class: "callout medium" }, h("b", {}, `${plural(untagged, "world")} not marked yet`),
      "Tag existing worlds once. New worlds are tagged automatically the first time you play them."),
    !play?.saves_dir && h("p", { class: "muted" }, "The game's Saves folder couldn't be found."),
    play?.saves.length
      ? play.saves.map(worldCard)
      : h("div", { class: "empty" }, h("b", {}, "No worlds yet"), h("div", {}, "Worlds appear here after you start a game.")))));
}

// ---- conflicts -------------------------------------------------------------

function issueCard(i) {
  return h("div", { class: `card issue ${i.severity}` },
    h("div", { class: "issue-head" }, h("span", { class: `badge ${i.severity}` }, i.severity), i.title),
    h("div", { class: "issue-detail" }, i.detail),
    i.xpath && h("div", { class: "issue-path mono" }, h("em", {}, i.file), "  ", i.xpath),
    i.xpath && i.parts.map((p) => h("div", { class: `part ${i.winner === p.title ? "win" : ""}` },
      h("b", {}, p.title), h("span", { class: "muted" }, p.op), h("span", { class: "val mono" }, p.value))),
    !i.xpath && i.parts.length > 1 && h("div", { class: "muted" }, i.parts.map((p) => p.folder).join("  ·  ")));
}

function viewConflicts() {
  const r = S.report || { issues: [], mods: 0, edits: 0 };
  viewEl.replaceChildren(h("div", { class: "page" }, h("div", { class: "page-narrow" },
    h("div", { class: "page-head" }, h("h1", {}, "Conflicts"), h("div", { class: "spacer" }),
      btn("Scan again", afterChange, { icon: "refresh" })),
    h("p", { class: "lede" }, `Compared ${plural(r.edits, "XML edit")} across ${plural(r.mods, "enabled mod")}. Adding to the same list is normally fine, so only real collisions appear here.`),
    h("div", { class: "summary" },
      [["high", "Serious"], ["medium", "Overlaps"], ["low", "Notes"]].map(([sev, label]) =>
        h("div", { class: `stat ${sev}` }, h("b", {}, issueCount(sev)), h("span", {}, label)))),
    r.issues.length ? r.issues.map(issueCard) : h("div", { class: "empty" }, h("b", {}, "All clear"), h("div", {}, "Your enabled mods don't collide.")))));
}

// ---- backups ---------------------------------------------------------------

async function viewBackups() {
  const list = await call("list_backups");
  if (S.view !== "backups") return;
  const label = h("input", { type: "text", placeholder: "Backup name (optional)", style: "flex:1;min-width:200px" });
  const mods = h("input", { type: "checkbox", checked: true });
  const saves = h("input", { type: "checkbox", checked: false });

  const create = async () => {
    try {
      await withBusy("Creating backup", () => call("create_backup", { label: label.value, mods: mods.checked, saves: saves.checked }));
      toast("Backup created");
    } catch (e) {
      fail(e);
    }
    viewBackups();
  };

  const restore = async (b) => {
    const what = b.kind === "save" ? "this world" : [b.kind === "mod" ? "this mod" : b.has_mods && "all mods", b.has_saves && "your saves"].filter(Boolean).join(" and ");
    if (!(await confirmBox(`Restore ${b.label}?`, `This replaces ${what} with the backed-up copy. A safety backup of the current state is made first.`, "Restore", true))) return;
    try {
      await withBusy("Restoring", () => call("restore_backup", { id: b.id }));
      toast("Restored");
    } catch (e) {
      fail(e);
    }
    await afterChange();
  };

  const remove = async (b) => {
    if (!(await confirmBox(`Delete ${b.label}?`, "This backup is permanently removed.", "Delete", true))) return;
    await call("delete_backup", { id: b.id }).catch(fail);
    viewBackups();
  };

  const row = (b) => h("div", { class: "card" }, h("div", { class: "card-row" },
    h("div", { style: "flex:1;min-width:200px" },
      h("div", { class: "card-title", style: "font-size:18px" }, b.label, " ", b.auto && h("span", { class: "badge" }, "Auto")),
      h("div", { class: "card-sub" }, [fmtDate(b.created), fmtSize(b.size), b.kind === "save" ? "World" : b.kind === "mod" ? "Single mod" : [b.has_mods && "Mods", b.has_saves && "Saves"].filter(Boolean).join(" + ")].join("  ·  "))),
    h("div", { class: "actions" },
      btn("Restore", () => restore(b), { small: true }),
      btn("", () => remove(b), { small: true, kind: "danger", icon: "trash", title: "Delete backup" }))));

  viewEl.replaceChildren(h("div", { class: "page" }, h("div", { class: "page-narrow" },
    h("div", { class: "page-head" }, h("h1", {}, "Backups"), h("div", { class: "spacer" }),
      btn("Open folder", () => call("open_backups").catch(fail), { icon: "folder" })),
    h("p", { class: "lede" }, "Mods are backed up automatically before being replaced or uninstalled, and the 20 latest are kept. Worlds tagged on the Worlds page are backed up before you launch the other mode."),
    h("div", { class: "card section" },
      h("div", { class: "form-row" }, label, btn("Create backup", create, { kind: "primary" })),
      h("div", { class: "form-row", style: "margin:0" },
        h("label", { class: "check" }, mods, "Mods"),
        h("label", { class: "check" }, saves, "Saves"))),
    list.length ? list.map(row) : h("div", { class: "empty" }, h("b", {}, "No backups yet")))));
}

// ---- logs ------------------------------------------------------------------

const LINE_H = 19;
let logBody, logInner, logSummary, logRows = [];

async function viewLogs() {
  const L = S.log;
  L.files = await call("list_logs");
  if (S.view !== "logs") return;
  if (!L.files.some((f) => f.path === L.path)) {
    L.path = L.files[0]?.path || null;
    L.data = null;
  }
  if (!L.files.length) {
    viewEl.replaceChildren(h("div", { class: "empty" }, h("b", {}, "No logs yet"), h("div", {}, "Launch the game once and its log appears here."),
      h("div", { class: "actions" }, btn("Check again", viewLogs, { icon: "refresh" }))));
    return;
  }
  logBody = h("div", { class: "log-body", onscroll: paintLog });
  logInner = h("div", { class: "log-lines" });
  logBody.append(logInner);
  logSummary = h("div", { class: "log-summary" });
  viewEl.replaceChildren(
    h("div", { class: "logs-bar" },
      h("select", { "aria-label": "Log file", onchange: async (e) => { L.path = e.target.value; L.data = null; await loadLog(); } },
        L.files.map((f) => h("option", { value: f.path, selected: f.path === L.path }, `${f.name}  (${fmtDate(f.modified)})`))),
      h("div", { class: "seg", role: "group", "aria-label": "Show" },
        [["all", "All"], ["err", "Errors"], ["warn", "Warnings"], ["mods", "Mods"]].map(([id, label]) =>
          h("button", { class: L.level === id ? "on" : "", onclick: () => { L.level = id; viewLogs(); } }, label))),
      h("div", { class: "search" }, icon("search"), h("input", {
        type: "text", placeholder: "Search log", "aria-label": "Search log", value: L.query,
        oninput: (e) => { L.query = e.target.value; filterLog(); },
      })),
      h("label", { class: "check" }, h("input", { type: "checkbox", checked: L.follow, onchange: (e) => { L.follow = e.target.checked; if (L.follow) logBody.scrollTop = logBody.scrollHeight; } }), "Follow")),
    logSummary,
    logBody);
  await loadLog(true);
}

async function loadLog(force) {
  const L = S.log;
  if (!L.path) return;
  try {
    const next = await call("read_log", { path: L.path, known: force || !L.data ? null : L.data.size });
    if (next) L.data = next;
    if (next || force) filterLog();
  } catch (e) {
    fail(e);
  }
}

function filterLog() {
  const L = S.log;
  if (!L.data) return;
  const q = L.query.trim().toLowerCase();
  logRows = L.data.lines.filter(([lvl, mod, text]) => {
    if (L.level === "err" && lvl !== 2) return false;
    if (L.level === "warn" && lvl !== 1) return false;
    if (L.level === "mods" && !mod) return false;
    return !q || text.toLowerCase().includes(q);
  });
  fill(logSummary,
    h("span", { class: "badge high" }, plural(L.data.errors, "error")),
    h("span", { class: "badge medium" }, plural(L.data.warnings, "warning")),
    L.data.truncated && h("span", {}, "Showing the last 4 MB."),
    L.data.implicated.length > 0 && h("span", {}, "Errors mention: ", L.data.implicated.slice(0, 5).map((m) => `${m.title} (${m.count})`).join(", ")));
  logInner.style.height = `${logRows.length * LINE_H}px`;
  if (L.follow) logBody.scrollTop = logBody.scrollHeight;
  paintLog();
}

// Logs can run to tens of thousands of lines, so only visible rows are in the DOM.
function paintLog() {
  if (!logBody) return;
  const first = Math.max(0, Math.floor(logBody.scrollTop / LINE_H) - 10);
  const count = Math.ceil(logBody.clientHeight / LINE_H) + 20;
  const frag = document.createDocumentFragment();
  logRows.slice(first, first + count).forEach(([lvl, mod, text], i) => {
    const row = h("div", { class: `log-line l${lvl} ${mod ? "m" : ""}` }, text);
    row.style.position = "absolute";
    row.style.top = `${(first + i) * LINE_H}px`;
    row.style.left = 0;
    frag.append(row);
  });
  logInner.replaceChildren(frag);
}

// ---- settings --------------------------------------------------------------

async function viewSettings() {
  const i = inst();
  const handler = await invoke("nexus_handler_status");
  if (S.view !== "settings") return;
  const page = h("div", { class: "page" });

  const saveLaunch = async () => {
    await call("set_launch", { prefs: S.launch }).catch(fail);
    renderTop();
  };

  const eacOption = (value, title, sub) => h("label", {},
    h("input", { type: "radio", name: "eac", checked: S.launch.eac === value, onchange: () => { S.launch.eac = value; saveLaunch(); } }),
    h("span", {}, title, h("small", {}, sub)));

  const keyInput = h("input", { type: "password", placeholder: "Paste your Nexus personal API key", style: "flex:1;min-width:240px", spellcheck: "false" });
  const saveKey = async () => {
    try {
      const user = await withBusy("Checking key with Nexus", () => invoke("nexus_set_key", { key: keyInput.value }));
      S.boot.nexus_user = user;
      S.boot.has_key = true;
      toast(`Connected as ${user.name}`);
      viewSettings();
    } catch (e) {
      fail(e);
    }
  };
  const removeKey = async () => {
    await invoke("nexus_clear_key").catch(fail);
    S.boot.nexus_user = null;
    S.boot.has_key = false;
    viewSettings();
  };
  const setHandler = async (on) => {
    await invoke("nexus_set_handler", { on }).catch(fail);
    viewSettings();
  };

  const showDll = S.status?.eac_needed.length > 0;
  const folders = S.boot.instances.map((x) => h("div", { class: "found" },
    h("div", {}, h("b", {}, x.name), " ", h("span", { class: "badge" }, x.kind === "server" ? "Server" : "Client"), h("div", { class: "mono muted" }, x.path)),
    btn("Remove", async () => {
      if (!(await confirmBox(`Remove ${x.name}?`, "The manager stops tracking this folder. Your game and mods aren't touched.", "Remove", true))) return;
      await invoke("remove_instance", { inst: x.id }).catch(fail);
      S.boot = await invoke("bootstrap");
      S.status = null;
      topEl = null;
      if (instId()) S.launch = await call("get_launch");
      await reload().catch(fail);
    }, { small: true })));

  const detectedHolder = h("div", {});
  const scan = async () => {
    const found = foundList(await invoke("detect_games"));
    detectedHolder.replaceChildren(...(found.length ? found : [h("p", { class: "muted" }, "No other installs found.")]));
  };

  page.append(h("div", { class: "page-narrow" },
    h("div", { class: "page-head", style: "margin-bottom:18px" }, h("h1", {}, "Settings")),

    h("div", { class: "section" }, h("h2", {}, "Game folders"), folders, detectedHolder,
      h("div", { class: "actions", style: "margin-top:12px" },
        btn("Browse for folder", browseFolder, { icon: "folder" }),
        btn("Find installs", () => scan().catch(fail), { icon: "search" }))),

    i?.kind === "client" && h("div", { class: "section" }, h("h2", {}, "Launching"),
      h("div", { class: "radio-list" },
        eacOption("auto", "Automatic", "Turns EAC off only when an enabled mod contains a DLL. Vanilla mode always uses EAC, for crossplay."),
        eacOption("off", "Always without EAC", "Starts 7DaysToDie.exe. Servers with EAC on won't accept you."),
        eacOption("on", "Always with EAC", "Starts 7DaysToDie_EAC.exe. DLL mods won't load.")),
      h("label", { class: "field", style: "display:block;margin-bottom:5px" }, "Extra launch options"),
      h("input", { type: "text", value: S.launch.args, style: "width:100%", spellcheck: "false", placeholder: "-force-d3d11", onchange: (e) => { S.launch.args = e.target.value; saveLaunch(); } })),

    i?.kind === "server" && h("div", { class: "section" }, h("h2", {}, "Server EAC"),
      S.status?.server_eac === true
        ? h("div", { class: showDll ? "callout medium" : "callout" }, h("b", {}, "EAC is on in serverconfig.xml"), showDll ? "You have DLL mods enabled, which need EAC off." : "Fine for mods without DLLs.",
            h("div", {}, btn("Turn EAC off", async () => { await call("disable_server_eac").then(() => toast("EAC turned off in serverconfig.xml")).catch(fail); await refreshStatus(); viewSettings(); }, { small: true })))
        : h("p", { class: "muted" }, S.status?.server_eac === false ? "EAC is off in serverconfig.xml." : "Couldn't read the EAC setting from serverconfig.xml.")),

    h("div", { class: "section" }, h("h2", {}, "Nexus Mods"),
      S.boot.nexus_user
        ? h("div", { class: "form-row" }, h("span", {}, "Connected as ", h("b", {}, S.boot.nexus_user.name)), S.boot.nexus_user.premium && h("span", { class: "badge amber" }, "Premium"), btn("Remove key", removeKey, { small: true }))
        : [h("div", { class: "form-row" }, keyInput, btn("Connect", saveKey, { kind: "primary" })),
           h("p", { class: "muted" }, "Your key is stored in Windows Credential Manager. ",
             h("a", { href: "#", style: "color:var(--amber-hi)", onclick: (e) => { e.preventDefault(); invoke("open_url", { url: "https://next.nexusmods.com/settings/api-keys" }).catch(fail); } }, "Get your API key"))],
      h("div", { class: "form-row" },
        handler.state === "this"
          ? [h("span", {}, h("i", { class: "dot ok" }), "Handling Mod Manager Download links"), btn("Stop", () => setHandler(false), { small: true })]
          : [h("span", { class: "muted" }, handler.state === "other" ? "Another app, or an older copy of this one, handles Mod Manager Download links." : "Mod Manager Download links aren't connected."),
             btn("Use this app", () => setHandler(true), { small: true, kind: "primary" })])),

    h("div", { class: "section" }, h("h2", {}, "About"),
      h("dl", { class: "meta", style: "grid-template-columns:110px 1fr" },
        h("dt", {}, "Version"), h("dd", {}, S.boot.version),
        h("dt", {}, "App data"), h("dd", { class: "mono" }, S.boot.data_dir)),
      h("p", { class: "muted" }, "Not affiliated with The Fun Pimps or Nexus Mods."))));
  viewEl.replaceChildren(page);
}

// ---- startup ---------------------------------------------------------------

async function main() {
  S.boot = await invoke("bootstrap");
  if (instId()) S.launch = await call("get_launch");
  await reload();

  listen("progress", (e) => { if (busyEl) setBusy(e.payload.message, e.payload.fraction); });
  listen("nxm-ready", handleNxm);

  const drop = document.getElementById("drop");
  listen("tauri://drag-enter", () => { if (instId()) drop.hidden = false; });
  listen("tauri://drag-leave", () => { drop.hidden = true; });
  listen("tauri://drag-drop", (e) => {
    drop.hidden = true;
    if (e.payload.paths?.length) installPaths(e.payload.paths);
  });

  // Mods can change outside the app, so catch up when the window regains focus.
  let refreshing = false;
  window.addEventListener("focus", async () => {
    if (refreshing || !instId() || document.querySelector(".overlay")) return;
    refreshing = true;
    try {
      await refreshMods();
      renderNav();
      if (S.view === "mods") { renderList(); renderDetail(); }
    } catch (_) { /* Shown on the next action. */ }
    refreshing = false;
  });

  setInterval(refreshStatus, 5000);
  setInterval(() => {
    if (S.view === "logs" && S.log.follow && !document.hidden && S.log.path && logBody?.isConnected) loadLog();
  }, 3000);
  handleNxm();
}

main().catch((e) => {
  document.getElementById("app").replaceChildren(h("div", { class: "empty" }, h("b", {}, "Couldn't start"), h("div", {}, String(e))));
});
