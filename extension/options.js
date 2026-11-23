// Options page: shows whether the core app is reachable and drives the
// one-time pairing flow against its local handoff server.

const APP_BASE_URL = 'http://127.0.0.1:57431';
const PING_TIMEOUT_MS = 1500;

/**
 * Checks whether the core app's handoff server is reachable.
 * @returns {Promise<boolean>} true only on a 200 response with body "ok";
 *   false on any error/timeout.
 */
async function checkAppRunning() {
  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), PING_TIMEOUT_MS);
  try {
    const resp = await fetch(`${APP_BASE_URL}/ping`, { signal: controller.signal });
    if (resp.status !== 200) {
      return false;
    }
    const text = await resp.text();
    return text === 'ok';
  } catch (err) {
    return false;
  } finally {
    clearTimeout(timeoutId);
  }
}

/**
 * Performs the pairing handshake with the core app.
 * @returns {Promise<{ok: boolean, error?: string}>} On a 200 response,
 *   stores the returned token in chrome.storage.local under "dmToken" and
 *   resolves {ok: true}. On 403 (pairing closed -- either this is the very
 *   first browser ever and something else already claimed that slot, or
 *   it's a later one and the app's "Allow new pairing" window isn't open)
 *   or any network failure, resolves {ok: false, error: "..."} without
 *   touching storage. The app's own 403 body carries a human-readable
 *   `message` explaining exactly what to do (open the app, Settings,
 *   "Allow new pairing") -- shown verbatim when present, since it already
 *   says this more specifically than any fixed string here could.
 */
async function pair() {
  let resp;
  try {
    resp = await fetch(`${APP_BASE_URL}/pair`, { method: 'POST' });
  } catch (err) {
    return { ok: false, error: 'Could not reach the app. Is it running?' };
  }

  if (resp.status === 403) {
    let message;
    try {
      message = (await resp.json()).message;
    } catch (err) {
      message = undefined;
    }
    return {
      ok: false,
      error: message || 'Pairing is closed right now. Open the app\'s Settings and allow a new pairing.',
    };
  }

  if (resp.status !== 200) {
    return { ok: false, error: `Pairing failed (status ${resp.status}).` };
  }

  let data;
  try {
    data = await resp.json();
  } catch (err) {
    return { ok: false, error: 'Pairing failed: malformed response.' };
  }

  const token = data && data.token;
  if (!token) {
    return { ok: false, error: 'Pairing failed: no token returned.' };
  }

  await chrome.storage.local.set({ dmToken: token });
  return { ok: true };
}

/**
 * I3: forgets this extension install's stored pairing token, so the
 * options page shows the "Pair" button again.
 *
 * If a token is currently stored, this first calls the app's `/unpair`
 * with it, which lifts the app's one-time pairing gate (the app only
 * accepts this if the token matches what it currently considers "paired",
 * so an app that's already moved on -- a different token, or not running
 * -- simply ignores it; that failure is not fatal here, since clearing the
 * token locally is the part this function must always do). Only after
 * that does it clear the token from this extension's own storage, so a
 * following "Pair manually" click can succeed immediately instead of
 * hitting 403 "already paired" with no way to recover short of deleting
 * the app's marker file by hand.
 *
 * This still also covers the case where the EXTENSION's identity changed
 * (moved/reinstalled, which mints a new extension id and empty storage):
 * use this button on the OLD install while it's still around, which
 * reaches the app the same way. See the README's "Reset pairing" section
 * for the manual fallback if the app isn't reachable when this runs.
 */
async function forgetPairing() {
  const { dmToken } = await chrome.storage.local.get('dmToken');
  if (dmToken) {
    try {
      await fetch(`${APP_BASE_URL}/unpair`, {
        method: 'POST',
        headers: { 'X-Dm-Token': dmToken },
      });
    } catch (err) {
      // App not reachable -- fall through and forget locally anyway.
    }
  }
  await chrome.storage.local.remove('dmToken');
}

async function initOptionsPage() {
  const statusEl = document.getElementById('status');
  const statusDotEl = document.getElementById('status-dot');
  const pairBtn = document.getElementById('pair-btn');
  const errorEl = document.getElementById('pair-error');
  const forgetBtn = document.getElementById('forget-btn');
  const forgetStatusEl = document.getElementById('forget-status');

  // Visual state only -- the button's label always stays "Pair manually"
  // (the status card above already conveys paired/connected state), it just
  // disables/dims itself once a token is stored.
  const refreshPairButton = async () => {
    const { dmToken } = await chrome.storage.local.get('dmToken');
    pairBtn.disabled = !!dmToken;
  };

  // "Connected" must mean the thing a user clicking "Forget pairing"
  // right above it expects it to mean: are we actually paired right now.
  // Checking only `checkAppRunning()` (the app process being reachable at
  // all) made this card show "Connected" even the instant after
  // "Forget pairing" cleared the stored token -- confusing, since nothing
  // about the status card reacted to the button directly below it. A
  // missing token still auto-pairs invisibly on the next download/extract
  // (see `ensurePaired()`), so this never blocks that; it only makes the
  // status card honest about pairing state in the meantime.
  const refreshStatus = async () => {
    const running = await checkAppRunning();
    const { dmToken } = await chrome.storage.local.get('dmToken');
    const connected = running && !!dmToken;
    statusEl.textContent = connected ? 'Connected to the app' : 'Not connected';
    statusEl.classList.toggle('off', !connected);
    statusDotEl.classList.toggle('off', !connected);
  };

  await refreshPairButton();
  await refreshStatus();

  // `refreshStatus()` otherwise only reruns on a `dmToken` change (below)
  // or this page's own initial load -- neither fires when the APP quits
  // or starts while this page just sits open, so "Connected to the app" could
  // go stale (stay connected after the app was closed, or stay
  // disconnected after it was reopened) until the page was manually
  // reloaded. Polled, not event-driven, because there's no event for
  // "the app's process state changed" to listen for from here.
  const statusPollId = setInterval(refreshStatus, 2000);
  window.addEventListener('unload', () => clearInterval(statusPollId));

  pairBtn.addEventListener('click', async () => {
    errorEl.textContent = '';
    pairBtn.disabled = true;

    const result = await pair();
    if (result.ok) {
      pairBtn.disabled = true;
    } else {
      pairBtn.disabled = false;
      errorEl.textContent = result.error || 'Pairing failed.';
    }
  });

  // background.js may auto-pair (see its `ensurePaired()`) while this page
  // is already open -- e.g. the user triggers a download/extract from
  // another tab right after installing, before ever opening this page.
  // Without this, the button (and the status card) would keep showing
  // their stale state until the page is manually reopened.
  chrome.storage.onChanged.addListener((changes, area) => {
    if (area === 'local' && changes.dmToken) {
      refreshPairButton();
      refreshStatus();
    }
  });

  forgetBtn.addEventListener('click', async () => {
    forgetStatusEl.textContent = '';
    await forgetPairing();
    await refreshPairButton();
    await refreshStatus();
    errorEl.textContent = '';
    forgetStatusEl.textContent =
      'Pairing forgotten. If the app was running, it also forgot this ' +
      'pairing on its side, so "Pair manually" should work right away. ' +
      'If the app wasn\'t reachable just now, you may need to delete its ' +
      'persisted pairing marker file and restart it instead -- see the ' +
      'extension README\'s "Reset pairing" section.';
  });
}

document.addEventListener('DOMContentLoaded', initOptionsPage);
