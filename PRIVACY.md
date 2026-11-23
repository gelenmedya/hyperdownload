# HyperDownload browser extension: privacy

The HyperDownload extension for Chrome and Edge hands the browser's downloads to HyperDownload, a download manager that runs on your
own computer (an add-on of [rustyfiles](https://github.com/gelenmedya/rustyfiles)). This page says what it reads, where that goes and
what it keeps. The extension's whole source is in [`extension/`](extension/) of this repository.

## In short

- Nothing is sent to us or to anyone else. There is no account, no analytics, no advertising and no code loaded from the internet.
- What the extension reads about a download goes only to the HyperDownload program on the same computer, which uses it to download
  that file from the same server your browser would have asked.

## What it reads, and when

Only when the browser starts a download that the extension takes over, it gives the HyperDownload program on your computer:

- the download's address, and the address it had reached after redirects;
- the file name the browser suggested, and the file's size when the browser knows it;
- the address of the page the download came from (the referrer);
- the cookies your browser holds for the download's own address, so that a download that needs you to be signed in works as it
  would in the browser (from a private window: that window's own cookies);
- your browser's user-agent text.

To tell a download that answers a submitted form (which it leaves to the browser), the extension notes the addresses of form
submissions for two minutes, in memory only. A small script on every page notices whether Alt is held during a click, so that such
a link stays with the browser; it reads nothing else from pages.

## What it keeps

Three settings, saved with the browser's own extension storage (which your browser may sync to your own account, if you use sync):
whether taking over downloads is on, the size below which files stay with the browser, and the list of sites left to the browser.
No history of downloads is kept by the extension.

## What the HyperDownload program does with it

It downloads the file from the server the address names, sending the cookies and the referrer only to that server (cookies are
never sent on to another site, also not when a download redirects to one). It keeps the address and the file's progress on your
computer so that a download can be paused and resumed. It contacts no server of ours.

## Changes and contact

Changes to this page are in this repository's history. Questions and problems:
[github.com/gelenmedya/hyperdownload/issues](https://github.com/gelenmedya/hyperdownload/issues).
