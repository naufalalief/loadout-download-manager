# Loadout browser extension

This folder contains the Manifest V3 browser extension that sends downloads from Chrome or Edge to the Loadout desktop app.

## Installing (sideload)

This is a personal, sideloaded install. There is no Chrome Web Store or Edge Add-ons listing for v1, so you load the extension folder directly into your browser in developer mode.

The folder to load is `extension/` in the repository root: the folder that directly contains `manifest.json`.

### Chrome

1. Open `chrome://extensions` in the address bar.
2. Turn on **Developer mode** (toggle in the top-right corner of the page).
3. Click **Load unpacked**.
4. Select the `extension/` folder inside the repository (the one containing `manifest.json`).
5. The **Loadout** extension appears in the list. Make sure it is enabled.

### Edge

1. Open `edge://extensions` in the address bar.
2. Turn on **Developer mode** (toggle in the left sidebar or the page, depending on your Edge version).
3. Click **Load unpacked**.
4. Select the `extension/` folder inside the repository (the one containing `manifest.json`).
5. The **Loadout** extension appears in the list. Make sure it is enabled.

Browser menus and labels change between versions. If the exact wording differs, look for the equivalent options: a "Developer mode" toggle and a "Load unpacked" button.

Pairing happens automatically the first time you trigger a download or a video extraction -- there is no manual "Pair" step to do first. The **Loadout desktop app must already be running** for that first auto-pair attempt to succeed, and needs to be running for the handoff to work at all afterwards (a stopped app just means every download falls back to the browser's normal, native download -- nothing is lost, but nothing gets handed off either). The options page still has a manual **Pair** button as a fallback -- useful if the app wasn't running yet the first time, so auto-pairing couldn't happen; click it once the app is running and it does the same thing immediately.

More than one browser profile can be paired with the app at once (each gets its own entry under Settings -> **Paired browsers**). Zero-click auto-pairing only ever applies to the very first browser profile the app has ever seen, though -- every profile after that needs the app's Settings -> **Allow new pairing** clicked first (it stays open for 2 minutes), *then* that profile's own first download/extraction, or its options page's **Pair** button, to actually pair. Trying to pair without that window open fails with a "Pairing is closed" message telling you exactly where to click.

If **Load unpacked** fails (e.g. `manifest.json` is missing or invalid, or you selected the wrong folder), the extensions page shows a red error card in place of the extension instead of adding it to your list, with a short reason underneath. Fix the folder/file and click **Load unpacked** again.

## Updating after code changes

After you change files in `extension/`, open the extensions page and click the reload (circular arrow) icon on the **Loadout** card. Then reload any tabs you were testing with.

## Reset pairing

Each paired browser profile gets its own token, remembered across app restarts in `paired_profiles.json` in the app's data directory. An unpacked extension's id is derived from the path it was loaded from, so moving or reinstalling the extension (or a browser profile reset) mints a brand-new id with empty storage -- the app still has an entry for the *old* install, which now just sits there unused (it does **not** block the new one from pairing, as long as a pairing window is open for it the same way any other new profile needs).

There are two ways to clean up a stale entry, or recover a working pairing, depending on what's still available to you:

1. **The old extension install is still loaded and usable, and the app is running.** Open its options page and click **Forget pairing**. This calls the app's `/unpair` with the token it still holds, which removes just that one profile's entry, then clears the token from that extension install's own storage.
2. **The old extension install is gone, or the app wasn't running when you tried step 1.** Clean it up from the app's side instead: open Loadout -> Settings -> **Paired browsers**, find the stale entry (it's safe to tell them apart by when each was paired, or rename one first from whichever extension install is still open, if any), and click its **Forget** button.

Either way, pairing the browser profile you actually want to use again needs a pairing window open for it first (Settings -> **Allow new pairing**, unless it's the very first browser profile this install of the app has ever paired) -- then the next download/extraction, or that profile's own options page **Pair** button, picks it up.

If the app ever regenerates its token on its own (e.g. its token file was missing or corrupted on startup) while the extension still holds the old one, every handoff will silently 401 and fall back to native downloads with no visible error. If downloads stop being handed off unexpectedly, use **Forget pairing** and pair again.

## Privacy note on permissions

This extension requests broad host permissions (`*://*/*`) combined with the `cookies` permission. Technically, that combination would let it read cookies for any http(s) site, not just the one being downloaded from -- it is not sandboxed to a narrower set of domains the way a store-reviewed extension with `optional_host_permissions` might be (not workable here: a download event isn't a user gesture, and pre-granting every possible file host up front would defeat the point of a "no fiddly configuration" tool). In practice, this extension only ever reads cookies at the moment of an actual download, scoped to that download's own URL, and only ever sends the captured cookies/referer/user-agent to the local app at `127.0.0.1:57431` -- never anywhere else. This is accepted as a reasonable tradeoff for a personal, sideloaded tool, not for a Web Store-listed extension.

## ToS note on video-site downloads

Downloading video from sites like YouTube generally runs against those platforms' terms of service. That tradeoff is accepted here the same way the broad permissions above are: this is a personal, sideloaded tool for the user's own use, not a distributed product -- there is no store listing, and no attempt is made to resist takedown or ToS enforcement. If you're not comfortable with that tradeoff, don't use the hover-to-download button on those sites.

## Refreshing yt-dlp

Video-site extraction (the hover-to-download button) depends on the `yt-dlp` binary the desktop app manages for itself. Sites like YouTube change their internals often enough that a previously-working `yt-dlp` can start failing to extract a given site -- jobs for that site will end up `Failed` with an error from `yt-dlp` itself. This isn't something the extension can fix; it's a stale-binary problem on the app side.

To recover, open the desktop app, click **Settings** in the header, then click **Check for yt-dlp update**. If a newer version is available it's downloaded and swapped in; the status line next to the button reports the result. Re-trigger the failed extraction afterwards.

## Notes

- Load the folder from the repository, not a copy. If you move or rename the repository folder, the browser loses the path and you need to load it again with **Load unpacked** (see "Reset pairing" above for what that does to pairing).
- Developer-mode extensions show a warning banner on startup. This is expected for sideloaded extensions.
