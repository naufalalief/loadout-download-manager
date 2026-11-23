// Frontend for the download queue: polls `list_downloads`/`list_extract_jobs`
// and renders them as table rows, plus the always-visible add-link bar and
// the Settings / Browser setup pages. Plain JS, no framework, no build step
// -- `window.__TAURI__` is exposed because `withGlobalTauri` is enabled in
// `tauri.conf.json`.

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
const checkYtdlpUpdateBtn = document.getElementById("check-ytdlp-update-btn");
const ytdlpUpdateStatusEl = document.getElementById("ytdlp-update-status");
const allowNewPairingBtn = document.getElementById("allow-new-pairing-btn");
const allowPairingStatusEl = document.getElementById("allow-pairing-status");
const pairedProfilesListEl = document.getElementById("paired-profiles-list");
const pairedProfilesEmptyEl = document.getElementById("paired-profiles-empty");
const pairedProfilesDotEl = document.getElementById("paired-profiles-dot");
const pairedProfilesCountEl = document.getElementById("paired-profiles-count");

const browserSetupBtn = document.getElementById("browser-setup-btn");
const browserSetupErrorEl = document.getElementById("browser-setup-error");
const setupOpenPageBtn = document.getElementById("setup-open-page-btn");
const setupPageStatusEl = document.getElementById("setup-page-status");
const setupOpenFolderBtn = document.getElementById("setup-open-folder-btn");

// The three full-window pages. Exactly one is visible at a time; the
// statusbar sits outside all three and is always shown.
const ledgerPageEl = document.getElementById("ledger-page");
const settingsPageEl = document.getElementById("settings-page");
const browserSetupPageEl = document.getElementById("browser-setup-page");
const PAGES = [ledgerPageEl, settingsPageEl, browserSetupPageEl];
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

// Trailing chevron on an AwaitingFormatChoice row's clickable Status cell
// (rotated by CSS while that row's unfold is open).
const CHOOSE_CHEVRON_MARKUP =
  '<svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg>';

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

// Extract-job statuses that keep a live subprocess around and so can be
// canceled; `Failed` is the only one that can be retried.
const EXTRACT_CANCELABLE_STATUSES = ["Probing", "AwaitingFormatChoice", "Downloading"];

// Extract-job statuses the backend's `remove_extract_job` command accepts
// (mirrors `ExtractJobManager::remove`'s terminal-status check).
const EXTRACT_TERMINAL_STATUSES = ["Completed", "Failed", "Canceled"];

// Statuses (shared by downloads and extract jobs -- the strings happen to
// match exactly) that mean no further transfer will ever happen for this
// row.
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
const EXTRACT_BADGE = {
  Probing: "Fetching info",
  Downloading: "Downloading",
  AwaitingFormatChoice: "Choose a quality",
  Completed: "Completed",
  Failed: "Failed",
  Canceled: "Canceled",
};

// Rows are kept alive across polls, keyed by `rowIdFor`'s id, and only
// their contents are updated in place. Rebuilding the whole list every poll
// could destroy a button between mousedown and mouseup and silently lose
// the click, and would wipe a half-typed speed limit.
const rowsById = new Map();

// Extract-job rows are kept in `rowsById` alongside HTTP download rows,
// keyed by this prefixed id so a yt-dlp job's uuid can never collide with
// an HTTP download's.
function rowIdFor(item) {
  return item.kind === "extract" ? `x:${item.id}` : item.id;
}

function makeCell(className) {
  const el = document.createElement("span");
  el.className = className;
  return el;
}

