const APP_BASE_URL = 'http://127.0.0.1:57431';
const PING_TIMEOUT_MS = 1500;

async function checkAppRunning() {
  const controller = new AbortController();
  const timeoutId = setTimeout(() => controller.abort(), PING_TIMEOUT_MS);
  try {
    const resp = await fetch(`${APP_BASE_URL}/ping`, { signal: controller.signal });
    if (resp.status !== 200) return false;
    return (await resp.text()) === 'ok';
  } catch (err) {
    return false;
  } finally {
    clearTimeout(timeoutId);
  }
}

async function initPopup() {
  const statusEl = document.getElementById('status');
  const statusDotEl = document.getElementById('status-dot');
  const lastSentSection = document.getElementById('last-sent-section');
  const lastSentLabelEl = document.getElementById('last-sent-label');
  const optionsLinkEl = document.getElementById('options-link');
  const versionEl = document.getElementById('popup-version');

  versionEl.textContent = `v${chrome.runtime.getManifest().version}`;

  const running = await checkAppRunning();
  const { dmToken, lastSent } = await chrome.storage.local.get(['dmToken', 'lastSent']);
  const connected = running && !!dmToken;
  statusEl.textContent = connected ? 'Connected to Loadout' : 'Not connected';
  statusDotEl.classList.toggle('off', !connected);

  if (lastSent && lastSent.label) {
    lastSentSection.classList.remove('hidden');
    lastSentLabelEl.textContent = lastSent.label;
  } else {
    lastSentSection.classList.add('hidden');
  }

  optionsLinkEl.addEventListener('click', () => chrome.runtime.openOptionsPage());
}

document.addEventListener('DOMContentLoaded', initPopup);
