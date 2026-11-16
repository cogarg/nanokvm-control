// Injected into console webviews. If the NanoKVM's MJPEG screen (<img id="screen">) shows no
// picture a few seconds after it starts loading, point it at the same stream under a fresh URL.
//
// Why: WebKit (Linux, macOS) remembers that the console page loads /api/stream/mjpeg and on later
// visits fetches it speculatively. That stream never finishes, so the page's own request for it can
// wait forever: a black screen while mouse and keyboard still work. Reloading the page doesn't help
// (it walks into the same trap); a URL WebKit hasn't seen does. On Linux the app also disables the
// disk cache behind this, so this is a safety net there.
(function () {
  if (window.top !== window || location.origin !== __ORIGIN__) return;
  var WAIT_MS = 4000, MAX_KICKS = 5;
  setInterval(function () {
    var el = document.getElementById('screen');
    // Only the MJPEG mode uses an <img>; no src means the page itself gave up (its onError).
    if (!el || el.tagName !== 'IMG' || !el.getAttribute('src')) return;
    var st = el.__nanokvmControl || (el.__nanokvmControl = { since: Date.now(), kicks: 0, done: false });
    if (st.done) return;
    if (el.naturalWidth > 0) { st.done = true; return; }
    if (Date.now() - st.since < WAIT_MS || st.kicks >= MAX_KICKS) return;
    st.kicks++;
    st.since = Date.now();
    var url = new URL(el.getAttribute('src'), location.href);
    url.searchParams.set('_nkc', String(Date.now()));
    console.warn('[NanoKVM Control] no video after ' + WAIT_MS + ' ms, restarting the stream (' + st.kicks + '/' + MAX_KICKS + ')');
    el.src = url.href;
  }, 500);
})();
