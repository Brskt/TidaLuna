// Cover video errors kept from the page
// A cover `<video>` errors on TIDAL's own H.264, and on our transcode (src/video/cover-hijack.ts)
// until it is cached; told, TIDAL swaps in an `<img>` read as a new artwork. Captured on `window`
// before React's listeners and stopped; for our URL the bundle hears `luna-cover-miss` instead.

// Mirrors `isCoverUrl` in src/video/cover-hijack.ts; `cover-error.test.ts` holds the two together.
function isCoverVideoUrl(url) {
    try {
        var parsed = new URL(url);
        return parsed.protocol === 'https:' && parsed.hostname === 'resources.tidal.com' && parsed.pathname.startsWith('/videos/');
    } catch (e) {
        return false;
    }
}

function isTranscodeUrl(url) {
    return url.indexOf('luna-cover://video/') === 0;
}

window.addEventListener('error', function(event) {
    var target = event.target;
    var url = '';
    // React can rewrite the `<source>` before its error runs: `currentSrc` still names the request
    // that failed. A source detached from its `<video>` names none.
    var failed = '';
    if (target && target.localName === 'source') {
        url = target.src;
        failed = (target.parentElement && target.parentElement.currentSrc) || '';
    } else if (target && target.localName === 'video') {
        url = failed = target.currentSrc || target.src;
    }
    if (!url) return;
    if (isTranscodeUrl(url)) {
        event.stopImmediatePropagation();
        window.dispatchEvent(new CustomEvent('luna-cover-miss', { detail: { target: target, url: failed } }));
    } else if (isCoverVideoUrl(url)) {
        event.stopImmediatePropagation();
    }
}, true);
