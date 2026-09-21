import { useState, type FormEvent } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { useSession } from "../context/SessionContext";
import { ApiClient } from "../api/client";
import { ApiRequestError } from "../api/types";
import { Button } from "../components/Button";
import { Input } from "../components/Field";

function defaultBaseUrl(): string {
  return window.location.origin;
}

export function ConnectPage() {
  const { session, setSession } = useSession();
  const navigate = useNavigate();
  const [baseUrl, setBaseUrl] = useState(defaultBaseUrl());
  const [apiKey, setApiKey] = useState("");
  const [remember, setRemember] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [connecting, setConnecting] = useState(false);

  if (session) {
    return <Navigate to="/" replace />;
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    setConnecting(true);
    try {
      const trial = new ApiClient({ baseUrl, apiKey, role: "reader", principalName: "" }, () => {
        // No-op: a 401 here just means the key is invalid, handled
        // below via the thrown ApiRequestError -- not a "session
        // expired" event, since no session exists yet.
      });
      const who = await trial.whoami();
      setSession({ baseUrl, apiKey, role: who.role, principalName: who.principal_name }, remember);
      navigate("/", { replace: true });
    } catch (err) {
      if (err instanceof ApiRequestError) {
        setError(
          err.status === 401
            ? "That API key was not accepted."
            : `Could not connect: ${err.message}`,
        );
      } else {
        setError("Could not reach the RubiXDB API at that address.");
      }
    } finally {
      setConnecting(false);
    }
  }

  return (
    <main
      aria-label="Connect to RubiXDB"
      style={{
        minHeight: "100vh",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        background: "var(--color-bg-subtle)",
      }}
    >
      <form
        onSubmit={handleSubmit}
        className="card stack"
        style={{ width: 380 }}
        aria-labelledby="connect-title"
      >
        <div>
          <h1 id="connect-title" style={{ fontSize: "var(--font-size-xl)", marginBottom: "var(--space-1)" }}>
            RubiXDB Console
          </h1>
          <p className="text-muted">Connect to a RubiXDB API endpoint.</p>
        </div>
        <Input
          label="API endpoint"
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          placeholder="https://your-rubixdb-api.example"
          required
          mono
        />
        <Input
          label="API key"
          type="password"
          value={apiKey}
          onChange={(e) => setApiKey(e.target.value)}
          placeholder="Paste your API key"
          hint="Never stored anywhere but this browser, and only if you choose to remember it."
          required
          mono
          autoComplete="off"
        />
        <label className="row" style={{ fontSize: "var(--font-size-sm)" }}>
          <input
            type="checkbox"
            checked={remember}
            onChange={(e) => setRemember(e.target.checked)}
          />
          Remember this connection on this device
        </label>
        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
        <Button type="submit" variant="primary" disabled={connecting}>
          {connecting ? "Connecting…" : "Connect"}
        </Button>
      </form>
    </main>
  );
}
