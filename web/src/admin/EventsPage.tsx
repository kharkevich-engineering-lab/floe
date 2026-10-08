import { StatusBadge } from "../components/ui";
import { Facts, SectionEditor, focusForm } from "./SectionEditor";
import { secretState } from "./schema-form";

/** The `events` section: the floe-native webhook (docs/EVENTS.md). Restart-only (the bridge is built at startup). */
export function EventsPage() {
  return <SectionEditor section="events" icon="webhook" title="Events webhook" summary={eventsSummary} />;
}

const eventsSummary = (_current: Record<string, unknown>, saved: Record<string, unknown>) => <EventsSummary saved={saved} />;

function EventsSummary({ saved }: { saved: Record<string, unknown> }) {
  const url = typeof saved.webhook_url === "string" && saved.webhook_url !== "" ? saved.webhook_url : null;
  const secret = secretState(saved.webhook_secret);
  return (
    <div className="section-status">
      <div className="section-status-head">
        {url ? <StatusBadge tone="ok">Configured</StatusBadge> : <StatusBadge tone="neutral">Not configured</StatusBadge>}
        <span className="muted">{url ? "Ref changes are delivered to the webhook as JSON." : "No webhook is set up; ref changes are not sent anywhere."}</span>
        <span className="spacer" />
        {!url && (
          <button type="button" className="btn primary" onClick={() => focusForm("events")}>
            Set up a webhook
          </button>
        )}
      </div>
      <Facts
        rows={[
          ["Webhook", url ? <code key="u">{url}</code> : <span key="u" className="muted">Not set</span>],
          ["Signing secret", secret.kind === "unset" ? <span key="s" className="muted">Not set — deliveries are unsigned</span> : secret.kind === "env" ? `From $${secret.env}` : "Set"],
          ["Backstop sweep", typeof saved.sweep_interval === "string" && saved.sweep_interval !== "0s" && saved.sweep_interval !== "0" ? `Every ${saved.sweep_interval}` : "Off"],
        ]}
      />
    </div>
  );
}
