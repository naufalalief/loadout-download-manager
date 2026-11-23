// Content script: shows a hover-over "Download" button on candidate
// <video> elements and sends the extraction request to the background
// worker.
//
// The one rule every line of this file is written to protect: the URL
// sent for extraction must always reflect reality AT THE MOMENT OF THE
// CLICK, never a value captured earlier (when the button was shown, or
// when the page was first scanned). On an SPA site (YouTube chief among
// them) the page can navigate to a different video via pushState, with
// no full reload, between the moment a user hovers the player and the
// moment they click the button -- if the click handler used a URL
// resolved at hover time, the user would download the WRONG video. See
// resolveTargetUrl() below: it is called fresh, inline, inside the click
// handler itself, and never memoized anywhere.
(function () {
  const MIN_WIDTH_PX = 200;
  const MIN_HEIGHT_PX = 150;
  const MIN_DURATION_SECONDS = 3;
  const HIDE_DELAY_MS = 200;
  const LOCATION_POLL_MS = 1000;

  /**
   * All <video> elements on the page that are plausible "real" videos
   * worth offering a download button for, filtering out tiny/decorative
   * or hidden ones.
   *
   * A video whose duration isn't known yet (readyState hasn't reached
   * HAVE_METADATA, so `.duration` is still NaN) is kept rather than
   * filtered -- we'd rather briefly offer a button for a short video than
   * never offer one for a long video just because it hasn't finished
   * loading metadata yet.
   *
   * @returns {HTMLVideoElement[]}
   */
  function findCandidateVideos() {
    return Array.from(document.querySelectorAll('video')).filter((video) => {
      let rect;
      try {
        rect = video.getBoundingClientRect();
      } catch (err) {
        return false;
      }
      if (rect.width < MIN_WIDTH_PX || rect.height < MIN_HEIGHT_PX) {
        return false;
      }

      let style;
      try {
        style = window.getComputedStyle(video);
      } catch (err) {
        style = null;
      }
      if (style && (style.display === 'none' || style.visibility === 'hidden')) {
        return false;
      }

      // `duration` is NaN until enough metadata has loaded to know it --
      // Number.isFinite(NaN) is false, so an unknown duration falls
      // through this check and the video is kept, per the filter above.
      if (Number.isFinite(video.duration) && video.duration < MIN_DURATION_SECONDS) {
        return false;
      }

      return true;
    });
  }

  /**
   * The URL a download button click for `videoEl` should target, resolved
   * from live DOM/location state RIGHT NOW -- never cached, never
   * precomputed. Call this exactly once, synchronously, at the top of a
   * click handler; do not store its result anywhere that outlives the
   * click.
   *
   * Prefers the href of the nearest ancestor <a href> (common on listing/
   * thumbnail pages, where the video itself isn't "the page"); falls back
   * to the current page URL otherwise. Both `anchor.href` and
   * `window.location.href` are DOM properties that are already resolved
   * to absolute URLs by the browser, so no extra resolution step is
   * needed.
   *
   * @param {HTMLVideoElement} videoEl
   * @returns {string}
   */
  function resolveTargetUrl(videoEl) {
    const anchor = videoEl.closest && videoEl.closest('a[href]');
    if (anchor && anchor.href) {
      return anchor.href;
    }
    return window.location.href;
  }

  // --- Overlay button -------------------------------------------------
  //
  // One shared shadow-DOM button, repositioned/shown over whichever
  // candidate video is currently hovered, rather than one button per
  // video -- simpler to keep in sync and there's only ever one button
  // visible at a time anyway.

  // Loadout's download-arrow icon markup, ported as a static literal from
  // src/icons.js's `appIcon`. Content scripts can't `import` from the main
  // app's src/ (no shared module boundary between the two packages), so
  // the markup is duplicated here. Keep this in sync with src/icons.js's
  // `appIcon` if that shape ever changes.
  const APP_ICON_MARKUP =
    '<path d="M12 3v12m0 0l-4-4m4 4l4-4"/><path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"/>';

  let overlayHost = null;
  let overlayButton = null;
  let overlayLabel = null;
  let currentVideo = null;
  let hideTimer = null;

  function ensureOverlay() {
    if (overlayHost) {
      return;
    }

    const host = document.createElement('div');
    host.style.position = 'fixed';
    host.style.top = '0';
    host.style.left = '0';
    host.style.zIndex = '2147483647';
    host.style.display = 'none';
    // Never let the host's own box (even if some descendant miscomputes
    // size) capture clicks outside the button itself.
    host.style.pointerEvents = 'none';

    const shadow = host.attachShadow({ mode: 'closed' });
    const style = document.createElement('style');
    style.textContent = `
      @keyframes fadein {
        from { opacity: 0; }
        to { opacity: 1; }
      }
      .pill {
        /* No position/top/right here (unlike the spec mockup's static demo
           layout) -- this overlay's position is already fully driven by
           positionOverlayFor() via the fixed-position host div above, and
           a position:absolute offset on the pill itself would fight that
           existing, working tracking logic. */
        display: inline-flex;
        align-items: center;
        gap: 6px;
        height: 30px;
        padding: 0 8px;
        border: none;
        border-radius: 15px;
        background: #0067c0;
        color: #fff;
        box-shadow: 0 1px 3px rgba(0, 0, 0, 0.35), 0 0 0 2px rgba(255, 255, 255, 0.9);
        font-size: 12px;
        white-space: nowrap;
        overflow: hidden;
        cursor: pointer;
        /* A plain fade-in, not the old scale-up "pop" -- that animation's
           transform was still running when positionOverlayFor() measured
           the button right after showing it, which undercounted its real
           width and let it land outside the video on some pages (it now
           measures via offsetWidth, unaffected by transform either way,
           but there's no reason to keep an animation this button no
           longer needs). */
        animation: fadein .12s ease;
        /* The host above sets pointer-events: none so its own box never
           captures clicks outside the button; this opts the pill itself
           back in, same role the old plain button selector's rule played. */
        pointer-events: auto;
      }
      .pill:disabled {
        opacity: 0.65;
        cursor: default;
      }
      .pill .sprite {
        flex-shrink: 0;
        display: block;
        fill: none;
        stroke: #fff;
        stroke-width: 1.8;
        stroke-linecap: round;
        stroke-linejoin: round;
      }
      .pill .label {
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, Roboto, sans-serif;
        font-size: 12px;
        font-weight: 500;
        color: #fff;
        max-width: 0;
        opacity: 0;
        transition: max-width .14s ease, opacity .1s ease;
      }
      .pill.expanded {
        padding: 0 10px 0 8px;
      }
      .pill.expanded .label {
        max-width: 70px;
        opacity: 1;
      }
      .pill.idle {
        background: rgba(0, 0, 0, 0.55);
        box-shadow: 0 1px 3px rgba(0, 0, 0, 0.35), 0 0 0 1.5px rgba(255, 255, 255, 0.5);
      }
    `;
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'pill idle';

    const svgNs = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(svgNs, 'svg');
    svg.setAttribute('class', 'sprite');
    svg.setAttribute('width', '16');
    svg.setAttribute('height', '16');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.innerHTML = APP_ICON_MARKUP;

    const label = document.createElement('span');
    label.className = 'label';
    label.textContent = 'Download';

    button.appendChild(svg);
    button.appendChild(label);

    button.addEventListener('mouseenter', cancelHide);
    button.addEventListener('mouseleave', scheduleHide);
    button.addEventListener('mouseenter', () => {
      button.classList.remove('idle');
      button.classList.add('expanded');
    });
    button.addEventListener('mouseleave', () => {
      button.classList.remove('expanded');
      button.classList.add('idle');
    });
    button.addEventListener('click', onButtonClick);

    shadow.appendChild(style);
    shadow.appendChild(button);

    (document.documentElement || document.body).appendChild(host);

    overlayHost = host;
    overlayButton = button;
    overlayLabel = label;
  }

  /**
   * The video's actual on-screen PIXEL content rectangle -- not
   * necessarily the same as its DOM element's own box. A `<video>` with
   * `object-fit: contain` (common on pages that embed a vertical/Shorts-
   * style video inside a wider, fixed-size card, to show it uncropped)
   * paints its content letterboxed inside that box, offset by
   * `object-position`: the element's `getBoundingClientRect()` still
   * reports the full card, well past where the visible picture actually
   * ends, which anchored the button in the empty letterbox space instead
   * of against the video the user actually sees.
   *
   * @param {HTMLVideoElement} videoEl
   * @returns {DOMRect | {left: number, top: number, right: number, bottom: number, width: number, height: number}}
   */
  function getVisibleVideoRect(videoEl) {
    const rect = videoEl.getBoundingClientRect();
    const vw = videoEl.videoWidth;
    const vh = videoEl.videoHeight;
    if (!vw || !vh || rect.width <= 0 || rect.height <= 0) {
      return rect;
    }
    let style;
    try {
      style = window.getComputedStyle(videoEl);
    } catch (err) {
      return rect;
    }
    const fit = style.objectFit;
    // `cover`/`fill`/`scale-down` either fill the box completely or (for
    // `scale-down`) only ever shrink further, never leaving the kind of
    // persistent empty margin `contain`/`none` can -- only those two need
    // the content rect recomputed.
    if (fit !== 'contain' && fit !== 'none') {
      return rect;
    }
    const videoAspect = vw / vh;
    const boxAspect = rect.width / rect.height;
    let contentWidth;
    let contentHeight;
    if (fit === 'contain') {
      if (videoAspect > boxAspect) {
        contentWidth = rect.width;
        contentHeight = rect.width / videoAspect;
      } else {
        contentHeight = rect.height;
        contentWidth = rect.height * videoAspect;
      }
    } else {
      // 'none': painted at natural size, unscaled either way.
      contentWidth = Math.min(vw, rect.width);
      contentHeight = Math.min(vh, rect.height);
    }
    if (contentWidth >= rect.width - 1 && contentHeight >= rect.height - 1) {
      // No real letterboxing (content fills the box within rounding) --
      // the raw box is already correct.
      return rect;
    }
    const parseAxis = (token, available) => {
      if (!token) {
        return available / 2;
      }
      if (token.endsWith('%')) {
        return (parseFloat(token) / 100) * available;
      }
      const num = parseFloat(token);
      return Number.isFinite(num) ? num : available / 2;
    };
    // Browsers normalize `object-position` in computed style to two
    // length-percentage tokens (e.g. "50% 50%", "0% 50%"), regardless of
    // whether the author wrote keywords or percentages.
    const [xToken, yToken] = (style.objectPosition || '50% 50%').split(/\s+/);
    const left = rect.left + parseAxis(xToken, rect.width - contentWidth);
    const top = rect.top + parseAxis(yToken, rect.height - contentHeight);
    return {
      left,
      top,
      right: left + contentWidth,
      bottom: top + contentHeight,
      width: contentWidth,
      height: contentHeight,
    };
  }

  function positionOverlayFor(videoEl) {
    let rect;
    try {
      rect = getVisibleVideoRect(videoEl);
    } catch (err) {
      return false;
    }
    if (rect.width === 0 && rect.height === 0) {
      // Detached or fully collapsed -- nothing sensible to position over.
      return false;
    }
    // `offsetHeight`, not `getBoundingClientRect()` -- this runs the
    // instant `display` flips to `block`, which is also the instant the
    // pill's `popin` entrance animation starts, and `getBoundingClientRect()`
    // reflects the CSS `transform` that animation is actively running.
    // `offsetHeight` reflects the untransformed layout box, unaffected by
    // the animation. The button's height never changes between idle and
    // expanded, so this one read stays valid for the overlay's whole time
    // on screen -- unlike width (see the `right`-anchoring note below).
    const buttonHeight = overlayButton.offsetHeight || 30;
    const margin = 8;
    let top = Math.max(rect.top + margin, 0);
    top = Math.min(top, window.innerHeight - buttonHeight);
    overlayHost.style.top = `${top}px`;
    // Anchored via CSS `right`, not a computed `left` + measured
    // offsetWidth. The pill's width changes on hover (the "Download"
    // label's `max-width` transition), and re-measuring/repositioning via
    // `left` every time that width changed (on the toggle, or on its
    // transitionend) always either measured the wrong mid-transition
    // width or visibly snapped the box at the end -- both read as janky.
    // `right` is a distance from the viewport's right edge, independent of
    // the element's own width, so the box's right edge simply stays put
    // as CSS grows/shrinks it to the left -- the browser's own transition
    // handles the whole animation smoothly, with no JS involved at all.
    const desiredRightEdge = Math.min(rect.right - margin, window.innerWidth - margin);
    const cssRight = Math.max(window.innerWidth - desiredRightEdge, 0);
    overlayHost.style.left = 'auto';
    overlayHost.style.right = `${cssRight}px`;
    return true;
  }

  function showOverlayFor(videoEl) {
    const firstEverShow = !overlayHost;
    ensureOverlay();
    cancelHide();
    currentVideo = videoEl;
    overlayHost.style.display = 'block';
    resetButtonLabel();
    // Position after display:block so offsetHeight on the button reflects
    // its real rendered size.
    if (!positionOverlayFor(videoEl)) {
      hideOverlayNow();
      return;
    }
    if (firstEverShow) {
      // The very first time ever, ensureOverlay() just created the host/
      // shadow-root/style/button and inserted them into the page in this
      // same synchronous tick -- offsetHeight read by positionOverlayFor()
      // right above can reflect the shadow-DOM stylesheet not having been
      // applied by the engine yet, undercounting the button's true size
      // and mispositioning it vertically. Re-measure one frame later, once
      // layout has definitely settled; a no-op every other time, since the
      // host/button already exist and are already styled.
      requestAnimationFrame(() => {
        if (currentVideo === videoEl) {
          positionOverlayFor(videoEl);
        }
      });
    }
  }

  function resetButtonLabel() {
    if (!overlayButton || !overlayLabel) {
      return;
    }
    overlayButton.disabled = false;
    const hovering = overlayButton.matches(':hover');
    overlayButton.classList.toggle('expanded', hovering);
    overlayButton.classList.toggle('idle', !hovering);
    overlayLabel.textContent = 'Download';
  }

  function hideOverlayNow() {
    cancelHide();
    if (overlayHost) {
      overlayHost.style.display = 'none';
    }
    currentVideo = null;
  }

  function cancelHide() {
    if (hideTimer !== null) {
      clearTimeout(hideTimer);
      hideTimer = null;
    }
  }

  function scheduleHide() {
    cancelHide();
    hideTimer = setTimeout(hideOverlayNow, HIDE_DELAY_MS);
  }

  /**
   * Whether `currentVideo` is still a trustworthy target to resolve a URL
   * from, checked at the moment of the click (never earlier). Being a
   * live DOM reference isn't enough on its own: a listing/carousel-style
   * SPA can swap which video sits under a stationary button via plain
   * DOM mutation, with no navigation and no mouseleave/mouseenter cycle
   * to make `rescan()` notice before the click lands, and a page scroll
   * while the pointer rests motionless on the (position:fixed) button can
   * carry a different video underneath it too. Both would otherwise
   * resolve a URL for content that's no longer what the button is
   * visually sitting on -- the same class of wrong-video bug this whole
   * phase exists to prevent, just via DOM mutation/scroll instead of a
   * URL change.
   *
   * Two independent checks, either of which failing means "no longer
   * trustworthy":
   * - `isConnected` -- catches the video having been removed from the
   *   document entirely.
   * - `document.elementsFromPoint()` at the button's current on-screen
   *   center still includes `currentVideo` -- catches both "a different
   *   video is now visually under the button" (node swap, no navigation)
   *   and "the page scrolled, so nothing/something else is under the
   *   button now" (scroll-while-hovering), without relying on `rescan()`
   *   having already run.
   *
   * @returns {boolean}
   */
  function isCurrentVideoStillValid() {
    if (!currentVideo || !currentVideo.isConnected || !overlayButton) {
      return false;
    }
    let buttonRect;
    try {
      buttonRect = overlayButton.getBoundingClientRect();
    } catch (err) {
      return false;
    }
    if (buttonRect.width === 0 && buttonRect.height === 0) {
      return false;
    }
    const x = buttonRect.left + buttonRect.width / 2;
    const y = buttonRect.top + buttonRect.height / 2;
    let stack;
    try {
      stack = document.elementsFromPoint(x, y);
    } catch (err) {
      // elementsFromPoint is broadly supported, but fail safe (treat as
      // untrustworthy) rather than assume validity if it ever throws.
      return false;
    }
    return stack.indexOf(currentVideo) !== -1;
  }

  /**
   * The button's click handler. This is the one place resolveTargetUrl()
   * must be called fresh -- see the file header comment. `currentVideo`
   * is only ever a plain DOM element reference (never a URL string), so
   * holding onto it between hover and click carries no staleness risk
   * from the URL side; what matters is that the URL is computed from it
   * right here, synchronously, before anything else happens, AND that
   * the reference itself is re-validated first (isCurrentVideoStillValid)
   * since the element it points to can stop being the right target
   * without the overlay ever being told (see that function's doc).
   */
  async function onButtonClick() {
    const videoEl = currentVideo;
    if (!videoEl || !isCurrentVideoStillValid()) {
      // The target under the button is no longer trustworthy -- hide and
      // send nothing. Falling through to resolve a URL from a reference
      // we just determined is wrong would reintroduce the exact bug this
      // file exists to prevent, just via a different trigger than a URL
      // change.
      hideOverlayNow();
      return;
    }

    // Resolved fresh, at the moment of the click -- never earlier.
    const pageUrl = resolveTargetUrl(videoEl);

    cancelHide();
    overlayButton.disabled = true;
    overlayLabel.textContent = 'Sending…';

    let result;
    try {
      result = await chrome.runtime.sendMessage({ type: 'extractVideo', pageUrl });
    } catch (err) {
      // Extension context gone (e.g. reloaded) or no listener -- treat as
      // a plain failure, never throw out of a click handler.
      result = { ok: false };
    }

    overlayLabel.textContent = result && result.ok ? 'Sent' : 'Failed';
    setTimeout(() => {
      // Only reset if the button is still showing this transient state --
      // if the overlay moved on to a different video in the meantime,
      // leave it alone.
      if (overlayLabel && (overlayLabel.textContent === 'Sent' || overlayLabel.textContent === 'Failed')) {
        resetButtonLabel();
      }
    }, 1500);
  }

  // --- Tracking / rescan ------------------------------------------------
  //
  // Same reconciliation spirit as src/app.js's refreshList(): recompute
  // the current candidate set and add/remove hover tracking so it never
  // drifts from reality, instead of diffing or rebuilding from scratch.

  const trackedVideos = new Map(); // HTMLVideoElement -> {onEnter, onLeave}

  function attachTracking(video) {
    const onEnter = () => showOverlayFor(video);
    const onLeave = () => scheduleHide();
    video.addEventListener('mouseenter', onEnter);
    video.addEventListener('mouseleave', onLeave);
    trackedVideos.set(video, { onEnter, onLeave });
  }

  function detachTracking(video) {
    const handlers = trackedVideos.get(video);
    if (handlers) {
      video.removeEventListener('mouseenter', handlers.onEnter);
      video.removeEventListener('mouseleave', handlers.onLeave);
      trackedVideos.delete(video);
    }
    if (currentVideo === video) {
      hideOverlayNow();
    }
  }

  function rescan() {
    let candidates;
    try {
      candidates = new Set(findCandidateVideos());
    } catch (err) {
      candidates = new Set();
    }

    for (const video of candidates) {
      if (!trackedVideos.has(video)) {
        attachTracking(video);
      }
    }

    for (const video of Array.from(trackedVideos.keys())) {
      if (!candidates.has(video) || !document.contains(video)) {
        detachTracking(video);
      }
    }
  }

  // YouTube's own SPA-navigation signal -- fires on every client-side
  // navigation between watch pages, well before our 1s poll would notice.
  document.addEventListener('yt-navigate-finish', rescan);

  // Generic fallback for every other SPA that doesn't fire that event:
  // poll location.href and rescan whenever it changes. This only drives
  // *rescanning which videos have hover tracking* -- it is never the
  // source of truth for a click's target URL (see resolveTargetUrl()).
  let lastHref = window.location.href;
  setInterval(() => {
    if (window.location.href !== lastHref) {
      lastHref = window.location.href;
      rescan();
    }
  }, LOCATION_POLL_MS);

  // DOM-mutation rescanning: a listing/carousel-style SPA can add,
  // remove, or swap <video> elements (or the ancestor chain around them)
  // with no navigation at all, which the yt-navigate-finish listener and
  // the location poll above can't see. Without this, (1) a video removed
  // or swapped out from under a stationary, already-visible button would
  // never get its hover tracking detached, and (2) videos that mount
  // after this script's initial document_idle scan (lazy-loaded feeds,
  // players that grow past the size filter later) would never get a
  // button at all. Debounced so a flurry of unrelated DOM churn on a
  // busy page rescans once, not once per mutation record.
  let mutationRescanTimer = null;
  const mutationObserver = new MutationObserver(() => {
    if (mutationRescanTimer !== null) {
      clearTimeout(mutationRescanTimer);
    }
    mutationRescanTimer = setTimeout(() => {
      mutationRescanTimer = null;
      rescan();
    }, 250);
  });
  try {
    mutationObserver.observe(document.body || document.documentElement, {
      childList: true,
      subtree: true,
    });
  } catch (err) {
    // If observation can't be started for some reason, rescanning still
    // happens via yt-navigate-finish and the location poll above -- just
    // without coverage for pure DOM-mutation changes until the next
    // navigation. The click-time isCurrentVideoStillValid() check still
    // protects against sending the wrong URL either way.
  }

  // Belt-and-suspenders for the moment between hovering and clicking: if
  // the page scrolls or the viewport resizes while a button is shown,
  // whatever was under the (position:fixed) button may no longer be --
  // hide immediately rather than let a stale-looking button sit there
  // until the next rescan. Capture phase because YouTube (and many other
  // SPAs) scroll an inner container, not `window`, and scroll events
  // don't bubble -- only capturing on `document` sees those too. The
  // click handler's isCurrentVideoStillValid() check is still the actual
  // safety net; this is just to avoid the UI looking misleadingly stale
  // in between.
  document.addEventListener(
    'scroll',
    () => {
      if (currentVideo) {
        hideOverlayNow();
      }
    },
    { capture: true, passive: true },
  );
  window.addEventListener('resize', () => {
    if (currentVideo) {
      hideOverlayNow();
    }
  });

  rescan();
})();
