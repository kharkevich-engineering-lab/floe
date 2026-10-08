/** One-word states for the admin status endpoints, shared by the overview and the section pages. */
import type { CatalogStatus, MirrorStatus } from "../api";
import type { Tone } from "../components/ui";
import { relTime } from "../format";

/** What the catalog is doing, in one word, with why. */
export function catalogState(s: CatalogStatus): { tone: Tone; label: string; detail: string } {
  if (!s.compiled) return { tone: "neutral", label: "Unavailable", detail: "This floe build has no catalog support, so it cannot be turned on." };
  if (!s.enabled) return s.uri ? { tone: "neutral", label: "Off", detail: "Configured, but not writing audit tables." } : { tone: "neutral", label: "Not configured", detail: "No catalog is set up yet." };
  if (s.restart_required) return { tone: "warn", label: "Restart required", detail: "The catalog settings changed since this instance started." };
  if (!s.running) return { tone: "warn", label: "Not running here", detail: "Turned on, but the writer is not running on this instance." };
  if (s.up === false) return { tone: "danger", label: "Failing", detail: "The writer cannot reach the catalog." };
  return { tone: "ok", label: "Connected", detail: "Writing audit tables." };
}

/** One word for the mirror's state, with why. */
export function mirrorState(s: MirrorStatus["summary"]): { tone: Tone; label: string; detail: string } {
  if (!s.enabled) return { tone: "neutral", label: "Off", detail: "Nothing is copied from GitHub." };
  if (s.error) return { tone: "danger", label: "Failing", detail: s.error };
  if (!s.last_pass?.finished_at) return { tone: "info", label: "Starting", detail: "The first discovery run has not finished yet." };
  if (s.last_pass.errors > 0) return { tone: "warn", label: "Syncing with errors", detail: `The last run finished ${relTime(s.last_pass.finished_at)} with ${s.last_pass.errors} ${s.last_pass.errors === 1 ? "error" : "errors"}.` };
  return { tone: "ok", label: "Syncing", detail: `Last run ${relTime(s.last_pass.finished_at)}${s.last_pass.complete ? "" : " (incomplete)"}.` };
}

