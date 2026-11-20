// HyperDownload's browser extension: a download the browser starts is handed to HyperDownload (the download manager of
// rustyfiles) through native messaging, with what the server may need to give it (the page it came from, the site's cookies, the
// browser's user agent). The browser keeps the download when:
//   - the extension is switched off (its popup),
//   - the link was clicked with Alt held (alt.js tells this script),
//   - the file is known to be smaller than the size set in the popup,
//   - the site is in the list of sites left to the browser,
//   - the link is not http or https (blob:, data:, a file made by the page itself),
//   - HyperDownload cannot be reached (then the browser downloads it again, as if nothing had happened).

const HOST = "com.gelenmedya.hyperdownload";
const DEFAULTS = { enabled: true, minMb: 1, excluded: [] };

// downloads the browser must keep: the URLs it starts again itself, and the ones after an Alt click
const keep = new Set();
let altUntil = 0;

chrome.runtime.onMessage.addListener((msg) => {
  if (msg && msg.alt) altUntil = Date.now() + 3000;
});

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

async function cookieHeader(url) {
  try {
    const all = await chrome.cookies.getAll({ url });
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

chrome.downloads.onCreated.addListener(async (item) => {
  const url = item.finalUrl || item.url;
  if (keep.delete(url)) return;
  if (!/^https?:/i.test(url)) return;
  if (Date.now() < altUntil) {
    altUntil = 0;
    return;
  }
  const s = await settings();
  if (!s.enabled) return;
  if (excludedHost(hostOf(url), s.excluded)) return;
  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize;
  if (size > 0 && size < s.minMb * 1024 * 1024) return;

  // the browser lets go of it, HyperDownload takes it
  try {
    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch {
    return;
  }
  const name = (item.filename || "").split(/[\\/]/).pop();
  const reply = await send({
    type: "add",
    url,
    name: name || undefined,
    referer: item.referrer || undefined,
    cookie: (await cookieHeader(url)) || undefined,
    userAgent: navigator.userAgent,
    size: size > 0 ? size : undefined,
  });
  if (!reply.ok) {
    // HyperDownload is not there (not installed, or it did not answer): the browser downloads it after all
    keep.add(url);
    chrome.downloads.download({ url });
  }
});

// the popup asks whether HyperDownload can be reached
chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg && msg.ping) {
    send({ type: "ping" }).then(reply);
    return true;
  }
  return false;
});
