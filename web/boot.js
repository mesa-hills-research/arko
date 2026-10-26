// Starts the page. A file of its own rather than inline, so the page's
// Content-Security-Policy needs no exception for inline scripts. Arko asks
// for it with the build's version (?v=…), and it asks for the page's other
// files with the same, so a browser never mixes two builds. A theme chosen
// in this browser applies before the page draws.
const version = new URL(import.meta.url).search;
const asset = (name) => new URL(name + version, import.meta.url);
document.documentElement.dataset.version = version;

/** Says that the page couldn't start, or stopped, with a way to reload it. */
globalThis.arkoFailed = (message, detail) => {
  if (document.getElementById("failed")) return;
  const box = document.createElement("section");
  box.id = "failed";
  box.className = "failed";
  const said = document.createElement("p");
  said.textContent = message;
  const reload = document.createElement("button");
  reload.type = "button";
  reload.className = "btn primary";
  reload.textContent = "Reload";
  reload.addEventListener("click", () => location.reload());
  box.append(said, reload);
  if (detail) {
    const why = document.createElement("p");
    why.className = "conn-detail";
    why.textContent = detail;
    box.append(why);
  }
  document.body.replaceChildren(box);
};

let theme = null;
try {
  theme = localStorage.getItem("arko.theme");
} catch {
  // Storage may be refused, and the system's theme shows.
}
if (theme) {
  document.documentElement.dataset.theme = theme;
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = asset("all-themes.css");
  link.id = "all-themes";
  document.head.append(link);
}

try {
  const { default: init } = await import(asset("arko-web.js"));
  await init({ module_or_path: asset("arko-web_bg.wasm") });
} catch (error) {
  arkoFailed("Couldn’t start the page.", String(error));
}
