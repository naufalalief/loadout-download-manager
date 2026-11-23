// Frontend for the download queue: polls `list_downloads` and renders them
// as table rows, plus the always-visible add-link bar and the Settings
// page. Plain JS, no framework, no build step -- `window.__TAURI__` is
// exposed because `withGlobalTauri` is enabled in `tauri.conf.json`.

import { appIcon, settingsIcon } from "./icons.js";

const { invoke } = window.__TAURI__.core;

const POLL_INTERVAL_MS = 1000;

const queueListEl = document.getElementById("queue-list");
const emptyStateEl = document.getElementById("empty-state");

const shedBtn = document.getElementById("shedbtn");
const shedIconEl = document.getElementById("shedbtn-icon");
const addbarIconEl = document.getElementById("addbar-icon");
const emptyStateIconEl = document.getElementById("empty-state-icon");
const statusTasksEl = document.getElementById("status-tasks");
const statusWorkingEl = document.getElementById("status-working");

const addFormEl = document.getElementById("add-form");
const addUrlEl = document.getElementById("add-url");
const addErrorEl = document.getElementById("add-error");

const settingsDownloadDirEl = document.getElementById("settings-download-dir");
const settingsBrowseDownloadDirEl = document.getElementById("settings-browse-download-dir");
const settingsMaxConcurrentEl = document.getElementById("settings-max-concurrent");
const settingsSpeedLimitEl = document.getElementById("settings-speed-limit");
const settingsErrorEl = document.getElementById("settings-error");

// The two full-window pages. Exactly one is visible at a time; the
// statusbar sits outside both and is always shown.
const ledgerPageEl = document.getElementById("ledger-page");
const settingsPageEl = document.getElementById("settings-page");
const PAGES = [ledgerPageEl, settingsPageEl];
function showPage(page) {
  for (const p of PAGES) p.classList.toggle("hidden", p !== page);
}

// Icons are plain stroke-based SVG markup (icons.js exports each icon's
// inner <svg> markup as a string). The stroke presentation attributes are
// set here on the target <svg> itself -- no stylesheet rule supplies them --
// so the icon's color comes from the element's own CSS `color`.
function renderIconInto(svgEl, markup) {
  svgEl.setAttribute("fill", "none");
  svgEl.setAttribute("stroke", "currentColor");
  svgEl.setAttribute("stroke-width", "1.6");
  svgEl.setAttribute("stroke-linecap", "round");
  svgEl.setAttribute("stroke-linejoin", "round");
  svgEl.innerHTML = markup;
}

function formatBytes(bytes) {
  if (bytes == null) return "?";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  const precision = unitIndex === 0 ? 0 : 1;
  return `${value.toFixed(precision)} ${units[unitIndex]}`;
}

function progressPercent(download) {
  if (!download.total_bytes) return null;
  const pct = (download.downloaded_bytes / download.total_bytes) * 100;
  return Math.max(0, Math.min(100, pct));
}

/**
 * Live transfer speed for a row, in bytes/sec, estimated from the byte
 * delta between this call and the last one for this same row. The
 * `list_downloads` response carries no speed number -- `downloaded_bytes`
 * is the only thing the backend tracks -- so this is the only source for
 * the statusbar's combined speed and the progress cell's tooltip.
 *
 * `els` is the row's persistent per-id cache (`rowsById`), so the
 * previous sample survives across polls. Returns `null` when `active` is
 * false (nothing to measure a rate for) or on a row's very first active
 * sample (no earlier point yet to diff against) -- both cases clear the
 * stored sample too, so a row that stops and later restarts (retry,
 * resume) doesn't compute a speed from a stale pre-restart byte count.
 */
function liveSpeedBytesPerSec(els, downloadedBytes, active) {
  if (!active || downloadedBytes == null) {
    els.speedPrevBytes = null;
    els.speedPrevTime = null;
    els.speedLast = null;
    return null;
  }

  const now = Date.now();
  if (els.speedPrevTime != null) {
    const deltaSeconds = (now - els.speedPrevTime) / 1000;
    // A delta of 0 (two updates landing in the same millisecond) would
    // divide by zero -- keep whatever rate is already on record instead.
    if (deltaSeconds > 0) {
      const deltaBytes = Math.max(0, downloadedBytes - els.speedPrevBytes);
      els.speedLast = deltaBytes / deltaSeconds;
    }
  }

  els.speedPrevBytes = downloadedBytes;
  els.speedPrevTime = now;
  return els.speedLast ?? null;
}

