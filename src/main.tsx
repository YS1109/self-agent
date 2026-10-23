import React from "react";
import ReactDOM from "react-dom/client";
import "@fontsource/geist/latin-400.css";
import "@fontsource/geist/latin-500.css";
import "@fontsource/geist/latin-600.css";
import "@fontsource/ibm-plex-mono/latin-400.css";
import "@xyflow/react/dist/style.css";
import "./styles.css";
import App from "./App";

document.documentElement.dataset.theme =
  localStorage.getItem("harness-theme") === "dark" ? "dark" : "light";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
