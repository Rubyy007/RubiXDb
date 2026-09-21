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
import { SettingsPage } from "./pages/SettingsPage";

function RequireSession({ children }: { children: ReactElement }) {
  const { session } = useSession();
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
        <Route path="/explorer" element={<ExplorerPage />} />
        <Route path="/snapshots" element={<SnapshotsPage />} />
        <Route path="/compaction" element={<CompactionPage />} />
        <Route path="/health" element={<HealthPage />} />
        <Route path="/settings" element={<SettingsPage />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
