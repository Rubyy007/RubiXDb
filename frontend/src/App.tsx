import type { ReactElement } from "react";
import { Navigate, Route, Routes } from "react-router-dom";
import { useSession } from "./context/SessionContext";
import { AppShell } from "./components/AppShell";
import { ConnectPage } from "./pages/ConnectPage";
import { DashboardPage } from "./pages/DashboardPage";
import { ExplorerPage } from "./pages/ExplorerPage";
import { SnapshotsPage } from "./pages/SnapshotsPage";
import { CompactionPage } from "./pages/CompactionPage";
import { HealthPage } from "./pages/HealthPage";
import { OperationsPage } from "./pages/OperationsPage";
import { SettingsPage } from "./pages/SettingsPage";
import { SqlConsolePage } from "./pages/SqlConsolePage";

function RequireSession({ children }: { children: ReactElement }) {
  const { session, bootstrapping } = useSession();
  // Hold the route while a `#token=` handoff is verified, so the redirect to
  // /connect cannot race it.
  if (!session && bootstrapping) return <main aria-busy="true">Connecting…</main>;
  if (!session) return <Navigate to="/connect" replace />;
  return children;
}

export function App() {
  return (
    <Routes>
      <Route path="/connect" element={<ConnectPage />} />
      <Route
        element={
          <RequireSession>
            <AppShell />
          </RequireSession>
        }
      >
        <Route path="/" element={<DashboardPage />} />
        <Route path="/sql" element={<SqlConsolePage />} />
        <Route path="/explorer" element={<ExplorerPage />} />
        <Route path="/snapshots" element={<SnapshotsPage />} />
        <Route path="/compaction" element={<CompactionPage />} />
        <Route path="/health" element={<HealthPage />} />
        <Route path="/operations" element={<OperationsPage />} />
        <Route path="/settings" element={<SettingsPage />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
