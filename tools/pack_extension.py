"""Packs extension/ for the Chrome Web Store and Edge Add-ons: dist/hyperdownload-extension-<version>.zip.

The manifest's "key" is left out: it only pins the id of the extension while it is loaded unpacked, and the stores refuse a package
that has one (they sign with their own key and give the item its own id).
"""
import json, os, sys, zipfile

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
src = os.path.join(root, "extension")
manifest = json.load(open(os.path.join(src, "manifest.json"), encoding="utf-8"))
manifest.pop("key", None)
out_dir = os.path.join(root, "dist")
os.makedirs(out_dir, exist_ok=True)
out = os.path.join(out_dir, "hyperdownload-extension-%s.zip" % manifest["version"])
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("manifest.json", json.dumps(manifest, indent=2, ensure_ascii=False) + "\n")
    for folder, _, files in os.walk(src):
        for name in sorted(files):
            full = os.path.join(folder, name)
            rel = os.path.relpath(full, src).replace(os.sep, "/")
            if rel != "manifest.json":
                z.write(full, rel)
print(out)
print("\n".join(sorted(zipfile.ZipFile(out).namelist())))
