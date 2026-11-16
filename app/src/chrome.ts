/**
 * The strip at the top of a window whose content runs under its title bar.
 *
 * The traffic lights sit in it and it drags the window. Its height is the
 * band of the window's toolbar, which AppKit centres the lights in, measured
 * rather than assumed: a second window in the tab group adds a tab bar below
 * it, which the content runs under as well. The band is kept in `--toolbar-h`
 * on the root, the whole strip in `--chrome-h`.
 */

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

/** The strip as the shell measures it, in points. */
interface TitleBar {
  band: number;
  strip: number;
}

/** Keep `--toolbar-h` and `--chrome-h` at the title bar's from now on. */
export function followTitleBar(): void {
  const root = document.documentElement;
  const measure = async () => {
    const bar = await invoke<TitleBar>("title_bar").catch(() => null);
    if (!bar || bar.band <= 0) return;
    root.style.setProperty("--toolbar-h", `${bar.band}px`);
    root.style.setProperty("--chrome-h", `${bar.strip}px`);
    root.toggleAttribute("data-tabbed", bar.strip > bar.band);
  };
  void measure();
  // A tab bar comes and goes without the window changing size, but with the
  // window coming to the front: a tab is opened or closed from another one.
  const win = getCurrentWindow();
  void win.onResized(() => void measure());
  void win.onFocusChanged(({ payload: focused }) => {
    if (focused) void measure();
  });
}
