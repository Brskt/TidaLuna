// Cover video errors kept from the page
// We paint TIDAL's animated covers ourselves (src/video/cover-canvas.ts): this build cannot play
// their H.264 `<video>`, and told of the failure TIDAL swaps in an `<img>` that reads as a real
// change of artwork. A capture on `window` runs before React's listeners on `<source>` and `<video>`.

// Mirrors `isCoverUrl` in src/video/cover-canvas.ts; `cover-error.test.ts` holds the two together.
function isCoverVideoUrl(url) {
    try {
        var parsed = new URL(url);
        return parsed.protocol === 'https:' && parsed.hostname === 'resources.tidal.com' && parsed.pathname.startsWith('/videos/');
    } catch (e) {
        return false;
    }
}

window.addEventListener('error', function(event) {
    var target = event.target;
    var url = '';
    if (target && target.localName === 'source') url = target.src;
    else if (target && target.localName === 'video') url = target.currentSrc || target.src;
    if (url && isCoverVideoUrl(url)) event.stopImmediatePropagation();
}, true);
