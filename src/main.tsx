import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Settings from "./Settings";

// One bundle serves both windows; the settings window is opened at
// `index.html?view=settings`. They cannot share a window because the HUD is
// click-through and never focused, which is the opposite of what a settings
// form needs.
const view = new URLSearchParams(window.location.search).get("view");

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{view === "settings" ? <Settings /> : <App />}</React.StrictMode>,
);
