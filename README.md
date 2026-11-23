<p align="center"><img src="assets/hyperdownload-256.png" width="112" alt="HyperDownload"></p>

<h1 align="center">HyperDownload</h1>

<p align="center">
  <b>The download manager of <a href="https://github.com/gelenmedya/rustyfiles">rustyfiles</a>.</b><br>
  Many connections per file, pause and resume, torrents and magnet links, and videos you can watch before they finish.<br>
  Written in Rust, free and open source.
</p>

<p align="center">
  <a href="https://github.com/gelenmedya/hyperdownload/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/gelenmedya/hyperdownload?label=release&color=ff6a1a&labelColor=232326"></a>
  <img alt="Windows 11, x64 and ARM64" src="https://img.shields.io/badge/Windows_11-x64_|_ARM64-3d3d44?labelColor=232326">
  <a href="LICENSE"><img alt="MIT licence" src="https://img.shields.io/badge/licence-MIT-3d3d44?labelColor=232326"></a>
</p>

## Install

HyperDownload is an add-on: it comes through rustyfiles and needs it.

1. In rustyfiles open **Settings › Add-ons › HyperDownload** and click **Install**. It is downloaded from this repository's
   releases, checked against its SHA-256 before it is installed, and can be removed from the same page at any time.
2. That is all for downloads started in rustyfiles. For the browser's downloads, add the extension (below).

## Use it

- **Right-click a folder's empty space › Download from a link…** (also in the command palette): type or paste an http, https or
  magnet link and the file downloads into that folder. A link you copied is filled in for you.
- **Paste a link into a folder:** rustyfiles offers to download it there.
- **A .torrent file:** right-click › **Download with HyperDownload**. A torrent with several files asks which ones you want.
- Progress, pause, resume and cancel are in rustyfiles' **Status Center**; downloads from the browser show up there too.
- **Watch** a video from a torrent while it downloads: it plays in DirectLook from the parts already there.

## The browser extension (Chrome and Edge)

The extension hands the browser's downloads to HyperDownload. It is **not in the Chrome Web Store or Edge Add-ons yet**, and it is
not inside the add-on that rustyfiles installs. Until it is in the stores, load it by hand:

1. Install HyperDownload from rustyfiles first (step 1 above): that also tells Chrome and Edge where HyperDownload is.
2. Download this repository as a ZIP (**Code › Download ZIP**, or
   [this link](https://github.com/gelenmedya/hyperdownload/archive/refs/heads/main.zip)) and unzip it.
3. Open `chrome://extensions` (or `edge://extensions`), switch on **Developer mode**, click **Load unpacked** and choose the
   `extension` folder from the unzipped files.

Small files, sites you exclude in the extension's pop-up, downloads that answer a posted form and links clicked with **Alt** held
stay with the browser.

<details>
<summary><b>How it works</b></summary>

#### Downloads

- One file over several connections: it starts with four and adds one while each added connection makes the download faster, up
  to sixteen. A connection that is done with its part helps with the parts still left.
- Each piece is written in its place in the file, so there is no "joining the parts" at the end.
- Pause and resume, also after a restart; a file that changed on the server starts again instead of mixing two versions.
- The SHA-256 is checked when the site gives one, and a finished file is marked as coming from the internet (the Mark of the Web),
  as browsers do.

#### The extension

The browser's own download is paused, not cancelled, while the extension decides, so a download HyperDownload does not take goes
on in the browser where it was and a one-time link stays good. A private window's download goes with that window's cookies. When a
site refuses the link itself, HyperDownload tries once from the address the browser's download had reached (without the cookies
when that is another site). It talks to `hyperdownload.exe --native-host` through native messaging; rustyfiles registers it when
it installs the add-on (by hand: `hyperdownload --register`). While loaded unpacked its id is
`pbnficneolckhnkmpagjimjbhjahcepa`.

#### The program's interface

`hyperdownload serve` listens on `127.0.0.1` on a free port and writes the port and a secret to
`%LOCALAPPDATA%\HyperDownload\rpc.json`. `POST /jsonrpc` takes aria2's JSON-RPC (`aria2.addUri`, `aria2.addTorrent`,
`aria2.tellStatus`, `aria2.pause`, `aria2.unpause`, `aria2.remove`, `aria2.changeOption` with `select-file`, ...), with
`"token:<secret>"` as the first parameter. HyperDownload's own calls: `hd.choices` and `hd.choose` (a torrent's file list waiting
for a choice) and `hd.streamUrl` (a torrent's file to play while it downloads). Requests from web pages (with an `Origin`) and for
another host name are refused.

#### Project layout

| Path | What |
|---|---|
| `crates/hd-engine` | The HTTP/HTTPS engine: pieces, connections, pause, resume |
| `crates/hd-torrent` | Torrents and magnet links, and watching while downloading: librqbit, the buffer, the local stream, subtitles, audio |
| `crates/hyperdownload` | The program rustyfiles starts: both engines behind the aria2-compatible JSON-RPC interface |
| `extension/` | The Chrome and Edge extension (Manifest V3) |
| `vendor/librqbit` | [librqbit](https://github.com/ikatson/rqbit) 9.1.0 (Apache-2.0) with a few patches, listed in `vendor/librqbit/TORWATCH.md` |

#### Build

```
cargo build --release -p hyperdownload
```

A tag like `v0.2.0` builds the x64 and ARM64 zips with their SHA-256 and publishes them as a release.

</details>

## Third-party code

- librqbit and librqbit-dualstack-sockets by Igor Katson, Apache-2.0 (`vendor/`, their notices kept there).
- The audio decoders and the container readers are crates under MIT or Apache-2.0 (see `Cargo.lock`).

## Licence

MIT, see [LICENSE](LICENSE).
