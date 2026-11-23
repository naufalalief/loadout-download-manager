console.log('background worker loaded');

const DM_APP_URL = 'http://127.0.0.1:57431/downloads';
const DM_EXTRACT_URL = 'http://127.0.0.1:57431/extract';
const PAIR_URL = 'http://127.0.0.1:57431/pair';

/** Timeout for the handoff POST itself. Longer than options.js's /ping
 * check (1500ms) because this is a POST the app may take a moment to
 * process (parsing the body, validating the token, queuing the download)
 * rather than a trivial liveness check. */
const SEND_TIMEOUT_MS = 5000;

/** Shared in-flight `/pair` attempt, so a burst of downloads/extracts that
 * all arrive before pairing has happened share one request instead of each
 * firing their own -- see `ensurePaired()`. */
let pairingPromise = null;

/** How close together two `onDeterminingFilename` events for the same URL
 * have to land to be treated as a double-click/double-trigger race rather
 * than two separate, intentional downloads of the same file -- see
 * `isDuplicateDownload()`. */
const DEDUPE_WINDOW_MS = 2000;

/** url -> timestamp (ms) of the last time that url was seen by
 * `isDuplicateDownload()`. Module-level by design: the whole point is to
 * catch a second browser-level download event for a URL the FIRST event's
 * (still-async) handling hasn't finished with yet. */
const recentDownloadUrls = new Map();

/**
 * Whether `url` was already seen by this function within the last
 * `DEDUPE_WINDOW_MS` -- i.e. this is almost certainly the same link
 * double-clicked (or otherwise double-triggered) rather than a second,
 * intentional download of the same file. Always records `url`'s current
 * timestamp as a side effect, whatever the answer, so the next call's
 * window is measured from this call.
 *
 * @param {string} url
 * @returns {boolean}
 */
function isDuplicateDownload(url) {
  const now = Date.now();
  // Opportunistic prune so this map never grows unbounded over a long
  // browser session -- cheap relative to how rarely downloads fire.
  for (const [key, ts] of recentDownloadUrls) {
    if (now - ts > DEDUPE_WINDOW_MS) {
      recentDownloadUrls.delete(key);
    }
  }
  const last = recentDownloadUrls.get(url);
  recentDownloadUrls.set(url, now);
  return last !== undefined && now - last < DEDUPE_WINDOW_MS;
}

/**
 * Returns a usable pairing token, auto-pairing with the app on first use if
 * none is stored yet.
 *
 * Safe to do without a user click: the app's `/pair` endpoint is
 * trust-on-first-use (whoever calls it first wins) -- options.js's manual
 * "Pair" button added no security of its own, just a confirmation click.
 * That button stays as a fallback/retry path (e.g. the app wasn't running
 * yet the first time this was tried), it is never removed, only no longer
 * required before the extension does anything useful.
 *
 * @returns {Promise<string|undefined>} the token, or `undefined` if none is
 *   available (not paired, and auto-pairing failed or the app isn't
 *   reachable).
 */
async function ensurePaired() {
  let stored;
  try {
    stored = await chrome.storage.local.get('dmToken');
  } catch (err) {
    stored = undefined;
  }
  if (stored && stored.dmToken) {
    return stored.dmToken;
  }

  if (!pairingPromise) {
    pairingPromise = (async () => {
      try {
        const resp = await fetch(PAIR_URL, { method: 'POST' });
        if (resp.status !== 200) {
          // Covers "already paired elsewhere" (403) and any other
          // non-success status the same way: no token, caller falls back.
          return undefined;
        }
        const data = await resp.json();
        const token = data && data.token;
        if (!token) {
          return undefined;
        }
        await chrome.storage.local.set({ dmToken: token });
        return token;
      } catch (err) {
        // App not running, network error, malformed JSON -- all the same
        // outcome: no token this time, caller falls back.
        return undefined;
      }
    })();
  }

  try {
    return await pairingPromise;
  } finally {
    pairingPromise = null;
  }
}

