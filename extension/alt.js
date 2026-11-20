// A link clicked with Alt held stays with the browser: the background script is told just before the download starts.
document.addEventListener(
  "mousedown",
  (e) => {
    if (e.altKey) chrome.runtime.sendMessage({ alt: true }).catch(() => {});
  },
  true
);
