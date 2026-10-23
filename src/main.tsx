import React from "react";
import ReactDOM from "react-dom/client";
import "@fontsource/geist/latin-400.css";
import "@fontsource/geist/latin-500.css";
import "@fontsource/geist/latin-600.css";
import "@fontsource/ibm-plex-mono/latin-400.css";
import "@xyflow/react/dist/style.css";
import "./styles.css";
import App from "./App";
import { hasMacosTitlebar, installDesktopContextMenu } from "./desktop-chrome";

document.documentElement.dataset.theme =
  localStorage.getItem("harness-theme") === "dark" ? "dark" : "light";
const macosTitlebar = hasMacosTitlebar();
if (macosTitlebar) document.documentElement.dataset.desktopTitlebar = "macos";
const removeContextMenuHandler = installDesktopContextMenu();
if (import.meta.hot) import.meta.hot.dispose(removeContextMenuHandler);

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    {macosTitlebar && <header className="desktop-titlebar" data-tauri-drag-region>
      <span>Self Agent</span>
    </header>}
    <App />
  </React.StrictMode>,
);
