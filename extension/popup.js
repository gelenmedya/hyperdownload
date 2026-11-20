const DEFAULTS = { enabled: true, minMb: 1, excluded: [] };
const $ = (id) => document.getElementById(id);

chrome.storage.sync.get(DEFAULTS).then((s) => {
  $("enabled").checked = s.enabled;
  $("minMb").value = s.minMb;
  $("excluded").value = s.excluded.join("\n");
});

$("enabled").addEventListener("change", (e) => chrome.storage.sync.set({ enabled: e.target.checked }));
$("minMb").addEventListener("change", (e) => chrome.storage.sync.set({ minMb: Math.max(0, Number(e.target.value) || 0) }));
$("excluded").addEventListener("change", (e) =>
  chrome.storage.sync.set({ excluded: e.target.value.split(/\r?\n/).map((l) => l.trim()).filter(Boolean) })
);

chrome.runtime.sendMessage({ ping: true }).then((r) => {
  const el = $("state");
  if (r && r.ok) {
    el.textContent = `Connected · HyperDownload ${r.version || ""}`.trim();
    el.className = "state ok";
  } else {
    el.textContent = "Not connected: install HyperDownload in rustyfiles (Settings › Add-ons)";
    el.className = "state bad";
  }
});