/**
 * `<downloaded> of <total>` (or just `<downloaded>` when the total isn't
 * known yet), plus a ` -- <speed>/s` suffix whenever a live speed is
 * available. Shown as the progress cell's hover tooltip.
 */
function sizeAndSpeedText(downloadedBytes, totalBytes, speedBytesPerSec) {
  const sizeText =
    totalBytes != null
      ? `${formatBytes(downloadedBytes)} of ${formatBytes(totalBytes)}`
      : formatBytes(downloadedBytes);
  return speedBytesPerSec != null ? `${sizeText} — ${formatBytes(speedBytesPerSec)}/s` : sizeText;
}

// The table's Size column: the total once it's known, else whatever has
// been downloaded so far, else a dash (nothing known yet -- a pending row).
function sizeCellText(item) {
  if (item.total_bytes != null) return formatBytes(item.total_bytes);
  if (item.downloaded_bytes) return formatBytes(item.downloaded_bytes);
  return "—";
}

// The table's Added column. `created_at` is Unix seconds; a missing/0 value
// (entries saved before `created_at` existed) shows a dash.
function formatDateAdded(createdAtUnixSeconds) {
  if (!createdAtUnixSeconds) return "—";
  const date = new Date(createdAtUnixSeconds * 1000);
  const now = new Date();
  if (date.toDateString() === now.toDateString()) return "Today";
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

// Leading icon for the Status cell -- only these three statuses get one.
function statusIconMarkup(status) {
  if (status === "Completed") return '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M20 6L9 17l-5-5"/></svg>';
  if (status === "Failed") return '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 8v5M12 16h.01"/></svg>';
  if (status === "Paused") return '<svg width="11" height="11" viewBox="0 0 24 24" fill="currentColor"><rect x="7" y="5" width="3" height="14" rx="0.5"/><rect x="14" y="5" width="3" height="14" rx="0.5"/></svg>';
  return "";
}

// Small stroke icons for the unfold's `.uf-opt` entries.
const UF_ICONS = {
  open: '<path d="M14 3h7v7"/><path d="M10 14L21 3"/><path d="M21 14v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  link: '<path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1 1"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1-1"/>',
  pause: '<path d="M9 5v14M15 5v14"/>',
  resume: '<path d="M7 4l12 8-12 8z"/>',
  retry: '<path d="M3 12a9 9 0 1 0 3-6.7"/><path d="M3 4v5h5"/>',
  cancel: '<path d="M6 6l12 12M18 6L6 18"/>',
  remove: '<path d="M4 7h16"/><path d="M10 11v6M14 11v6"/><path d="M6 7l1 13h10l1-13"/><path d="M9 7V4h6v3"/>',
  speed: '<path d="M12 14l4-4"/><path d="M4 18a9 9 0 1 1 16 0"/>',
};

function ufIconMarkup(name) {
  return `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${UF_ICONS[name] ?? ""}</svg>`;
}

// Statuses that mean no further transfer will ever happen for this row.
const TERMINAL_STATUSES = ["Completed", "Failed", "Canceled"];

// Parses a speed-limit input's value into what the backend expects:
// `null` for "no limit" (empty, 0, or anything non-numeric), else a
// positive integer number of bytes/sec. 0 deliberately means "no limit",
// never "stall forever".
//
// The value must be *entirely* digits -- `parseInt` alone silently parses
// a leading numeric prefix and ignores the rest (e.g. "2MB" -> 2,
// "1,000,000" -> 1), which would turn a typo'd limit into a near-zero
// throttle instead of being rejected. A non-digit value is treated as
// invalid the same way genuinely empty input is: no limit.
function parseSpeedLimit(raw) {
  const trimmed = String(raw ?? "").trim();
  if (trimmed === "") return null;
  if (!/^\d+$/.test(trimmed)) return null;
  const value = parseInt(trimmed, 10);
  if (!Number.isFinite(value) || value <= 0) return null;
  return value;
}

// Status -> Status-cell word. Exact words, not open to rephrasing. (Colors
// come from the CSS -- `.c-status`, `.c-status.failed` -- not from here.)
const DOWNLOAD_BADGE = {
  Downloading: "Downloading",
  Paused: "Paused",
  Queued: "Queued",
  Completed: "Completed",
  Failed: "Failed",
  Canceled: "Canceled",
};

// Rows are kept alive across polls, keyed by id, and only their contents
// are updated in place. Rebuilding the whole list every poll could destroy
// a button between mousedown and mouseup and silently lose the click, and
// would wipe a half-typed speed limit.
const rowsById = new Map();

function makeCell(className) {
  const el = document.createElement("span");
  el.className = className;
  return el;
}

function createRow(item) {
  const rowId = item.id;

  const row = document.createElement("div");
  row.className = "trow";
  // Focusable so a keyboard user can Tab onto the row itself and reveal
  // `.acts` via `:focus-within` (below) -- without this, `.acts`'s buttons
  // stay `display:none` until hovered, and a `display:none` element can
  // never receive focus in the first place, so Tab would otherwise skip
  // straight over every row's actions.
  row.tabIndex = 0;

  const cName = makeCell("c-name");
  const cProgress = makeCell("c-progress");
  const cSize = makeCell("c-size");
  const cStatus = makeCell("c-status");
  const cDate = makeCell("c-date");
  // Hover-only action links (CSS swaps `.acts` in for `.c-status` on hover
  // or keyboard focus -- see `.trow:focus-within` in styles.css).
  const acts = makeCell("acts");
  row.append(cName, cProgress, cSize, cStatus, cDate, acts);

  // The row's one accordion panel -- right-click menu and the per-download
  // speed-limit form both render into this (see `openUnfold`/
  // `buildUnfoldContent`). Kept out of the DOM while closed (so closed
  // unfolds never count as `.tbody` children for the zebra striping) and
  // inserted directly after `row` when opened (`.trow.open + .unfold`).
  const unfold = document.createElement("div");
  unfold.className = "unfold hidden";

  const els = {
    rowId,
    row,
    cName,
    cProgress,
    cSize,
    cStatus,
    cDate,
    acts,
    unfold,
    actionsKey: null,
    statusKey: null,
    currentItem: null,
  };

  // Right-click opens the row's unfold (FDM/IDM-style), double-click a
  // finished row to open its file directly -- both read `els.currentItem`
  // at the moment of the event (kept fresh every poll by `updateRow`), not
  // whatever `item` this row happened to be created with.
  row.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    event.stopPropagation();
    const current = els.currentItem;
    if (!current) return;
    const content = buildUnfoldContent(current);
    if (!content.length) return;
    openUnfold(rowId, content);
  });
  row.addEventListener("dblclick", () => {
    const current = els.currentItem;
    if (!current || current.status !== "Completed" || !current.dest_path) return;
    invoke("open_path_in_default_app", { path: current.dest_path }).catch((err) =>
      console.error("open_path_in_default_app failed:", err),
    );
  });

  return els;
}

