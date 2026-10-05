import { SectionEditor } from "./SectionEditor";

/** The `events` section: the floe-native webhook (docs/EVENTS.md). Restart-only (the bridge is built at startup). */
export function EventsPage() {
  return <SectionEditor section="events" title="Events webhook" />;
}
