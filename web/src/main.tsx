import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app";
import "./styles.css";

/**
 * The theme has to be applied before first paint or there is a flash of the wrong
 * one. Done here rather than in an inline script in the shell, because the
 * Content-Security-Policy allows `script-src 'self'` only.
 */
const stored = localStorage.getItem("seedmedic.theme");
if (stored === "light" || stored === "dark") {
  document.documentElement.setAttribute("data-theme", stored);
}

const root = document.getElementById("root");
if (!root) throw new Error("the shell is missing #root");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
