import { useState } from "react";
import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { useSession } from "../context/SessionContext";
import { useStatusQuery } from "../api/queries";
import { Badge, storageStateTone } from "./Badge";
import { Button } from "./Button";

const NAV_ITEMS = [
  { to: "/", label: "Dashboard", icon: "▣" },
  { to: "/explorer", label: "Data Explorer", icon: "⌕" },
  { to: "/snapshots", label: "Snapshots", icon: "⧉" },
  { to: "/compaction", label: "Compaction", icon: "⚙" },
  { to: "/health", label: "Health / Storage", icon: "♥" },
  { to: "/settings", label: "Settings", icon: "⚙︎" },
];

export function AppShell() {
  const { session, clearSession } = useSession();
  const navigate = useNavigate();
  const { data: status } = useStatusQuery();
  const [navOpen, setNavOpen] = useState(false);

  function handleLogout() {
    clearSession();
    navigate("/connect");
  }

  return (
    <div className="app-shell">
      <a href="#main-content" className="skip-link">
        Skip to content
      </a>
      <header className="app-topbar">
        <div className="row">
          <button
            type="button"
            className="btn btn-ghost"
            aria-label="Toggle navigation"
            aria-expanded={navOpen}
            style={{ display: "none" }}
            data-mobile-toggle
            onClick={() => setNavOpen((o) => !o)}
          >
            {"☰"}
          </button>
          <strong>RubiXDB Console</strong>
          {status && <Badge tone={storageStateTone(status.storage_state)}>{status.storage_state}</Badge>}
        </div>
        <div className="row">
          {session && (
            <>
              <span className="text-muted" style={{ fontSize: "var(--font-size-sm)" }}>
                {session.principalName} &middot; <strong>{session.role}</strong>
              </span>
              <Button variant="ghost" onClick={handleLogout}>
                Log out
              </Button>
            </>
          )}
        </div>
      </header>
      <nav className="app-nav" aria-label="Primary" data-open={navOpen}>
        {NAV_ITEMS.map((item) => (
          <NavLink
            key={item.to}
            to={item.to}
            end={item.to === "/"}
            className="app-nav-link"
            onClick={() => setNavOpen(false)}
          >
            <span aria-hidden="true">{item.icon}</span>
            <span className="app-nav-label">{item.label}</span>
          </NavLink>
        ))}
      </nav>
      <main className="app-content" id="main-content" tabIndex={-1}>
        <Outlet />
      </main>
    </div>
  );
}
