// Starts the page. A file of its own rather than inline, so the page's
// Content-Security-Policy needs no exception for inline scripts. A theme
// chosen in this browser applies before the page draws.
import init from "./arko-web.js";

const theme = localStorage.getItem("arko.theme");
if (theme) {
  document.documentElement.dataset.theme = theme;
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = new URL("all-themes.css", import.meta.url);
  link.id = "all-themes";
  document.head.append(link);
}

init({ module_or_path: new URL("arko-web_bg.wasm", import.meta.url) });
