import { useLayoutEffect } from "react";
import { getCurrentWindow, currentMonitor } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";

/**
 * Grow a window to fit its content, up to what the monitor can show.
 *
 * A fixed height either scrolls on a normal display or overflows a small one,
 * and these pages are long by design. This measures what the content actually
 * needs and asks for exactly that, falling back to scrolling only when the
 * screen genuinely cannot fit it — which is why the pages must *also* be
 * scrollable. A window that is clipped with no scrollbar hides its own Save
 * button, which is worse than either.
 */
export function useFitWindowToContent(
  ref: React.RefObject<HTMLDivElement>,
  width: number,
  minHeight: number,
  deps: unknown[],
) {
  useLayoutEffect(() => {
    let cancelled = false;
    // Two frames: one for React to commit, one for the browser to lay out and
    // settle webfont metrics. Measuring earlier reports a too-short page.
    const raf = requestAnimationFrame(() =>
      requestAnimationFrame(async () => {
        const el = ref.current;
        if (!el || cancelled) return;
        try {
          const monitor = await currentMonitor();
          const scale = monitor?.scaleFactor ?? 1;
          // Leave room for the taskbar and the title bar.
          const maxHeight = monitor
            ? (monitor.size.height / scale) * 0.88
            : Number.MAX_SAFE_INTEGER;

          const needed = el.scrollHeight;
          const height = Math.round(Math.min(Math.max(needed, minHeight), maxHeight));

          const win = getCurrentWindow();
          await win.setSize(new LogicalSize(width, height));
          await win.center();
        } catch {
          // Sizing is a nicety; the page scrolls without it, so a failure here
          // must not break the screen.
        }
      }),
    );
    return () => {
      cancelled = true;
      cancelAnimationFrame(raf);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}
