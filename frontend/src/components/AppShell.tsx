import { useEffect, useRef, useState } from "react";
import { Link, NavLink, Outlet, useLocation, useNavigate } from "react-router-dom";
import { useSession } from "../context/SessionContext";
import { useStatusQuery } from "../api/queries";
import { Badge, storageStateTone } from "./Badge";
import { Icon, type IconName } from "./Icon";
import { RamCard } from "./RamCard";
import { clearSessionActivity } from "../utils/sessionActivity";
import logoUrl from "../assets/rubixdb-logo.png";

interface NavItem {
  to: string;
  label: string;
  icon: IconName;
}
interface NavGroup {
  id: string;
  title: string;
  items: NavItem[];
}

// Only pages rubiXDb really has. Items for features it does not have
// (ingestion, transformation, AI & ML, apps, marketplace, data sharing,
// Postgres) are omitted entirely rather than shown disabled.
const NAV_GROUPS: NavGroup[] = [
  {
    id: "work",
    title: "Work with data",
    items: [
      { to: "/", label: "Home", icon: "home" },
      { to: "/sql", label: "SQL Console", icon: "sql" },
      { to: "/health", label: "Monitoring", icon: "monitoring" },
    ],
  },
  {
    id: "catalog",
    title: "Data catalog",
    items: [
      { to: "/explorer", label: "Catalog", icon: "catalog" },
      { to: "/settings", label: "Governance & security", icon: "shield" },
    ],
  },
  {
    id: "manage",
    title: "Manage",
    items: [
      { to: "/compaction", label: "Compute", icon: "compute" },
      { to: "/operations", label: "Admin", icon: "admin" },
      { to: "/snapshots", label: "Snapshots", icon: "snapshots" },
    ],
  },
];

function crumbsFor(pathname: string): string[] {
  if (pathname !== "/") {
    for (const group of NAV_GROUPS) {
      for (const item of group.items) {
        if (item.to === pathname) return [group.title, item.label];
      }
    }
  }
  return ["Home"];
}

function initials(name: string): string {
  const parts = name.split(/[\s._-]+/).filter(Boolean);
  const letters = parts.length > 1 ? parts[0][0] + parts[1][0] : name.slice(0, 2);
  return (letters || "?").toUpperCase();
}

// The console has no notification source today, so the panel states that
// honestly instead of showing invented entries.
function Notifications() {
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    const onDown = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("mousedown", onDown);
    };
  }, [open]);
  return (
    <div className="shell-popover-wrap" ref={wrap}>
      <button
        type="button"
        className="shell-icon-btn"
        aria-label="Notifications"
        aria-expanded={open}
        aria-controls="notifications-panel"
        onClick={() => setOpen((o) => !o)}
      >
        <Icon name="bell" />
      </button>
      {open && (
        <div id="notifications-panel" className="shell-popover" role="region" aria-label="Notification list">
          <p className="text-muted">No notifications.</p>
        </div>
      )}
    </div>
  );
}

export function AppShell() {
  const { session, clearSession } = useSession();
  const navigate = useNavigate();
  const location = useLocation();
  const { data: status } = useStatusQuery();
  const [navOpen, setNavOpen] = useState(false);
  const [collapsed, setCollapsed] = useState(false);
  const crumbs = crumbsFor(location.pathname);

  // In-memory query history must not outlive the signed-in session (log out,
  // 401, or any other unmount of the shell).
  useEffect(() => clearSessionActivity, []);

  function handleLogout() {
    clearSession();
    navigate("/connect");
  }

  function focusSearch() {
    setNavOpen(false);
    document.getElementById("global-search")?.focus();
  }

  return (
    <div className="app-shell" data-collapsed={collapsed}>
      <a href="#main-content" className="skip-link">
        Skip to content
      </a>
      <aside className="app-nav" aria-label="Sidebar" data-open={navOpen}>
        <div className="rail-top">
          <Link to="/" className="rail-logo-link" aria-label="rubiXDb" onClick={() => setNavOpen(false)}>
            <img className="rail-logo" src={logoUrl} alt="" width="480" height="112" />
          </Link>
          <button
            type="button"
            className="shell-icon-btn rail-collapse"
            aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"}
            aria-expanded={!collapsed}
            onClick={() => setCollapsed((c) => !c)}
          >
            <Icon name={collapsed ? "chevronRight" : "chevronLeft"} />
          </button>
          <Link
            to="/sql"
            className="shell-icon-btn rail-new"
            aria-label="New query"
            onClick={() => setNavOpen(false)}
          >
            <Icon name="plus" />
          </Link>
          <button type="button" className="shell-icon-btn rail-search" aria-label="Search" onClick={focusSearch}>
            <Icon name="search" />
          </button>
        </div>
        <nav aria-label="Primary" className="rail-nav">
          {NAV_GROUPS.map((group) => (
            <div key={group.id} className="rail-group">
              <p className="rail-group-title" id={`rail-group-${group.id}`}>
                {group.title}
              </p>
              <ul aria-labelledby={`rail-group-${group.id}`}>
                {group.items.map((item) => (
                  <li key={item.to}>
                    <NavLink
                      to={item.to}
                      end={item.to === "/"}
                      className="app-nav-link"
                      onClick={() => setNavOpen(false)}
                    >
                      <Icon name={item.icon} />
                      <span className="app-nav-label">{item.label}</span>
                    </NavLink>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </nav>
        <RamCard />
        {session && (
          <div className="rail-profile">
            <span className="avatar" aria-hidden="true">
              {initials(session.principalName)}
            </span>
            <span className="rail-profile-text">
              <span className="rail-profile-name">{session.principalName}</span>
              <span className="rail-profile-role">{session.role}</span>
            </span>
            <button type="button" className="shell-icon-btn rail-logout" onClick={handleLogout}>
              <Icon name="logout" />
              <span className="visually-hidden">Log out</span>
            </button>
          </div>
        )}
      </aside>
      <header className="app-topbar">
        <button
          type="button"
          className="shell-icon-btn mobile-toggle"
          aria-label="Toggle navigation"
          aria-expanded={navOpen}
          onClick={() => setNavOpen((o) => !o)}
        >
          <Icon name="menu" />
        </button>
        <nav aria-label="Breadcrumb" className="breadcrumb">
          <ol>
            {crumbs.map((c, i) => (
              <li key={c} aria-current={i === crumbs.length - 1 ? "page" : undefined}>
                {c}
              </li>
            ))}
          </ol>
        </nav>
        <div className="topbar-search" role="search">
          <Icon name="search" />
          <input
            id="global-search"
            type="search"
            aria-label="Search"
            placeholder="Search your database, tables, and SQL history"
            autoComplete="off"
          />
        </div>
        <div className="topbar-right">
          {status && <Badge tone={storageStateTone(status.storage_state)}>{status.storage_state}</Badge>}
          <Notifications />
          {session && (
            <span className="avatar" role="img" aria-label={`Signed in as ${session.principalName}`}>
              {initials(session.principalName)}
            </span>
          )}
        </div>
      </header>
      <main className="app-content" id="main-content" tabIndex={-1}>
        <div className="app-content-inner">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