// `window.confirm()`/`alert()`/`prompt()` never actually show anything in
// this app's webview -- confirmed live: clicking an action that called
// `window.confirm()` deleted a file immediately, with no dialog ever
// appearing, meaning it silently resolved as if the user had clicked OK.
// Every yes/no prompt needs this instead: a real in-app modal (backdrop +
// box, styled like the rest of the app), resolving `true`/`false` for
// "confirm" vs. "cancel" (Escape and the backdrop both count as cancel).
function confirmModal({ message, confirmLabel = "OK", cancelLabel = "Cancel", danger = false }) {
  return new Promise((resolve) => {
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";

    const box = document.createElement("div");
    box.className = "modal-box";

    const msg = document.createElement("div");
    msg.className = "modal-msg";
    msg.textContent = message;
    box.appendChild(msg);

    const actions = document.createElement("div");
    actions.className = "modal-actions";

    function finish(result) {
      document.removeEventListener("keydown", onKeydown);
      backdrop.remove();
      resolve(result);
    }
    function onKeydown(event) {
      if (event.key === "Escape") finish(false);
    }

    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "btn";
    cancelBtn.textContent = cancelLabel;
    cancelBtn.addEventListener("click", () => finish(false));

    const confirmBtn = document.createElement("button");
    confirmBtn.type = "button";
    confirmBtn.className = danger ? "btn danger" : "btn primary";
    confirmBtn.textContent = confirmLabel;
    confirmBtn.addEventListener("click", () => finish(true));

    actions.append(cancelBtn, confirmBtn);
    box.appendChild(actions);
    backdrop.appendChild(box);
    backdrop.addEventListener("click", (event) => {
      if (event.target === backdrop) finish(false);
    });

    document.body.appendChild(backdrop);
    document.addEventListener("keydown", onKeydown);
    confirmBtn.focus();
  });
}

