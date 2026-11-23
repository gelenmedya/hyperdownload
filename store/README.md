# Publishing the extension (Chrome Web Store, Edge Add-ons)

What to upload and what to type. The package is made by `python tools/pack_extension.py` (it writes
`dist/hyperdownload-extension-<version>.zip`, the `extension/` folder without the manifest's `key`: the stores sign with their own
key and refuse a package that has one). The pictures are made by `python tools/store_pictures.py <popup.png>` from a real picture of
the popup.

**After the item exists:** the store gives it its own id (not `pbnficneolckhnkmpagjimjbhjahcepa`, which is the id of the unpacked
extension). That id has to be added to `EXTENSION_IDS` in `crates/hyperdownload/src/register.rs` here and in rustyfiles'
`crates/rf-app/src/hyperdownload.rs`, and released, or the store's extension is not allowed to talk to the program ("Not connected").

## Store listing

- **Package:** `dist/hyperdownload-extension-0.1.0.zip`
- **Language:** English
- **Category:** Tools
- **Icon:** in the package (128 px)
- **Screenshot:** `store/screenshot-1280x800.png`
- **Small promo tile:** `store/promo-440x280.png`
- **Homepage:** https://github.com/gelenmedya/hyperdownload
- **Support:** https://github.com/gelenmedya/hyperdownload/issues
- **Privacy policy:** https://github.com/gelenmedya/hyperdownload/blob/main/PRIVACY.md

**Description:**

```
HyperDownload hands your browser's downloads to HyperDownload, the download manager of rustyfiles (a free, open-source file manager for Windows).

What you get:
• One file over many connections at once, paused and resumed where it stopped, also after a restart
• Progress, pause and cancel in rustyfiles' Status Center, with the file in the folder you chose
• Downloads that need you to be signed in still work: the site's cookies for that download go with it

What stays with the browser:
• Small files (you set the size)
• The sites you list
• Any link you click with Alt held
• Downloads that answer a submitted form

IMPORTANT: this extension needs rustyfiles with the HyperDownload add-on installed (rustyfiles › Settings › Add-ons › HyperDownload › Install). Without it the extension does nothing and every download stays with the browser; its pop-up tells you whether it is connected.

Nothing is sent to us or to anyone else: what the extension reads about a download goes only to the HyperDownload program on your own computer. Free and open source (MIT): https://github.com/gelenmedya/hyperdownload
```

## Privacy practices

**Single purpose:**

```
Hand the browser's file downloads to HyperDownload, a download manager installed on the same computer.
```

**Permission justifications:**

- `downloads`

```
To notice a download when it starts, hold (pause) it while deciding, remove it from the browser's list once HyperDownload has taken it, or resume it when it stays with the browser.
```

- `nativeMessaging`

```
To pass the download (its address, name, size, referrer, cookies for that address, user agent) to the HyperDownload program installed on the user's computer, which does the downloading. This is the extension's whole purpose.
```

- `cookies`

```
To give HyperDownload the cookies of the download's own address, so that a download that needs the user to be signed in works as it would in the browser. They are read only for that address, at the moment of a download, and passed only to the local program.
```

- `storage`

```
To keep the user's three settings: on/off, the size below which files stay with the browser, and the list of sites left to the browser.
```

- `webRequest`

```
To see that a page was requested by submitting a form (method POST). A download that answers a submitted form is left to the browser, because the form cannot be sent again by the download manager. Only the address and the method are looked at, kept in memory for two minutes.
```

- Host permission (`<all_urls>`) and the content script

```
A download can come from any site, and the cookies and webRequest APIs need access to that site. The content script on every page does one thing: it notices whether Alt is held during a click, so that the link stays with the browser. It reads nothing from the page.
```

- **Remote code:** No, I am not using remote code.

**Data usage** (what the form asks; these are the owner's own declarations to tick):

- Collected: **Authentication information** (the cookies of a download's address) and **Web history** (the download's address and the
  page it came from). Both go only to the HyperDownload program on the same computer; nothing leaves it except the request to the
  server the file is on.
- The three certifications hold for the code as it is: the data is not sold or transferred to third parties, not used for anything
  but the single purpose, and not used for creditworthiness or lending.
