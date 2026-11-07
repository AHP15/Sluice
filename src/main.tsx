import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { queryClient } from "./lib/ipc";
import { initUiPersistence } from "./store/ui";
// D-031 §3: bundled, local woff2 — the CSP (`default-src 'self'`) forbids
// remote fonts, and system fonts would look different on every OS.
import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "./styles.css";

void initUiPersistence();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </React.StrictMode>,
);