// Removing a Completed entry asks whether to delete the finished file too;
// only an explicit "OK" deletes it. Every other status just removes the
// entry (the backend only ever cleans up its own partial files for those).
async function removeItem(download) {
  if (download.status === "Completed") {
    const deleteFile = await confirmModal({
      message: `Delete the downloaded file "${download.filename}" too?`,
      confirmLabel: "Delete file",
      cancelLabel: "Keep file",
      danger: true,
    });
    if (deleteFile) {
      await invoke("remove_download_and_delete_file", { id: download.id });
      return;
    }
  }
  await invoke("remove_download", { id: download.id });
}

// Each entry is either `{ label, onClick, danger }` (an action link that
// calls `invoke` and refreshes -- see `makeActionLink`).
function actionsFor(download) {
  const id = download.id;
  const actions = [];
  if (download.status === "Downloading") {
    actions.push({ label: "Pause", onClick: () => invoke("pause_download", { id }) });
  }
  if (download.status === "Paused") {
    actions.push({ label: "Resume", onClick: () => invoke("resume_download", { id }) });
  }
  if (download.status === "Failed") {
    actions.push({ label: "Retry", onClick: () => invoke("retry_download", { id }) });
  }
  // The per-download speed limit is no longer a hover action -- it lives in
  // the row's unfold (see `buildUnfoldContent`).
  if (["Queued", "Downloading", "Paused"].includes(download.status)) {
    actions.push({ label: "Cancel", onClick: () => invoke("cancel_download", { id }), danger: true });
  }
  if (TERMINAL_STATUSES.includes(download.status)) {
    actions.push({
      label: "Remove",
      onClick: () => removeItem(download),
      danger: download.status === "Failed",
    });
  }
  return actions;
}

// One `.uf-opt` row: icon + label. Clicking it closes the unfold first, then
// runs `onClick` and (unless `skipRefresh`) refreshes the list.
function makeUnfoldOption(rowId, entry) {
  const opt = document.createElement("button");
  opt.type = "button";
  opt.className = entry.danger ? "uf-opt danger" : "uf-opt";
  opt.innerHTML = entry.icon ? ufIconMarkup(entry.icon) : "";
  const label = document.createElement("span");
  label.textContent = entry.label;
  opt.appendChild(label);
  opt.addEventListener("click", async () => {
    closeUnfold(rowId);
    try {
      await entry.onClick();
      if (!entry.skipRefresh) await refreshList();
    } catch (err) {
      console.error(`${entry.label} failed:`, err);
    }
  });
  return opt;
}

function makeUnfoldDivider() {
  const div = document.createElement("div");
  div.className = "uf-div";
  return div;
}

