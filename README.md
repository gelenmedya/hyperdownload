<p align="center"><img src="assets/hyperdownload-256.png" width="128" alt="HyperDownload"></p>

# HyperDownload

The download manager of [rustyfiles](https://github.com/gelenmedya/rustyfiles), as an optional add-on: install it from rustyfiles'
Settings > Add-ons.

**Work in progress, planned for rustyfiles 0.9.5.**

What it is being built to do:

- Many connections per file, adjusted to the speed it measures. Each piece is written in its place in the file, so there is no
  "joining parts" wait at the end.
- Pause and resume, also after a restart. An expired link is renewed by the browser extension and the download goes on.
- Torrents and magnet links: choose the files, then download. Watch a video while it downloads.
- A Chrome and Edge extension that takes over the browser's downloads.
- Built into rustyfiles: the file shows in its folder with its progress, and a link dropped into a folder downloads there.
- Checksums checked when the site gives one, and downloaded files marked as coming from the internet, as browsers do.

## Layout

| Path | What |
|---|---|
| `crates/hd-engine` | The HTTP/HTTPS engine: pieces, pause, resume |
| `crates/hd-torrent` | Torrents and magnet links, and watching while downloading (from torwatch): librqbit, the buffer, the local stream, subtitles, audio |
| `crates/hyperdownload` | The program rustyfiles starts: both engines behind an aria2-compatible JSON-RPC interface |
| `vendor/librqbit` | [librqbit](https://github.com/ikatson/rqbit) 9.1.0 (Apache-2.0) with a few patches, listed in `vendor/librqbit/TORWATCH.md` |

## The interface

`hyperdownload serve` listens on `127.0.0.1` on a free port and writes the port and a secret to
`%LOCALAPPDATA%\HyperDownload\rpc.json`. `POST /jsonrpc` takes aria2's JSON-RPC (`aria2.addUri`, `aria2.addTorrent`,
`aria2.tellStatus`, `aria2.pause`, `aria2.unpause`, `aria2.remove`, `aria2.changeOption` with `select-file`, ...), with
`"token:<secret>"` as the first parameter. HyperDownload's own calls: `hd.choices` and `hd.choose` (a torrent's file list waiting
for a choice) and `hd.streamUrl` (a torrent's file to play while it downloads). Requests from web pages (with an `Origin`) and for
another host name are refused.

## The browser extension

`extension/` is the Chrome and Edge extension (Manifest V3, id `pbnficneolckhnkmpagjimjbhjahcepa` while it is loaded unpacked). It
hands the browser's downloads to HyperDownload through native messaging (`hyperdownload.exe --native-host`, registered by
rustyfiles when it installs the add-on, or by hand with `hyperdownload --register`). The browser's own download is paused, not
cancelled, while the extension decides: one HyperDownload does not take goes on in the browser where it was, so a one-time link
stays good. Small files, sites you list, downloads that answer a posted form and links clicked with Alt held stay with the
browser; a private window's download goes with that window's cookies. When a site refuses the link itself, HyperDownload tries
once from the address the browser's download had reached (without the cookies, if that is another site).

To try it before it is in the stores: install HyperDownload from rustyfiles (Settings › Add-ons), open `chrome://extensions`
(or `edge://extensions`), switch on Developer mode, and "Load unpacked" the `extension` folder.

## Third-party code

- librqbit and librqbit-dualstack-sockets by Igor Katson, Apache-2.0 (`vendor/`, their notices kept there).
- The audio decoders and the container readers are crates under MIT or Apache-2.0 (see `Cargo.lock`).

## Licence

MIT, see [LICENSE](LICENSE).
