// HyperDownload's browser extension: a download the browser starts is handed to HyperDownload (the download manager of
// rustyfiles) through native messaging, with what the server may need to give it (the page it came from, the site's cookies, the
// browser's user agent, and the address the browser's own download had reached after redirects).
//
// THE BROWSER'S DOWNLOAD IS HELD, NOT CANCELLED, WHILE IT IS DECIDED. It is paused the moment it exists (`onCreated`), and the
// decision waits for `onDeterminingFilename`, where its real name and size are known and before the browser asks "Save as". Taken by
// HyperDownload, it is cancelled and leaves the browser's list; left to the browser, it is resumed where it was. So a download is
// never asked for twice by the browser (a one-time link stays good), a small file cannot slip through by finishing first, and the
// "Save as" window does not come up behind HyperDownload. (The way Chromium allows it; learnt from how other download managers'
// extensions do it, written here anew.)
//
// The browser keeps the download when:
//   - the extension is switched off (its popup), or the site is in the list left to the browser,
//   - the link was clicked with Alt held (alt.js tells this script),
//   - the file is known to be smaller than the size set in the popup,
//   - it is not http or https (blob:, data:, a file made by the page itself), or this extension started it,
//   - it answered a form that was posted (the server wants that form again, which HyperDownload cannot send),
//   - HyperDownload cannot be reached.

const HOST = "com.gelenmedya.hyperdownload";
const DEFAULTS = { enabled: true, minMb: 1, excluded: [] };

let altUntil = 0;
// addresses that answered a form post a moment ago (the downloads they bring stay with the browser)
const posted = new Map();
const POSTED_MS = 120000;

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg && msg.alt) altUntil = Date.now() + 3000;
  if (msg && msg.ping) {
    send({ type: "ping" }).then(reply);
    return true;
  }
  return false;
});

chrome.webRequest.onBeforeRequest.addListener(
  (d) => {
    if (d.method !== "POST") return;
    posted.set(d.url.split("#")[0], Date.now());
    if (posted.size > 64) posted.delete(posted.keys().next().value);
  },
  { urls: ["http://*/*", "https://*/*"], types: ["main_frame", "sub_frame"] }
);

function wasPosted(...urls) {
  return urls.some((u) => {
    const at = u && posted.get(u.split("#")[0]);
    return !!at && Date.now() - at < POSTED_MS;
  });
}

async function settings() {
  return Object.assign({}, DEFAULTS, await chrome.storage.sync.get(DEFAULTS));
}

function hostOf(url) {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

function excludedHost(host, list) {
  return list.some((s) => {
    const d = String(s).trim().toLowerCase().replace(/^\*\./, "");
    return d && (host === d || host.endsWith("." + d));
  });
}

// The site's cookies for the address, from the store of the window the download belongs to (a private window's own).
async function cookieHeader(url, incognito) {
  try {
    const stores = await chrome.cookies.getAllCookieStores();
    const store = stores.find((s) => s.tabIds.length && (incognito ? s.id !== "0" : s.id === "0")) || stores.find((s) => (incognito ? s.id !== "0" : s.id === "0"));
    const all = await chrome.cookies.getAll(store ? { url, storeId: store.id } : { url });
    return all.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

function send(msg) {
  return new Promise((resolve) => {
    try {
      chrome.runtime.sendNativeMessage(HOST, msg, (reply) => {
        if (chrome.runtime.lastError) resolve({ ok: false, error: chrome.runtime.lastError.message });
        else resolve(reply || { ok: false, error: "no answer" });
      });
    } catch (e) {
      resolve({ ok: false, error: String(e) });
    }
  });
}

function eligible(item, s) {
  const url = item.finalUrl || item.url;
  return s.enabled && /^https?:/i.test(url) && item.byExtensionId !== chrome.runtime.id;
}

// held the moment it exists
chrome.downloads.onCreated.addListener(async (item) => {
  if (!eligible(item, await settings())) return;
  try {
    await chrome.downloads.pause(item.id);
  } catch {
    // finished already, or not pausable: the decision below handles it
  }
});

async function decide(item) {
  const giveBack = async (why) => {
    console.debug(`HyperDownload: left to the browser (${why}): ${item.url}`);
    try {
      await chrome.downloads.resume(item.id);
    } catch {}
  };
  const s = await settings();
  const url = item.url;
  const finalUrl = item.finalUrl || item.url;
  if (!eligible(item, s)) return giveBack("not taken");
  if (Date.now() < altUntil) {
    altUntil = 0;
    return giveBack("Alt was held");
  }
  if (excludedHost(hostOf(url), s.excluded) || excludedHost(hostOf(finalUrl), s.excluded)) return giveBack("site left to the browser");
  if (wasPosted(url, finalUrl)) return giveBack("it answered a form");
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize;
  if (size > 0 && size < s.minMb * 1024 * 1024) return giveBack("small file");

  const name = (item.filename || "").split(/[\\/]/).pop();
  const reply = await send({
    type: "add",
    url,
    finalUrl: finalUrl !== url ? finalUrl : undefined,
    name: name || undefined,
    referer: item.referrer || undefined,
    cookie: (await cookieHeader(url, item.incognito)) || undefined,
    userAgent: navigator.userAgent,
    size: size > 0 ? size : undefined,
  });
  if (!reply.ok) return giveBack(`HyperDownload: ${reply.error}`);
  // HyperDownload has it: the browser's own copy goes
  try {
    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch {}
}

// the decision, before the browser settles the file's place ("Save as" waits for it: returning true holds it)
chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  decide(item)
    .catch((e) => console.debug(`HyperDownload: ${e}`))
    .finally(() => {
      try {
        suggest();
      } catch {}
    });
  return true;
});
