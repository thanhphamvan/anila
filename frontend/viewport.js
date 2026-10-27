// Mobile keyboards can resize only the visual viewport (not window.innerHeight).
// Update layout first, then reuse the terminal's existing window resize handler.
(() => {
  const viewport = window.visualViewport;
  if (!viewport) return;
  let height;
  const fit = () => {
    // Pinch zoom should magnify the existing terminal, not resize its PTY.
    if (viewport.scale !== 1 || viewport.height <= 0) return;
    document.documentElement.style.setProperty('--app-top', `${Math.round(viewport.offsetTop || 0)}px`);
    const next = Math.round(viewport.height);
    if (next === height) return;
    height = next;
    document.documentElement.style.setProperty('--app-height', `${height}px`);
    window.dispatchEvent(new Event('resize'));
  };
  viewport.addEventListener('resize', fit);
  viewport.addEventListener('scroll', fit);
  fit();
})();