function createRow(item) {
  const rowId = rowIdFor(item);

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
    if (!current || current.status !== "Completed") return;
    const path = current.kind === "extract" ? current.final_path : current.dest_path;
    if (!path) return;
    invoke("open_path_in_default_app", { path }).catch((err) =>
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
// Same choice for both kinds -- an extract job's finished file is just as
// permanently gone once deleted as an HTTP download's.
async function removeItem(item) {
  const isExtract = item.kind === "extract";
  const removeCmd = isExtract ? "remove_extract_job" : "remove_download";
  const removeAndDeleteCmd = isExtract ? "remove_extract_job_and_delete_file" : "remove_download_and_delete_file";

  if (item.status === "Completed") {
    const name = isExtract ? item.title || item.source_url : item.filename;
    const deleteFile = await confirmModal({
      message: `Delete the downloaded file "${name}" too?`,
      confirmLabel: "Delete file",
      cancelLabel: "Keep file",
      danger: true,
    });
    if (deleteFile) {
      await invoke(removeAndDeleteCmd, { id: item.id });
      return;
    }
  }
  await invoke(removeCmd, { id: item.id });
}

function actionsFor(item) {
  return item.kind === "extract" ? actionsForExtract(item) : actionsForDownload(item);
}

// Each entry is either `{ kind: "invoke", label, onClick, danger }` (an
// action link that calls `invoke` and refreshes -- see `makeActionLink`) or
// `{ kind: "unfold", label }` (only opens/closes this row's unfold, never
// calls `invoke` itself).
function actionsForDownload(download) {
  const id = download.id;
  const actions = [];
  if (download.status === "Downloading") {
    actions.push({ kind: "invoke", label: "Pause", onClick: () => invoke("pause_download", { id }) });
  }
  if (download.status === "Paused") {
    actions.push({ kind: "invoke", label: "Resume", onClick: () => invoke("resume_download", { id }) });
  }
  if (download.status === "Failed") {
    actions.push({ kind: "invoke", label: "Retry", onClick: () => invoke("retry_download", { id }) });
  }
  // The per-download speed limit is no longer a hover action -- it lives in
  // the row's unfold (see `buildUnfoldContent`, same `limitable` rule).
  if (["Queued", "Downloading", "Paused"].includes(download.status)) {
    actions.push({ kind: "invoke", label: "Cancel", onClick: () => invoke("cancel_download", { id }), danger: true });
  }
  // Gated to terminal statuses, same as extract jobs' Remove
  // (EXTRACT_TERMINAL_STATUSES) below.
  if (TERMINAL_STATUSES.includes(download.status)) {
    actions.push({
      kind: "invoke",
      label: "Remove",
      onClick: () => removeItem(download),
      danger: download.status === "Failed",
    });
  }
  return actions;
}

// Extract jobs only ever get Cancel (while a subprocess could still be
// running), Retry (once Failed), and Remove (once in any terminal status)
// -- never Pause/Resume (yt-dlp can't be paused mid-probe/download) and
// never a speed limit (no backend command exists for an extract job's
// speed). `AwaitingFormatChoice` additionally gets "Choose", which only
// opens this row's unfold on the quality picker: the Status cell's own
// click-to-choose affordance is hidden while the row is hovered (CSS swaps
// `.acts` in for `.c-status`), so this is the hover-time way to reach it.
function actionsForExtract(job) {
  const id = job.id;
  const actions = [];
  if (job.status === "AwaitingFormatChoice") {
    actions.push({ kind: "unfold", label: "Choose", primary: true });
  }
  if (EXTRACT_CANCELABLE_STATUSES.includes(job.status)) {
    actions.push({ kind: "invoke", label: "Cancel", onClick: () => invoke("cancel_extract_job", { id }), danger: true });
  }
  if (job.status === "Failed") {
    actions.push({ kind: "invoke", label: "Retry", onClick: () => invoke("retry_extract_job", { id }) });
  }
  if (EXTRACT_TERMINAL_STATUSES.includes(job.status)) {
    actions.push({ kind: "invoke", label: "Remove", onClick: () => removeItem(job) });
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
// (mirrors `actionsForDownload`/`actionsForExtract` above), then Remove. An
// empty group is never included, so there's never a leading/trailing/
// doubled divider. Returns an array of DOM elements.
function contextMenuItemsFor(item) {
  const isExtract = item.kind === "extract";
  const id = item.id;
  const rowId = rowIdFor(item);
  const groups = [];

  const filePath = isExtract ? item.final_path : item.dest_path;
  if (item.status === "Completed" && filePath) {
    groups.push([
      { label: "Open file", icon: "open", onClick: () => invoke("open_path_in_default_app", { path: filePath }) },
      { label: "Open file location", icon: "folder", onClick: () => invoke("reveal_path_in_file_manager", { path: filePath }) },
    ]);
  }

  const linkUrl = isExtract ? item.source_url : item.url;
  if (linkUrl) {
    groups.push([
      {
        label: "Copy download link",
        icon: "link",
        skipRefresh: true,
        onClick: () => navigator.clipboard.writeText(linkUrl),
      },
    ]);
  }

  const statusGroup = [];
  if (isExtract) {
    if (item.status === "Failed") {
      statusGroup.push({ label: "Retry", icon: "retry", onClick: () => invoke("retry_extract_job", { id }) });
    }
    if (EXTRACT_CANCELABLE_STATUSES.includes(item.status)) {
      statusGroup.push({ label: "Cancel", icon: "cancel", danger: true, onClick: () => invoke("cancel_extract_job", { id }) });
    }
  } else {
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
  }
  if (statusGroup.length) groups.push(statusGroup);

  const terminal = isExtract
    ? EXTRACT_TERMINAL_STATUSES.includes(item.status)
    : TERMINAL_STATUSES.includes(item.status);
  if (terminal) {
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
  const rowId = rowIdFor(download);
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

// The quality picker: a title plus one `.uf-opt` per offered format. One
// click picks and starts via the existing `choose_extract_format` call.
function makeQualityPicker(job) {
  const rowId = rowIdFor(job);
  const title = document.createElement("div");
  title.className = "uf-title";
  title.textContent = "Choose a quality";
  const opts = job.formats.map((format) => {
    const opt = document.createElement("button");
    opt.type = "button";
    opt.className = "uf-opt";
    const label = document.createElement("span");
    label.textContent = format.label;
    opt.appendChild(label);
    let busy = false;
    opt.addEventListener("click", async () => {
      if (busy) return;
      busy = true;
      try {
        await invoke("choose_extract_format", {
          id: job.id,
          formatLabel: format.label,
        });
        closeUnfold(rowId);
        await refreshList();
      } catch (err) {
        console.error("choose_extract_format failed:", err);
      } finally {
        busy = false;
      }
    });
    return opt;
  });
  return [title, ...opts];
}

// Everything a row's unfold shows, for its current item:
//  - an extract job awaiting a format choice: the quality picker first,
//    then (after a divider) the usual right-click entries;
//  - otherwise: the error line (Failed only), the speed-limit option
//    (downloads with today's `limitable` rule: every status except
//    Completed/Canceled), then the usual right-click entries.
function buildUnfoldContent(item) {
  const menuItems = contextMenuItemsFor(item);

  if (item.kind === "extract" && item.status === "AwaitingFormatChoice") {
    const picker = makeQualityPicker(item);
    return menuItems.length ? [...picker, makeUnfoldDivider(), ...menuItems] : picker;
  }

  const content = [];
  if (item.status === "Failed") {
    content.push(makeErrorLine(item.error || "Failed"));
  }
  if (item.kind !== "extract") {
    const limitable = !["Completed", "Canceled"].includes(item.status);
    if (limitable) {
      content.push(makeSpeedLimitOption(item));
      if (menuItems.length) content.push(makeUnfoldDivider());
    }
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

function toggleUnfold(rowId) {
  if (openUnfoldIds.has(rowId)) {
    closeUnfold(rowId);
    return;
  }
  const current = rowsById.get(rowId)?.currentItem;
  if (current) openUnfold(rowId, buildUnfoldContent(current));
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
  // status, `dest_path`/`final_path`, `url`/`source_url` -- not whatever
  // the row happened to show when it was first created.
  els.currentItem = item;

  const isExtract = item.kind === "extract";
  const nameText = isExtract ? item.title || item.source_url : item.filename;
  if (els.cName.textContent !== nameText) {
    els.cName.textContent = nameText;
    els.cName.title = nameText;
  }

  const sizeText = sizeCellText(item);
  if (els.cSize.textContent !== sizeText) els.cSize.textContent = sizeText;

  const dateText = formatDateAdded(item.created_at);
  if (els.cDate.textContent !== dateText) els.cDate.textContent = dateText;

  updateProgressCell(els, item);

  if (isExtract) {
    updateExtractRow(els, item);
  } else {
    updateDownloadRow(els, item);
  }
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
// Failed/Paused. An AwaitingFormatChoice row's cell becomes clickable
// (`.choose` + chevron) and opens the quality picker. Rebuilt only when the
// status changes. The raw error text never shows here -- only in the unfold.
function updateStatusCell(els, item, word) {
  if (els.statusKey === item.status) return;
  els.statusKey = item.status;

  const choose = item.kind === "extract" && item.status === "AwaitingFormatChoice";
  els.cStatus.classList.toggle("failed", item.status === "Failed");
  els.cStatus.classList.toggle("choose", choose);
  els.cStatus.innerHTML = statusIconMarkup(item.status);
  const wordEl = document.createElement("span");
  wordEl.textContent = word;
  els.cStatus.appendChild(wordEl);
  if (choose) {
    els.cStatus.insertAdjacentHTML("beforeend", CHOOSE_CHEVRON_MARKUP);
    els.cStatus.onclick = () => toggleUnfold(els.rowId);
  } else {
    els.cStatus.onclick = null;
  }
}

// Hover action links only change when the status does, so they are only
// rebuilt then -- a click in progress is never interrupted by an ordinary
// poll. A status change also closes this row's unfold, whose contents were
// built for the old status.
function updateActions(els, item, actionsKey) {
  if (els.actionsKey === actionsKey) return;
  const firstBuild = els.actionsKey === null;
  els.actionsKey = actionsKey;
  els.acts.replaceChildren(...actionsFor(item).map((action) => buildActionLink(els, action)));
  if (!firstBuild && openUnfoldIds.has(els.rowId)) closeUnfold(els.rowId);
}

function updateDownloadRow(els, download) {
  updateStatusCell(els, download, DOWNLOAD_BADGE[download.status] ?? "");
  updateActions(els, download, `${download.status}|${download.filename}`);
}

function updateExtractRow(els, job) {
  updateStatusCell(els, job, EXTRACT_BADGE[job.status] ?? "");
  updateActions(els, job, `${job.status}|${job.title}`);
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

function buildActionLink(els, action) {
  if (action.kind === "unfold") {
    const link = document.createElement("button");
    link.type = "button";
    if (action.primary) link.className = "primary";
    link.textContent = action.label;
    link.addEventListener("click", () => toggleUnfold(els.rowId));
    return link;
  }
  return makeActionLink(action.label, action.onClick, action.danger);
}

// Last-known-good lists, used so a transient failure of one `invoke` call
// below leaves that kind's rows exactly as they were on screen instead of
// the merged `items` list silently omitting that kind and the prune loop
// deleting every one of its rows for a single missed poll.
let lastDownloads = [];
let lastExtractJobs = [];

async function refreshList() {
  let downloads = lastDownloads;
  try {
    downloads = (await invoke("list_downloads")) || [];
    lastDownloads = downloads;
  } catch (err) {
    console.error("Failed to list downloads:", err);
  }

  let extractJobs = lastExtractJobs;
  try {
    extractJobs = (await invoke("list_extract_jobs")) || [];
    lastExtractJobs = extractJobs;
  } catch (err) {
    console.error("Failed to list extract jobs:", err);
  }

  const items = [
    ...downloads.map((download) => ({ ...download, kind: "http" })),
    ...extractJobs.map((job) => ({ ...job, kind: "extract" })),
  ];

  const isEmpty = items.length === 0;
  emptyStateEl.classList.toggle("hidden", !isEmpty);
  queueListEl.classList.toggle("hidden", isEmpty);

  const seen = new Set();
  let previous = null;
  for (const item of items) {
    const rowId = rowIdFor(item);
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

  updateStatusBar(items);
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
    combinedSpeed += rowsById.get(rowIdFor(item))?.speedLast ?? 0;
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

// Renders a paired-profile's `paired_at` (Unix seconds, from the backend --
// see `PairedProfile`'s doc comment) in the viewer's own locale/timezone.
function formatPairedAt(unixSeconds) {
  try {
    return new Date(unixSeconds * 1000).toLocaleString(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    });
  } catch (err) {
    return "";
  }
}

// One `.prow` in the paired-browsers list: the name (click to rename in
// place via `rename_paired_profile`), its paired date, and a hover-only
// "Forget" link (confirm, then `forget_paired_profile`).
function buildProfileRow(profile) {
  const row = document.createElement("div");
  row.className = "prow";
  // Focusable for the same reason `.trow` is (see createRow) -- reveals
  // the hover-only "Forget" button via `.prow:focus-within` in styles.css.
  row.tabIndex = 0;

  const left = document.createElement("div");
  const name = document.createElement("span");
  name.className = "pname";
  name.textContent = profile.label;
  name.title = "Click to rename";
  name.style.cursor = "text";
  const date = document.createElement("span");
  date.className = "pdate";
  date.textContent = `paired ${formatPairedAt(profile.paired_at)}`;
  left.append(name, date);

  name.addEventListener("click", () => {
    const input = document.createElement("input");
    input.type = "text";
    input.className = "pname";
    input.value = name.textContent;
    let finished = false;
    const finish = async (commit) => {
      if (finished) return;
      finished = true;
      const value = commit ? input.value.trim() || name.textContent : name.textContent;
      name.textContent = value;
      input.replaceWith(name);
      if (!commit) return;
      try {
        await invoke("rename_paired_profile", { id: profile.id, label: value });
      } catch (err) {
        console.error("rename_paired_profile failed:", err);
      }
    };
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        finish(true);
      } else if (event.key === "Escape") {
        finish(false);
      }
    });
    input.addEventListener("blur", () => finish(true));
    name.replaceWith(input);
    input.focus();
    input.select();
  });

  const forget = document.createElement("button");
  forget.type = "button";
  forget.className = "unpair";
  forget.textContent = "Forget";
  let busy = false;
  forget.addEventListener("click", async () => {
    if (busy) return;
    const confirmed = await confirmModal({
      message: `Forget "${name.textContent}"? That browser will need to pair again before it can send downloads here.`,
      confirmLabel: "Forget",
      danger: true,
    });
    if (!confirmed) return;
    busy = true;
    try {
      await invoke("forget_paired_profile", { id: profile.id });
      await refreshPairedProfiles();
    } catch (err) {
      console.error("forget_paired_profile failed:", err);
      busy = false;
    }
  });

  row.append(left, forget);
  return row;
}

// Rebuilds the Settings page's "Paired browsers" list (and its count row)
// from scratch -- called whenever Settings opens and after any forget,
// never kept in sync incrementally like the queue rows above.
async function refreshPairedProfiles() {
  let profiles = [];
  try {
    profiles = (await invoke("list_paired_profiles")) || [];
  } catch (err) {
    console.error("list_paired_profiles failed:", err);
  }

  pairedProfilesEmptyEl.classList.toggle("hidden", profiles.length > 0);
  pairedProfilesCountEl.textContent = `${profiles.length} browser${profiles.length === 1 ? "" : "s"} paired`;
  pairedProfilesDotEl.classList.toggle("off", profiles.length === 0);
  pairedProfilesListEl.replaceChildren(...profiles.map(buildProfileRow));
}

const ALLOW_PAIRING_IDLE_TEXT = "Lets one more browser profile connect, for 2 minutes";
let pairingCountdownId = null;

function formatCountdown(totalSeconds) {
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

// Drives the "Allow" button's live countdown from `remaining` seconds down
// to 0, ticking once a second -- called both right after the button is
// clicked (with the duration `allow_new_pairing` itself just opened) and
// when Settings is reopened while an earlier click's window is still
// counting down (with whatever `pairing_mode_remaining_seconds` reports),
// so the two can never show different numbers.
function startPairingCountdown(remaining) {
  if (pairingCountdownId != null) {
    clearInterval(pairingCountdownId);
  }
  allowNewPairingBtn.disabled = true;

  const tick = () => {
    if (remaining <= 0) {
      clearInterval(pairingCountdownId);
      pairingCountdownId = null;
      allowNewPairingBtn.disabled = false;
      allowPairingStatusEl.textContent = ALLOW_PAIRING_IDLE_TEXT;
      return;
    }
    allowPairingStatusEl.textContent = `Pairing open — ${formatCountdown(remaining)} left`;
    remaining -= 1;
  };
  tick();
  pairingCountdownId = setInterval(tick, 1000);
}

allowNewPairingBtn.addEventListener("click", async () => {
  allowNewPairingBtn.disabled = true;
  try {
    const seconds = await invoke("allow_new_pairing");
    startPairingCountdown(seconds);
  } catch (err) {
    console.error("allow_new_pairing failed:", err);
    allowNewPairingBtn.disabled = false;
  }
});

function setStatus(el, message, isError) {
  el.textContent = message;
  el.classList.toggle("error", Boolean(isError));
}

// Settings gear (in the addbar): loads the current settings into the
// Settings page's controls, then swaps to that page.
shedBtn.addEventListener("click", async () => {
  hideError(settingsErrorEl);
  ytdlpUpdateStatusEl.textContent = "";
  ytdlpUpdateStatusEl.classList.remove("error");
  try {
    const settings = await invoke("get_settings");
    currentSettings = settings;
    settingsDownloadDirEl.value = settings.download_dir;
    settingsMaxConcurrentEl.value = settings.max_concurrent;
    settingsSpeedLimitEl.value = settings.default_speed_limit_bytes_per_sec ?? "";
    showPage(settingsPageEl);
    await refreshPairedProfiles();
    const remaining = await invoke("pairing_mode_remaining_seconds");
    if (remaining != null && remaining > 0) {
      startPairingCountdown(remaining);
    } else if (pairingCountdownId == null) {
      allowNewPairingBtn.disabled = false;
      allowPairingStatusEl.textContent = ALLOW_PAIRING_IDLE_TEXT;
    }
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

checkYtdlpUpdateBtn.addEventListener("click", async () => {
  checkYtdlpUpdateBtn.disabled = true;
  ytdlpUpdateStatusEl.classList.remove("error");
  ytdlpUpdateStatusEl.textContent = "Checking…";
  try {
    const message = await invoke("check_ytdlp_update");
    ytdlpUpdateStatusEl.textContent = message;
  } catch (err) {
    ytdlpUpdateStatusEl.textContent = String(err);
    ytdlpUpdateStatusEl.classList.add("error");
  } finally {
    checkYtdlpUpdateBtn.disabled = false;
  }
});

// Settings' "Setup guide" link opens the Browser setup page; its Close and
// "Installed it" both return to Settings (no persisted "has the user set
// up the extension" state exists, so there's nothing to tell them apart).
browserSetupBtn.addEventListener("click", () => {
  hideError(browserSetupErrorEl);
  setStatus(setupPageStatusEl, "", false);
  showPage(browserSetupPageEl);
});
document.getElementById("browser-setup-close-btn").addEventListener("click", () => {
  showPage(settingsPageEl);
});
document.getElementById("setup-done-btn").addEventListener("click", () => {
  showPage(settingsPageEl);
});

// "Open page" -- copies chrome://extensions to the clipboard via the real
// `open_extensions_page` command. That command is clipboard-only on
// purpose: chrome://extensions has no registered OS protocol handler on a
// typical system, so attempting to open it would likely pop the OS's own
// "choose an app" dialog instead -- see that command's own doc comment. The
// confirmation is shown via `setStatus` (not `showError`) so this routine
// confirmation never renders as if something had gone wrong.
setupOpenPageBtn.addEventListener("click", async () => {
  setupOpenPageBtn.disabled = true;
  try {
    const message = await invoke("open_extensions_page");
    setStatus(setupPageStatusEl, message, false);
  } catch (err) {
    setStatus(setupPageStatusEl, String(err), true);
  } finally {
    setupOpenPageBtn.disabled = false;
  }
});

// "Open folder" -- opens the bundled extension/ folder in the OS file
// explorer. A successful open is its own confirmation (the explorer window
// appearing), so only a failure needs an inline message here.
setupOpenFolderBtn.addEventListener("click", async () => {
  hideError(browserSetupErrorEl);
  setupOpenFolderBtn.disabled = true;
  try {
    await invoke("open_extension_folder");
  } catch (err) {
    showError(browserSetupErrorEl, String(err));
  } finally {
    setupOpenFolderBtn.disabled = false;
  }
});

// Settings gear, addbar arrow, and empty-state icon: all static (never
// change at runtime), so rendered once here.
renderIconInto(shedIconEl, settingsIcon);
renderIconInto(addbarIconEl, appIcon);
renderIconInto(emptyStateIconEl, appIcon);

refreshList();
setInterval(refreshList, POLL_INTERVAL_MS);
