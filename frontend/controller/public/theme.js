// Restores the saved appearance before styles paint. Loaded as a blocking script
// from index.html: the controller's Content-Security-Policy forbids inline scripts.
let colorMode = "system";
try {
  const saved = localStorage.getItem("agentdesktop.colorMode");
  if (saved === "light" || saved === "dark") colorMode = saved;
} catch {
  // Storage may be blocked; fall back to the system preference.
}
const theme =
  colorMode === "system"
    ? matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light"
    : colorMode;
document.documentElement.dataset.colorMode = colorMode;
document.documentElement.dataset.theme = theme;
document.documentElement.style.colorScheme = theme;
document.querySelector('meta[name="theme-color"]').content =
  theme === "dark" ? "#1c1d21" : "#fff";
