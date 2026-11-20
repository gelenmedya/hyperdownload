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
| `crates/hyperdownload` | The program rustyfiles starts: the engines behind an aria2-compatible JSON-RPC interface |

The BitTorrent engine is [librqbit](https://github.com/ikatson/rqbit) (Apache-2.0).

## Licence

MIT, see [LICENSE](LICENSE).
