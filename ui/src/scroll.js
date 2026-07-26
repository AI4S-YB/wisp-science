// Chat scroll follow (mirrors web-dist ConversationView pinned-at-bottom behavior).

const hooks = new Map();

// ponytail: single chat scroller, so the jump pill id is a constant.
const JUMP_PILL_ID = "chat-jump-pill";

function lastUserRow(root) {
  const rows = root.querySelectorAll("[data-user-index]");
  return rows.length ? rows[rows.length - 1] : null;
}

function bottomGap(el) {
  return Math.max(0, el.scrollHeight - el.clientHeight - el.scrollTop);
}

function atBottom(el, eps = 2) {
  return bottomGap(el) <= eps;
}

function snapBottom(el) {
  const max = el.scrollHeight - el.clientHeight;
  if (max - el.scrollTop > 2) el.scrollTop = max;
}

/** @param {string} scrollerId @param {string} contentId */
export function attach_chat_scroll(scrollerId, contentId) {
  const scroller = document.getElementById(scrollerId);
  const content = document.getElementById(contentId);
  if (!scroller || !content || hooks.has(scrollerId)) return;

  let follow = true;
  let lastHeight = content.scrollHeight;
  // Timestamp of the last real user scroll gesture. The thread is re-rendered
  // on every streaming delta, which briefly collapses its height, clamps
  // scrollTop toward the top, and fires a spurious "scroll" event. Without this
  // guard that event unfollows and strands the view at the top mid-stream (#61).
  let lastUserScroll = -Infinity;
  const markUser = () => {
    lastUserScroll = performance.now();
  };

  // Floating "Your last message" jump pill: visible only when the last user
  // turn is off-screen and the view is scrolled away from the bottom. Class
  // toggle on a static element — no reactive rebuild involved.
  const syncPill = () => {
    const pill = document.getElementById(JUMP_PILL_ID);
    if (!pill) return;
    let show = false;
    const row = lastUserRow(content);
    if (row && bottomGap(scroller) > 48) {
      const view = scroller.getBoundingClientRect();
      const top = row.getBoundingClientRect().top;
      show = top < view.top - 4 || top > view.bottom - 4;
    }
    pill.classList.toggle("visible", show);
  };

  const syncFollow = () => {
    syncPill();
    if (atBottom(scroller)) {
      follow = true;
      return;
    }
    // Not at bottom: only treat it as an intentional scroll-up if a real gesture
    // happened just now. Reflow-driven scrolls leave `follow` untouched.
    if (performance.now() - lastUserScroll < 500) {
      follow = false;
      return;
    }
    // Reflow-driven clamp while following (streaming rebuilds shrink the
    // thread for a beat, yanking scrollTop up). Scroll events fire before
    // paint, so an instant snap here means the clamped position is never
    // painted — without it the view visibly bounces on every thinking delta.
    if (follow) snapBottom(scroller);
  };

  const onGrowth = () => {
    const h = content.scrollHeight;
    const grew = h > lastHeight;
    lastHeight = h;
    if (follow && grew) snapBottom(scroller);
    syncFollow();
  };

  scroller.style.overflowAnchor = "none";
  scroller.addEventListener("scroll", syncFollow, { passive: true });
  scroller.addEventListener(
    "wheel",
    (e) => {
      markUser();
      if (e.deltaY < 0) follow = false;
      else if (atBottom(scroller)) follow = true;
    },
    { passive: true },
  );
  scroller.addEventListener("touchmove", markUser, { passive: true });
  scroller.addEventListener("pointerdown", markUser, { passive: true });
  scroller.addEventListener("keydown", markUser, { passive: true });

  const ro = new ResizeObserver(() => onGrowth());
  ro.observe(content);

  hooks.set(scrollerId, {
    ro,
    onGrowth,
    unfollow: () => {
      follow = false;
      lastHeight = content.scrollHeight;
    },
    snap: () => {
      follow = true;
      snapBottom(scroller);
      lastHeight = content.scrollHeight;
    },
  });

  follow = true;
  snapBottom(scroller);
}

/** @param {string} scrollerId */
export function notify_chat_scroll(scrollerId) {
  const hook = hooks.get(scrollerId);
  if (!hook) return;
  requestAnimationFrame(() => {
    requestAnimationFrame(() => hook.onGrowth());
  });
}

/** @param {string} scrollerId */
export function force_chat_scroll_bottom(scrollerId) {
  const hook = hooks.get(scrollerId);
  if (hook) {
    hook.snap();
    return;
  }
  const scroller = document.getElementById(scrollerId);
  if (scroller) snapBottom(scroller);
}

/** @param {string} scrollerId @param {string} contentId */
export function preserve_chat_scroll_on_prepend(scrollerId, contentId) {
  const scroller = document.getElementById(scrollerId);
  const content = document.getElementById(contentId);
  if (!scroller || !content) return;
  const oldHeight = content.scrollHeight;
  const oldTop = scroller.scrollTop;
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      scroller.scrollTop = oldTop + content.scrollHeight - oldHeight;
    });
  });
}

/** Scroll the latest user turn into view (the floating jump pill).
 * @param {string} scrollerId */
export function jump_chat_scroll_last_user(scrollerId) {
  const scroller = document.getElementById(scrollerId);
  const target = scroller && lastUserRow(scroller);
  if (!target) return;
  hooks.get(scrollerId)?.unfollow();
  target.scrollIntoView({ block: "start" });
}

/** @param {string} scrollerId @param {string} selector */
export function jump_chat_scroll(scrollerId, selector) {
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      const target = document.querySelector(selector);
      if (!target) return;
      hooks.get(scrollerId)?.unfollow();
      target.scrollIntoView({ block: "start" });
    });
  });
}
