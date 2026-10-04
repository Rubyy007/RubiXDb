import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { SessionProvider } from "./context/SessionContext";
import { ToastProvider } from "./components/Toast";
import "./styles/global.css";
import "./components/components.css";
import { takeTokenFromLocation } from "./utils/tokenHandoff";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      staleTime: 5_000,
    },
  },
});

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("#root element not found");

// Read (and scrub from the URL) the `rubixdb gui` token handoff exactly once,
// before anything renders or any route can redirect and drop the fragment.
const handoffToken = takeTokenFromLocation();

createRoot(rootEl).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <SessionProvider handoffToken={handoffToken}>
        <ToastProvider>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </ToastProvider>
      </SessionProvider>
    </QueryClientProvider>
  </StrictMode>,
);
