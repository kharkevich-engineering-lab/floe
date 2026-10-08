/**
 * The small set of layout primitives every page shares, so headers, states and
 * notices look and behave the same everywhere. Styles: `styles.css` (§ page
 * header, § status, § states, § notices).
 */
import type { ReactNode } from "react";
import { Icon, type IconName } from "./Icon";

/** Page heading: an optional eyebrow, the title, a one-line description and actions on the right. */
export function PageHeader({
  eyebrow,
  title,
  description,
  actions,
  level = 1,
}: {
  eyebrow?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  level?: 1 | 2;
}) {
  const H = level === 1 ? "h1" : "h2";
  return (
    <header className="page-header">
      <div className="page-header-text">
        {eyebrow && <p className="eyebrow">{eyebrow}</p>}
        <H className="page-header-title">{title}</H>
        {description && <p className="page-header-description">{description}</p>}
      </div>
      {actions && <div className="page-header-actions">{actions}</div>}
    </header>
  );
}

export type Tone = "ok" | "warn" | "danger" | "info" | "neutral";

/** A status label: a dot (or icon) plus text. Colour is never the only signal — the text says it. */
export function StatusBadge({ tone, children, title }: { tone: Tone; children: ReactNode; title?: string }) {
  return (
    <span className={`status-badge tone-${tone}`} title={title}>
      <span className="status-dot" aria-hidden />
      {children}
    </span>
  );
}

const NOTICE_ICON: Record<Tone, IconName> = { ok: "check", warn: "alert", danger: "alert", info: "info", neutral: "info" };

/** An inline message. `role` defaults to `status` for ok/info and `alert` for warn/danger. */
export function Notice({
  tone = "info",
  title,
  children,
  action,
  role,
}: {
  tone?: Tone;
  title?: ReactNode;
  children?: ReactNode;
  action?: ReactNode;
  role?: "status" | "alert" | "none";
}) {
  const r = role ?? (tone === "danger" || tone === "warn" ? "alert" : "status");
  return (
    <div className={`notice tone-${tone}`} role={r === "none" ? undefined : r}>
      <Icon name={NOTICE_ICON[tone]} size={18} />
      <div className="notice-body">
        {title && <p className="notice-title">{title}</p>}
        {children && <div className="notice-text">{children}</div>}
      </div>
      {action && <div className="notice-action">{action}</div>}
    </div>
  );
}

/** Nothing to show yet: what this place is for, and the next step. */
export function EmptyState({ icon = "info", title, children, action }: { icon?: IconName; title: ReactNode; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="empty-state">
      <span className="empty-state-icon">
        <Icon name={icon} size={22} />
      </span>
      <p className="empty-state-title">{title}</p>
      {children && <div className="empty-state-text">{children}</div>}
      {action && <div className="empty-state-action">{action}</div>}
    </div>
  );
}

/**
 * The internal config key (and any decision reference) behind a setting, for
 * operators who need it — always secondary: a small toggle, never the label.
 */
export function KeyHint({ path }: { path: string }) {
  return (
    <details className="key-hint">
      <summary aria-label={`Show the configuration key for this setting`} title="Configuration key">
        <Icon name="key" size={13} />
      </summary>
      <code>{path}</code>
    </details>
  );
}