// Right-click content for a row, grouped with a `.uf-div` divider between
// each non-empty group: file actions (only once a real finished file
// exists), copying the original source link, the row's own status action
// (mirrors `actionsFor` above), then Remove. An empty group is never
// included, so there's never a leading/trailing/doubled divider. Returns
// an array of DOM elements.
function contextMenuItemsFor(item) {
  const id = item.id;
  const rowId = item.id;
  const groups = [];

  if (item.status === "Completed" && item.dest_path) {
    groups.push([
      { label: "Open file", icon: "open", onClick: () => invoke("open_path_in_default_app", { path: item.dest_path }) },
      { label: "Open file location", icon: "folder", onClick: () => invoke("reveal_path_in_file_manager", { path: item.dest_path }) },
    ]);
  }

  if (item.url) {
    groups.push([
      {
        label: "Copy download link",
        icon: "link",
        skipRefresh: true,
        onClick: () => navigator.clipboard.writeText(item.url),
      },
    ]);
  }

  const statusGroup = [];
  if (item.status === "Downloading") {
    statusGroup.push({ label: "Pause", icon: "pause", onClick: () => invoke("pause_download", { id }) });
  }
  if (item.status === "Paused") {
    statusGroup.push({ label: "Resume", icon: "resume", onClick: () => invoke("resume_download", { id }) });
  }
  if (item.status === "Failed") {
    statusGroup.push({ label: "Retry", icon: "retry", onClick: () => invoke("retry_download", { id }) });
  }
  if (["Queued", "Downloading", "Paused"].includes(item.status)) {
    statusGroup.push({ label: "Cancel", icon: "cancel", danger: true, onClick: () => invoke("cancel_download", { id }) });
  }
  if (statusGroup.length) groups.push(statusGroup);

  if (TERMINAL_STATUSES.includes(item.status)) {
    groups.push([
      {
        label: "Remove",
        icon: "remove",
        danger: item.status === "Failed",
        onClick: () => removeItem(item),
      },
    ]);
  }

  return groups.flatMap((group, groupIndex) => [
    ...(groupIndex > 0 ? [makeUnfoldDivider()] : []),
    ...group.map((entry) => makeUnfoldOption(rowId, entry)),
  ]);
}

// The unfold's first line on a Failed row: the raw error text (the Status
// cell itself only ever shows the plain word "Failed").
function makeErrorLine(text) {
  const line = document.createElement("div");
  line.className = "uf-error";
  line.innerHTML = statusIconMarkup("Failed");
  const span = document.createElement("span");
  span.textContent = text;
  line.appendChild(span);
  return line;
}

// The per-download speed limit, applied via the existing
// `set_download_speed_limit` command. Starts as one `.uf-opt` row; clicking
// it swaps that row in place for the `.uf-form` input + Set button.
function makeSpeedLimitOption(download) {
  const rowId = download.id;
  const opt = document.createElement("button");
  opt.type = "button";
  opt.className = "uf-opt";
  opt.innerHTML = ufIconMarkup("speed");
  const label = document.createElement("span");
  label.textContent = "Limit speed…";
  opt.appendChild(label);

  opt.addEventListener("click", () => {
    const form = document.createElement("div");
    form.className = "uf-form";
    const limitInput = document.createElement("input");
    limitInput.type = "text";
    limitInput.inputMode = "numeric";
    limitInput.placeholder = "no limit (bytes/sec)";
    limitInput.title = "Speed limit for this download, in bytes/sec (empty or 0 = no limit)";
    const limitButton = document.createElement("button");
    limitButton.type = "button";
    limitButton.textContent = "Set";
    limitButton.title = "Apply this speed limit";
    limitButton.addEventListener("click", async () => {
      limitButton.disabled = true;
      try {
        await invoke("set_download_speed_limit", {
          id: download.id,
          bytesPerSec: parseSpeedLimit(limitInput.value),
        });
        closeUnfold(rowId);
        await refreshList();
      } catch (err) {
        console.error("set_download_speed_limit failed:", err);
      } finally {
        limitButton.disabled = false;
      }
    });
    limitInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        limitButton.click();
      }
    });
    form.append(limitInput, limitButton);
    opt.replaceWith(form);
    limitInput.focus();
  });

  return opt;
}

// Everything a row's unfold shows, for its current item: the error line
// (Failed only), the speed-limit option (every status except Completed/
// Canceled), then the usual right-click entries.
function buildUnfoldContent(item) {
  const menuItems = contextMenuItemsFor(item);

  const content = [];
  if (item.status === "Failed") {
    content.push(makeErrorLine(item.error || "Failed"));
  }
  const limitable = !["Completed", "Canceled"].includes(item.status);
  if (limitable) {
    content.push(makeSpeedLimitOption(item));
    if (menuItems.length) content.push(makeUnfoldDivider());
  }
  content.push(...menuItems);
  return content;
}

// At most one row's unfold is open at a time.
const openUnfoldIds = new Set();

function closeUnfold(rowId) {
  const els = rowsById.get(rowId);
  if (!els) return;
  els.unfold.classList.add("hidden");
  els.unfold.replaceChildren();
  els.unfold.remove();
  els.row.classList.remove("open");
  openUnfoldIds.delete(rowId);
}

