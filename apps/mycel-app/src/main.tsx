import "./dev-mock"; // first: media.tsx listens at import time
import { render } from "solid-js/web";
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "./styles.css";
import App from "./App";

// No page zoom: Ctrl+wheel, Ctrl +/−/0, trackpad/Safari pinch.
addEventListener("wheel", (e) => e.ctrlKey && e.preventDefault(), { passive: false });
addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && ["+", "=", "-", "_", "0"].includes(e.key)) e.preventDefault();
});
for (const t of ["gesturestart", "gesturechange"]) addEventListener(t, (e) => e.preventDefault());
// Two-finger pinch on Android WebView: touch-action alone is not always honoured, so cancel it here too.
addEventListener("touchmove", (e) => e.touches.length > 1 && e.preventDefault(), { passive: false });

render(() => <App />, document.getElementById("root")!);
