// Presence beacon — runs only on the operator's holospaces origin (the narrow
// `content_scripts.matches`), at document_start. It marks the page so the
// Manager can **auto-detect** the egress extension and show a live connection
// indicator, and exposes the extension id so the page can open the egress port
// (`chrome.runtime.connect`) without the operator hand-configuring it.
//
// A content script runs in an isolated world but shares the DOM, so it sets a
// data attribute on <html> the page reads. It announces nothing to any other
// origin (the matches list is the operator's own holospaces origin only).
try {
  document.documentElement.setAttribute("data-holospaces-egress", chrome.runtime.id);
  // Re-announce on request (the page may probe before/after this runs).
  window.addEventListener("holospaces-egress-probe", () => {
    document.documentElement.setAttribute("data-holospaces-egress", chrome.runtime.id);
  });
} catch {
  /* no chrome.runtime — not in an extension context */
}