/**
 * Hands a captured download off to the core app's local server.
 *
 * Fails safe in every case: if no token is available even after
 * `ensurePaired()`'s auto-pair attempt, it returns `false` immediately
 * without attempting the handoff fetch at all. Otherwise it POSTs the
 * capture to the app's `/downloads` endpoint and resolves `true` only on a
 * `201` response -- any other status, a network error (including the app
 * simply not running, which makes `fetch()` reject rather than resolve),
 * or the request timing out (see `SEND_TIMEOUT_MS`; a hung app must not
 * block `suggest()` indefinitely) resolves `false` instead of throwing, so
 * a caller can always safely fall back to letting the browser's native
 * download proceed.
 *
 * @param {string} url - the download's URL.
 * @param {string} filename - the filename the browser determined for it.
 * @param {{cookieHeader: string, referer: string, userAgent: string}} headers
 *   - output of captureHeaders() for this download.
 * @returns {Promise<boolean>}
 */
async function sendToApp(url, filename, headers) {
  const dmToken = await ensurePaired();
  if (!dmToken) {
    return false;
  }

  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), SEND_TIMEOUT_MS);
  try {
    const response = await fetch(DM_APP_URL, {
      method: 'POST',
      signal: controller.signal,
      headers: {
        'Content-Type': 'application/json',
        'X-Dm-Token': dmToken,
      },
      body: JSON.stringify({
        url,
        filename,
        headers: {
          Cookie: headers.cookieHeader,
          Referer: headers.referer,
          'User-Agent': headers.userAgent,
        },
      }),
    });
    if (response.status === 201) {
      chrome.storage.local.set({ lastSent: { label: filename, sentAt: Date.now() } }).catch(() => {});
      return true;
    }
    return false;
  } catch (err) {
    // The app may simply not be running (fetch to a closed/refused port
    // rejects), the request may have timed out (the AbortController fires,
    // which also rejects fetch()), or some other network error occurred.
    // Either way this must resolve to `false`, never propagate as an
    // uncaught rejection, so the caller's fallback-to-native-download path
    // always runs. A possible duplicate download (if the app actually did
    // queue it right as the timeout fired) is the correct failure
    // direction over losing the download entirely.
    return false;
  } finally {
    clearTimeout(timeoutId);
  }
}

/**
 * Captures the cookie header, referer, and user agent to accompany a
 * handoff for a single download.
 *
 * Self-contained by design: everything it needs comes in as arguments and
 * everything it produces comes back in the resolved object. It reads no
 * module-level mutable state and writes none, so two downloads captured
 * back-to-back (before either's async work resolves) run as independent,
 * non-interfering calls instead of racing on shared state.
 *
 * @param {string} url - the download's URL; cookies are looked up for
 *   whatever domain this URL resolves to.
 * @param {string} referrer - `downloadItem.referrer`, the actual referrer
 *   the browser used for the download request itself (empty string if the
 *   browser didn't report one). This is the real value, not a guess: it is
 *   used as-is, with no active-tab fallback, so a background-tab download,
 *   a closed-tab download, or a tab-switch race can never cause the wrong
 *   page's URL to be sent as the referer.
 * @returns {Promise<{cookieHeader: string, referer: string, userAgent: string}>}
 */
async function captureHeaders(url, referrer) {
  let cookieHeader = '';
  try {
    const cookies = await chrome.cookies.getAll({ url });
    // A domain with no matching cookies is a normal, successful outcome:
    // `cookies` is simply an empty array, which joins to ''. We must not
    // treat that as an error or skip the capture.
    cookieHeader = cookies.map((cookie) => `${cookie.name}=${cookie.value}`).join('; ');
  } catch (err) {
    cookieHeader = '';
  }

  const refererValue = (typeof referrer === 'string' && referrer) || '';

  let userAgent = '';
  try {
    userAgent = (typeof navigator !== 'undefined' && navigator.userAgent) || '';
  } catch (err) {
    userAgent = '';
  }

  return { cookieHeader, referer: refererValue, userAgent };
}