function openUnfold(rowId, contentEls) {
  for (const id of Array.from(openUnfoldIds)) closeUnfold(id);
  const els = rowsById.get(rowId);
  if (!els) return;
  els.unfold.replaceChildren(...contentEls);
  els.unfold.classList.remove("hidden");
  els.row.after(els.unfold);
  els.row.classList.add("open");
  openUnfoldIds.add(rowId);
}

// Closes an open unfold on an ordinary left-click or a right-click landing
// outside both it and its row, and on Escape. Uses `composedPath()` (fixed
// at dispatch time) rather than `contains(event.target)`, because a click
// inside the unfold can detach its own target before the event bubbles
// here (e.g. "Limit speed…" replacing itself with the form).
function closeUnfoldsOutside(event) {
  const path = event.composedPath();
  for (const id of Array.from(openUnfoldIds)) {
    const els = rowsById.get(id);
    if (els && !path.includes(els.unfold) && !path.includes(els.row)) closeUnfold(id);
  }
}
document.addEventListener("click", closeUnfoldsOutside);
document.addEventListener("contextmenu", closeUnfoldsOutside);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") for (const id of Array.from(openUnfoldIds)) closeUnfold(id);
});

function updateRow(els, item) {
  // Kept fresh on every poll so the row's `contextmenu`/`dblclick`
  // listeners (bound once, in `createRow`) always act on current data --
  // status, `dest_path`, `url` -- not whatever the row happened to show
  // when it was first created.
  els.currentItem = item;

  if (els.cName.textContent !== item.filename) {
    els.cName.textContent = item.filename;
    els.cName.title = item.filename;
  }

  const sizeText = sizeCellText(item);
  if (els.cSize.textContent !== sizeText) els.cSize.textContent = sizeText;

  const dateText = formatDateAdded(item.created_at);
  if (els.cDate.textContent !== dateText) els.cDate.textContent = dateText;

  updateProgressCell(els, item);
  updateStatusCell(els, item, DOWNLOAD_BADGE[item.status] ?? "");
  updateActions(els, item, `${item.status}|${item.filename}`);
}

// Progress cell: a `.bar > .fill` plus a `.pct` label while Downloading
// (indeterminate -- full, dimmed fill and no percentage -- when the total
// size isn't known), empty otherwise. The bar's elements are built once
// and reused across polls.
function updateProgressCell(els, item) {
  const downloading = item.status === "Downloading";
  const speed = liveSpeedBytesPerSec(els, item.downloaded_bytes, downloading);

  if (!downloading) {
    if (els.cProgress.firstChild) els.cProgress.replaceChildren();
    els.cProgress.title = "";
    return;
  }

  if (!els.bar) {
    els.bar = document.createElement("div");
    els.bar.className = "bar";
    els.fill = document.createElement("div");
    els.fill.className = "fill";
    els.bar.appendChild(els.fill);
    els.pct = document.createElement("span");
    els.pct.className = "pct";
  }
  if (els.bar.parentNode !== els.cProgress) els.cProgress.replaceChildren(els.bar, els.pct);

  const pct = progressPercent(item);
  els.fill.style.width = `${pct ?? 100}%`;
  els.fill.style.opacity = pct == null ? "0.35" : "";
  const pctText = pct == null ? "" : `${Math.floor(pct)}%`;
  if (els.pct.textContent !== pctText) els.pct.textContent = pctText;
  els.cProgress.title = sizeAndSpeedText(item.downloaded_bytes, item.total_bytes, speed);
}

// Status cell: the plain status word, with a leading icon for Completed/
// Failed/Paused. Rebuilt only when the status changes. The raw error text
// never shows here -- only in the unfold.
function updateStatusCell(els, item, word) {
  if (els.statusKey === item.status) return;
  els.statusKey = item.status;

  els.cStatus.classList.toggle("failed", item.status === "Failed");
  els.cStatus.innerHTML = statusIconMarkup(item.status);
  const wordEl = document.createElement("span");
  wordEl.textContent = word;
  els.cStatus.appendChild(wordEl);
}

