//! The applier (§B.8 step 6): runs a plan's steps against a [`Target`] and
//! records the outcome on the state entries. Every step is idempotent and
//! re-derivable from the floe side, so a crash between a step and the state CAS
//! costs nothing but a repeat (the ownership and "who wrote it" rules below).

use floe_config::GithubMirrorConfig;
use floe_git::RepoId;

use crate::naming;
use crate::plan::Step;
use crate::settings;
use crate::source::Source;
use crate::state::{RepoEntry, Status};
use crate::target::{CreateOutcome, ExistingRepo, PublishOutcome, READ_ONLY_POLICY, Target};

/// What a step did to its repository's sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Go on with the next step.
    Continue,
    /// Stop this repository's steps (conflict, detached, too large and unclaimed).
    Stop,
}

/// Counters a step contributes to the pass report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    pub created: bool,
    pub published: bool,
    pub nudged: bool,
}

pub struct Ctx<'a> {
    pub cfg: &'a GithubMirrorConfig,
    pub source: &'a dyn Source,
    pub target: &'a dyn Target,
}

/// Run one step for entry `id`. An `Err` is the step's failure; the caller marks
/// the entry `error` and stops its sequence.
pub async fn step(
    ctx: &Ctx<'_>,
    id: &str,
    e: &mut RepoEntry,
    s: &Step,
    out: &mut Outcome,
) -> anyhow::Result<Flow> {
    match s {
        Step::Create { allow_create } => claim(ctx, id, e, *allow_create, out).await,
        Step::PutPolicy => {
            let rid = floe_id(e)?;
            ctx.target
                .put_policy_if_absent(&rid, READ_ONLY_POLICY.as_bytes())
                .await?;
            Ok(Flow::Continue)
        }
        Step::Publish { reason } => publish(ctx, id, e, reason, out).await,
        Step::Nudge => {
            if e.status == Status::Active {
                ctx.target.nudge_follow(&floe_id(e)?);
                out.nudged = true;
            }
            Ok(Flow::Continue)
        }
    }
}

fn floe_id(e: &RepoEntry) -> anyhow::Result<RepoId> {
    e.floe
        .as_deref()
        .and_then(naming::parse)
        .ok_or_else(|| anyhow::anyhow!("no floe repository recorded"))
}

/// Ownership (§B.7.1): ours when it carries our marker, or (only when we may
/// create) when it was never written, which is the crash between the create
/// and the first publish. Anything else is never modified (requirement 3).
fn is_ours(ex: &ExistingRepo, marker: &str, allow_create: bool) -> bool {
    ex.source.as_deref() == Some(marker)
        || (allow_create && ex.head_seq == 0 && ex.settings_revision == 0)
}

/// Map the name (plain, then `--<id>`), then adopt or create it.
async fn claim(
    ctx: &Ctx<'_>,
    id: &str,
    e: &mut RepoEntry,
    allow_create: bool,
    out: &mut Outcome,
) -> anyhow::Result<Flow> {
    let marker = settings::marker(ctx.source.kind(), id);
    let r = e.to_remote(id);
    let owner = naming::owner(&ctx.cfg.prefix, &r.owner);
    for name in naming::candidates(&ctx.cfg.prefix, &r.owner, &r.name, id) {
        let rid = RepoId::new(owner.clone(), name)?;
        let mut existing = ctx.target.exists(&rid).await?;
        if existing.is_none() && allow_create {
            match ctx.target.create(&rid).await? {
                CreateOutcome::Created => {
                    tracing::info!(id, floe = %rid, repo = %e.full_name, "created");
                    out.created = true;
                    e.floe = Some(rid.to_string());
                    e.status = Status::Active;
                    return Ok(Flow::Continue);
                }
                // Raced (or cached on this instance): judge what is there.
                CreateOutcome::AlreadyExists => existing = ctx.target.exists(&rid).await?,
            }
        }
        match existing {
            Some(ex) if is_ours(&ex, &marker, allow_create) => {
                tracing::info!(id, floe = %rid, repo = %e.full_name, "adopted");
                e.floe = Some(rid.to_string());
                e.status = Status::Active;
                return Ok(Flow::Continue);
            }
            Some(_) => {}
            // Too large and nobody prepared it: stays too-large.
            None => return Ok(Flow::Stop),
        }
    }
    if allow_create {
        tracing::warn!(id, repo = %e.full_name, "every candidate floe name is taken by a repository that is not ours");
        e.status = Status::Conflict;
        e.last_error = Some("every candidate floe name is taken by a repository that is not ours".into());
    }
    Ok(Flow::Stop)
}

/// Publish the rendered table, after deciding who wrote the current one.
async fn publish(
    ctx: &Ctx<'_>,
    id: &str,
    e: &mut RepoEntry,
    reason: &str,
    out: &mut Outcome,
) -> anyhow::Result<Flow> {
    let rid = floe_id(e)?;
    let desired = settings::render(ctx.source, ctx.cfg, &e.to_remote(id), e.status);
    let desired_hash = settings::hash(&desired);
    let ex = ctx
        .target
        .exists(&rid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("floe repository {rid} no longer exists"))?;
    let current = settings::upstream_of(&ex.settings_toml);
    let current_hash = settings::hash(&current);
    let marker = settings::marker(ctx.source.kind(), id);
    let unchanged = e.settings_sha.as_deref() == Some(current_hash.as_str());
    // The mirror wrote it and lost the state record, or it already is what we want.
    let ours_lost = ex.settings_author == settings::AUTHOR || current_hash == desired_hash;
    let adoptable = e.settings_sha.is_none()
        && (ex.settings_revision == 0 || ex.source.as_deref() == Some(marker.as_str()));
    if !(unchanged || ours_lost || adoptable) {
        tracing::warn!(id, floe = %rid, author = %ex.settings_author, "[upstream] was edited by someone else; detached (the mirror no longer touches it)");
        e.status = Status::Detached;
        e.last_error = Some(format!("[upstream] edited by {}", ex.settings_author));
        return Ok(Flow::Stop);
    }
    if current_hash == desired_hash {
        e.settings_sha = Some(current_hash);
        e.settings_revision = ex.settings_revision;
        return Ok(Flow::Continue);
    }
    match ctx
        .target
        .publish_upstream(&rid, &desired, ex.settings_revision, reason)
        .await?
    {
        PublishOutcome::Published(rev) => {
            tracing::info!(id, floe = %rid, revision = rev, reason, "[upstream] published");
            e.settings_sha = Some(desired_hash);
            e.settings_revision = rev;
            out.published = true;
            Ok(Flow::Continue)
        }
        PublishOutcome::Conflict => {
            anyhow::bail!("settings of {rid} changed while publishing; retried next pass")
        }
    }
}