/**
 * Hands a video-extraction request off to the core app's local server.
 *
 * Mirrors sendToApp()'s fail-safe shape above, with two differences: it
 * posts to `/extract` instead of `/downloads` (the page URL to probe with
 * yt-dlp, not a direct file URL), and it returns a `{ok, error?}` result
 * instead of a bare boolean, so the content script's button can surface
 * a short reason for a failure if it chooses to.
 *
 * Fails safe in every case: if no token is available even after
 * `ensurePaired()`'s auto-pair attempt, it returns `{ok: false}`
 * immediately without attempting a fetch at all. Otherwise it POSTs and
 * resolves `{ok: true}` only on a `201` response -- any other status, a
 * network error (including the app simply not running), or the request
 * timing out (see `SEND_TIMEOUT_MS`) resolves `{ok: false, error: "..."}`
 * instead of throwing.
 *
 * @param {string} pageUrl - the page URL to extract from. Callers must
 *   resolve this themselves, as close as possible to the moment the user
 *   asked for the download -- this function does no resolution of its
 *   own and just posts whatever it's given.
 * @param {{cookieHeader: string, referer: string, userAgent: string}} headers
 *   - output of captureHeaders() for this pageUrl.
 * @returns {Promise<{ok: boolean, error?: string}>}
 */
async function sendExtractProbe(pageUrl, headers) {
  const dmToken = await ensurePaired();
  if (!dmToken) {
    return { ok: false, error: 'Not paired with the app yet.' };
  }

  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), SEND_TIMEOUT_MS);
  try {
    const response = await fetch(DM_EXTRACT_URL, {
      method: 'POST',
      signal: controller.signal,
      headers: {
        'Content-Type': 'application/json',
        'X-Dm-Token': dmToken,
      },
      body: JSON.stringify({
        url: pageUrl,
        headers: {
          Cookie: headers.cookieHeader,
          Referer: headers.referer,
          'User-Agent': headers.userAgent,
        },
      }),
    });
    if (response.status === 201) {
      chrome.storage.local.set({ lastSent: { label: pageUrl, sentAt: Date.now() } }).catch(() => {});
      return { ok: true };
    }
    return { ok: false, error: `Extract request failed (status ${response.status}).` };
  } catch (err) {
    // The app may simply not be running, the request may have timed out,
    // or some other network error occurred. Either way this must resolve
    // to a failure result, never propagate as an uncaught rejection.
    return { ok: false, error: 'Could not reach the app. Is it running?' };
  } finally {
    clearTimeout(timeoutId);
  }
}

/**
 * Handles messages from content scripts.
 *
 * On `{type: "extractVideo", pageUrl}`: captures headers for `pageUrl`
 * (empty referrer -- there is no meaningful "download referrer" concept
 * for a page-URL extraction, unlike the direct-file-download case
 * `captureHeaders` was originally written for) and hands off to
 * `sendExtractProbe()`. Returns `true` so Chrome keeps the message
 * channel open while that async work runs, and eventually calls
 * `sendResponse` with `{ok: boolean, error?: string}`.
 *
 * Never throws: any failure anywhere in the async work below resolves to
 * `sendResponse({ok: false, ...})` rather than leaving the sender's
 * `sendMessage()` promise hanging forever.
 */
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || message.type !== 'extractVideo') {
    // Not a message this listener handles.
    return false;
  }

  (async () => {
    try {
      const headers = await captureHeaders(message.pageUrl, '');
      const result = await sendExtractProbe(message.pageUrl, headers);
      sendResponse(result);
    } catch (err) {
      sendResponse({ ok: false, error: 'Unexpected error handling extract request.' });
    }
  })();

  return true;
});

