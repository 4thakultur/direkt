// Swipe pager for the downloads page on small screens.
//
// The two panes (New, Activity) sit side by side in a scroll-snapping track,
// so swiping is the browser's own native scroll. This script only follows
// that scroll: it moves the tab ink, keeps the selected tab and the #activity
// hash in sync, and shows the active count. The pane fade is pure CSS.
(() => {
  "use strict";

  const pager = document.querySelector("[data-pager]");
  if (!pager) return;
  const track = pager.querySelector("[data-pager-track]");
  const tabs = Array.from(pager.querySelectorAll(".pager-tab"));
  const ink = pager.querySelector(".pager-ink");
  const count = pager.querySelector("[data-pager-count]");
  const small = matchMedia("(max-width: 760px)");
  const calm = matchMedia("(prefers-reduced-motion: reduce)");

  let current = location.hash === "#activity" ? 1 : 0;
  let frame = 0;

  const progress = () => {
    const width = track.clientWidth;
    return width ? Math.min(1, Math.max(0, track.scrollLeft / width)) : 0;
  };

  function select(index) {
    tabs.forEach((tab, i) => tab.setAttribute("aria-selected", String(i === index)));
    if (index !== current) {
      current = index;
      const url = location.pathname + location.search + (index ? "#activity" : "");
      history.replaceState(history.state, "", url);
    }
  }

  function paint() {
    frame = 0;
    if (!small.matches) return;
    const p = progress();
    const [a, b] = tabs;
    ink.style.width = `${a.offsetWidth + (b.offsetWidth - a.offsetWidth) * p}px`;
    ink.style.transform = `translateX(${a.offsetLeft + (b.offsetLeft - a.offsetLeft) * p}px)`;
    select(p > 0.5 ? 1 : 0);
  }

  const schedule = () => {
    if (!frame) frame = requestAnimationFrame(paint);
  };

  function go(index, smooth = true) {
    track.scrollTo({
      left: index * track.clientWidth,
      behavior: smooth && !calm.matches ? "smooth" : "auto",
    });
  }

  let shown = null;
  function updateCount() {
    const meta = document.querySelector("#dashboard .dash-count");
    const n = meta ? Number(meta.dataset.count) || 0 : 0;
    const text = n > 99 ? "99+" : String(n);
    count.hidden = n === 0;
    if (shown !== null && text !== shown && n > 0) {
      count.classList.remove("bump");
      void count.offsetWidth; // restart the animation
      count.classList.add("bump");
    }
    count.textContent = text;
    shown = text;
  }

  track.addEventListener("scroll", schedule, { passive: true });

  // Keep the current pane in place when the track's width changes (rotation,
  // desktop to phone layout). Only on a real width change: mobile browsers
  // also resize while the address bar slides, mid-swipe.
  let width = track.clientWidth;
  new ResizeObserver(() => {
    if (track.clientWidth === width) return;
    width = track.clientWidth;
    go(current, false);
    schedule();
  }).observe(track);

  pager.addEventListener("click", (event) => {
    const target = event.target.closest("[data-pane]");
    if (target) go(Number(target.dataset.pane));
  });

  document.body.addEventListener("htmx:afterSwap", (event) => {
    if (event.target.id === "dashboard") updateCount();
  });

  // A download was confirmed: slide over to Activity to watch it.
  document.body.addEventListener("download-started", () => {
    if (small.matches) go(1);
  });

  updateCount();
  go(current, false);
  paint();
})();