// Hover action links only change when the status does, so they are only
// rebuilt then -- a click in progress is never interrupted by an ordinary
// poll. A status change also closes this row's unfold, whose contents were
// built for the old status.
function updateActions(els, item, actionsKey) {
  if (els.actionsKey === actionsKey) return;
  const firstBuild = els.actionsKey === null;
  els.actionsKey = actionsKey;
  els.acts.replaceChildren(...actionsFor(item).map((action) => makeActionLink(action.label, action.onClick, action.danger)));
  if (!firstBuild && openUnfoldIds.has(els.rowId)) closeUnfold(els.rowId);
}

function makeActionLink(label, onClick, danger) {
  const link = document.createElement("button");
  link.type = "button";
  if (danger) link.className = "danger";
  link.textContent = label;
  let busy = false;
  link.addEventListener("click", async () => {
    if (busy) return;
    busy = true;
    try {
      await onClick();
      await refreshList();
    } catch (err) {
      console.error(`${label} failed:`, err);
    } finally {
      busy = false;
    }
  });
  return link;
}

// Last-known-good list, used so a transient `list_downloads` failure
// leaves the rows exactly as they were on screen instead of the poll
// silently emptying them.
let lastDownloads = [];

async function refreshList() {
  let downloads = lastDownloads;
  try {
    downloads = (await invoke("list_downloads")) || [];
    lastDownloads = downloads;
  } catch (err) {
    console.error("Failed to list downloads:", err);
  }

  const isEmpty = downloads.length === 0;
  emptyStateEl.classList.toggle("hidden", !isEmpty);
  queueListEl.classList.toggle("hidden", isEmpty);

  const seen = new Set();
  let previous = null;
  for (const item of downloads) {
    const rowId = item.id;
    seen.add(rowId);
    let els = rowsById.get(rowId);
    if (!els) {
      els = createRow(item);
      rowsById.set(rowId, els);
    }
    updateRow(els, item);

    // Keep DOM order matching the backend's order, moving a row only if
    // it's actually out of place (moving detaches it, which would also
    // interrupt a click). An open unfold must stay directly after its row.
    const expectedNext = previous ? previous.nextSibling : queueListEl.firstChild;
    if (expectedNext !== els.row) {
      queueListEl.insertBefore(els.row, expectedNext);
    }
    previous = els.row;
    if (els.unfold.isConnected) {
      if (els.row.nextSibling !== els.unfold) els.row.after(els.unfold);
      previous = els.unfold;
    }
  }

  for (const [id, els] of rowsById) {
    if (!seen.has(id)) {
      els.row.remove();
      els.unfold.remove();
      openUnfoldIds.delete(id);
      rowsById.delete(id);
    }
  }

  updateStatusBar(downloads);
}

// Status bar: "<N> item(s)", then "— <M> active, <speed>/s" while anything
// is downloading (empty otherwise). The combined speed sums each
// Downloading row's `els.speedLast`, already sampled this poll by
// `updateProgressCell` -- so this must run after the rows are updated.
function updateStatusBar(items) {
  const n = items.length;
  statusTasksEl.textContent = n === 0 ? "0 items" : `${n} item${n === 1 ? "" : "s"}`;

  let activeCount = 0;
  let combinedSpeed = 0;
  for (const item of items) {
    if (item.status !== "Downloading") continue;
    activeCount += 1;
    combinedSpeed += rowsById.get(item.id)?.speedLast ?? 0;
  }
  statusWorkingEl.textContent =
    activeCount > 0 ? `— ${activeCount} active, ${formatBytes(combinedSpeed)}/s` : "";
}

function showError(el, message) {
  el.textContent = message;
  el.classList.remove("hidden");
}

function hideError(el) {
  el.classList.add("hidden");
}

// The addbar is always visible: Enter in the link field submits straight to
// `add_download` (no filename field -- the backend picks the name).
addFormEl.addEventListener("submit", async (event) => {
  event.preventDefault();
  hideError(addErrorEl);

  const url = addUrlEl.value.trim();

  try {
    await invoke("add_download", { url });
    addUrlEl.value = "";
    await refreshList();
  } catch (err) {
    showError(addErrorEl, String(err));
  }
});

// Cached copy of the persisted settings, refreshed every time Settings
// opens and updated after every successful save, so a save only ever
// merges its patch onto a value known to still be current.
let currentSettings = null;