chrome.downloads.onDeterminingFilename.addListener((downloadItem, suggest) => {
  // Local to this single invocation only: no module-level state, so a
  // second download arriving before this one's async work finishes gets
  // its own closure and cannot clobber or be clobbered by this one.
  let settled = false;
  const fallbackToNativeDownload = () => {
    if (!settled) {
      // No arguments: tells the browser to proceed with its own, unmodified
      // native download. This is the safety net for every failure mode
      // below (app not running, not paired, non-http(s) URL, network
      // error, bad response, thrown exception, anything) -- the download
      // must never be lost.
      suggest();
      // Set only after suggest() has actually run without throwing, so a
      // synchronous throw from suggest() itself leaves `settled` false
      // (accurately reflecting that it was never successfully invoked)
      // rather than suppressing a higher-level catch's own fallback attempt.
      settled = true;
    }
  };

  // C0: same URL seen again within DEDUPE_WINDOW_MS -- almost certainly a
  // double-click/double-trigger race, not two intentional downloads.
  // Cancel this second download outright so it never produces a duplicate
  // file, whatever the first one's own outcome ends up being (handed off
  // to the app, or falling back to the browser's native download).
  // Checked before anything else, including the scheme check below, since
  // a duplicate of any scheme is still a duplicate.
  const dedupeKey = downloadItem.finalUrl || downloadItem.url;
  if (isDuplicateDownload(dedupeKey)) {
    chrome.downloads.cancel(downloadItem.id).catch(() => {});
    fallbackToNativeDownload();
    return true;
  }

  // C1: a handoff can only ever work for http(s) URLs -- the app's reqwest
  // client has no way to fetch a `blob:`/`data:`/other-scheme URL (those
  // only exist inside the page that created them), and by the time this
  // listener fires the browser has already started the download, so if we
  // cancel it assuming the app will take over, the download is gone for
  // good once the app's fetch fails. Checked synchronously, before any
  // async work (including cookie capture) even starts.
  let scheme = '';
  try {
    scheme = new URL(downloadItem.finalUrl || downloadItem.url).protocol;
  } catch (err) {
    scheme = '';
  }
  if (scheme !== 'http:' && scheme !== 'https:') {
    fallbackToNativeDownload();
    return true;
  }

  (async () => {
    try {
      // M1: check (and, on first use, auto-establish) pairing before doing
      // anything else. If no token ends up available, no handoff can
      // possibly happen, so we must not read cookies (or anything else)
      // for this URL at all -- never read cookies unless a handoff can
      // actually happen. `sendToApp()` below calls `ensurePaired()` again,
      // but that's a cache hit once this has already succeeded (or a
      // shared in-flight attempt if one is still running), not a second
      // `/pair` request.
      const dmToken = await ensurePaired();

      if (!dmToken) {
        fallbackToNativeDownload();
        return;
      }

      const headers = await captureHeaders(downloadItem.url, downloadItem.referrer);

      const handoffSucceeded = await sendToApp(downloadItem.url, downloadItem.filename, headers);

      if (handoffSucceeded) {
        // The core app has taken over the download: stop the browser from
        // also saving it, then tell the browser what we'd have called it
        // (purely cosmetic at this point -- the file itself is cancelled).
        await chrome.downloads.cancel(downloadItem.id);
        if (!settled) {
          suggest({ filename: downloadItem.filename, conflictAction: 'uniquify' });
          // Set only after suggest() has actually run without throwing --
          // see the matching comment in fallbackToNativeDownload() above.
          settled = true;
        }
      } else {
        fallbackToNativeDownload();
      }
    } catch (err) {
      // Anything going wrong anywhere above (cookie lookup, the handoff
      // call, cancel, etc.) must never block or lose the download -- fall
      // through to the browser's native handling.
      fallbackToNativeDownload();
    }
  })();

  // Required so Chrome keeps this event open while we call suggest()
  // asynchronously above, instead of finalizing the filename immediately.
  return true;
});
