import { useNavigate } from "react-router-dom";
import { useSession } from "../context/SessionContext";
import { useMetadataQuery } from "../api/queries";
import { Card } from "../components/Card";
import { Button } from "../components/Button";
import { Select } from "../components/Field";
import { useThemePref } from "../utils/useTheme";
import { setThemePref, type ThemePref } from "../utils/theme";

export function SettingsPage() {
  const { session, clearSession } = useSession();
  const navigate = useNavigate();
  const metadata = useMetadataQuery();
  const theme = useThemePref();

  return (
    <div className="stack">
      <h1 style={{ fontSize: "var(--font-size-2xl)" }}>Settings</h1>

      <Card title="Connection">
        <div className="stack">
          <div className="spread">
            <span>Endpoint</span>
            <span className="text-mono">{session?.baseUrl}</span>
          </div>
          <div className="spread">
            <span>Principal</span>
            <span className="text-mono">{session?.principalName}</span>
          </div>
          <div className="spread">
            <span>Role</span>
            <span className="text-mono">{session?.role}</span>
          </div>
          <Button
            variant="danger"
            onClick={() => {
              clearSession();
              navigate("/connect");
            }}
          >
            Log out
          </Button>
        </div>
      </Card>

      <Card title="Appearance">
        <Select
          label="Theme"
          value={theme}
          onChange={(e) => setThemePref(e.target.value as ThemePref)}
        >
          <option value="system">Match system</option>
          <option value="light">Light</option>
          <option value="dark">Dark</option>
        </Select>
      </Card>

      {metadata.data && (
        <Card title="Engine configuration (read-only)">
          <div className="stack">
            <div className="spread">
              <span>Keyspace model</span>
              <span className="text-muted">{metadata.data.keyspace_model}</span>
            </div>
            <div className="spread">
              <span>Data directory</span>
              <span className="text-mono">{metadata.data.data_dir}</span>
            </div>
            <div className="spread">
              <span>MemTable max size</span>
              <span className="text-mono">{metadata.data.memtable_max_size_bytes} bytes</span>
            </div>
            <div className="spread">
              <span>Compaction trigger count</span>
              <span className="text-mono">{metadata.data.compaction_trigger_count}</span>
            </div>
          </div>
        </Card>
      )}
    </div>
  );
}