// Serializes every settings-mutating operation so a write only ever merges
// onto `currentSettings` after every earlier-queued write has actually
// finished -- without this, two near-simultaneous commits can race, and
// the later-resolving one can silently persist a stale snapshot over the
// other's change.
let settingsQueue = Promise.resolve();
function enqueueSettingsWrite(fn) {
  const result = settingsQueue.then(fn, fn);
  settingsQueue = result.catch(() => {});
  return result;
}

// Persists one Settings field change immediately. `set_settings` stores
// the whole DTO, not a diff, so `patch` is always merged onto the cached
// `currentSettings` and the cache is only updated once the save succeeds.
// A failure is shown in `#settings-error`. Returns whether the save
// succeeded.
async function saveSettings(patch) {
  return enqueueSettingsWrite(async () => {
    hideError(settingsErrorEl);
    const settings = { ...currentSettings, ...patch };
    try {
      await invoke("set_settings", { settings });
      currentSettings = settings;
      return true;
    } catch (err) {
      showError(settingsErrorEl, String(err));
      return false;
    }
  });
}

// Settings gear (in the addbar): loads the current settings into the
// Settings page's controls, then swaps to that page.
shedBtn.addEventListener("click", async () => {
  hideError(settingsErrorEl);
  try {
    const settings = await invoke("get_settings");
    currentSettings = settings;
    settingsDownloadDirEl.value = settings.download_dir;
    settingsMaxConcurrentEl.value = settings.max_concurrent;
    settingsSpeedLimitEl.value = settings.default_speed_limit_bytes_per_sec ?? "";
    showPage(settingsPageEl);
  } catch (err) {
    console.error("Failed to load settings:", err);
  }
});

// Change opens the OS's native folder picker (commands::pick_download_folder)
// instead of the field taking typed paths -- `settings-download-dir` is
// `readonly`. `null` covers the user canceling the dialog, which must leave
// the field exactly as it was, not clear it.
settingsBrowseDownloadDirEl.addEventListener("click", async () => {
  settingsBrowseDownloadDirEl.disabled = true;
  try {
    const picked = await invoke("pick_download_folder");
    if (picked) {
      const previous = settingsDownloadDirEl.value;
      settingsDownloadDirEl.value = picked;
      if (!(await saveSettings({ download_dir: picked }))) {
        settingsDownloadDirEl.value = previous;
      }
    }
  } catch (err) {
    console.error("pick_download_folder failed:", err);
  } finally {
    settingsBrowseDownloadDirEl.disabled = false;
  }
});

// Downloads-at-once and the default speed limit commit on blur or Enter
// (a no-op when the value didn't actually change).
function commitOnBlurOrEnter(inputEl, commit) {
  inputEl.addEventListener("blur", commit);
  inputEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      commit();
    }
  });
}

commitOnBlurOrEnter(settingsMaxConcurrentEl, () => {
  // Same native `required`/`min`/`step` checks the old <form> ran.
  if (!settingsMaxConcurrentEl.reportValidity()) return;
  const value = parseInt(settingsMaxConcurrentEl.value, 10);
  if (value === currentSettings.max_concurrent) return;
  saveSettings({ max_concurrent: value });
});

commitOnBlurOrEnter(settingsSpeedLimitEl, async () => {
  const value = parseSpeedLimit(settingsSpeedLimitEl.value.trim());
  if (value === (currentSettings.default_speed_limit_bytes_per_sec ?? null)) return;
  if (await saveSettings({ default_speed_limit_bytes_per_sec: value })) {
    settingsSpeedLimitEl.value = value ?? "";
  }
});

// Settings has no Save button: every field saves itself at its own commit
// point (see the handlers above), so Close just navigates back.
document.getElementById("settings-close-btn").addEventListener("click", () => {
  showPage(ledgerPageEl);
});

// Settings gear, addbar arrow, and empty-state icon: all static (never
// change at runtime), so rendered once here.
renderIconInto(shedIconEl, settingsIcon);
renderIconInto(addbarIconEl, appIcon);
renderIconInto(emptyStateIconEl, appIcon);

refreshList();
setInterval(refreshList, POLL_INTERVAL_MS);
