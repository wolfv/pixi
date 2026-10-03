//! blaze's steps as pixi compute-engine keys.
//!
//! A prepared blaze run ([`blaze::session::Prepared`]) is executed by pixi's
//! [`ComputeEngine`](pixi_compute_engine::ComputeEngine) instead of blaze's
//! own scheduler: every step (each compile, link, test, install, ...) is a
//! [`BlazeStep`] key, which computes its dependencies' keys and then runs the
//! step. A [`BlazeSubtree`] is a step plus everything it spawned (the build
//! graph a `configure` step discovers), so pixi schedules the discovered
//! actions too. blaze keeps the semantics of a step (its cache, reporting,
//! trace and spawned steps, see [`ExternalRun`]); pixi decides what runs when,
//! deduplicates, and tracks the dependencies.

use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use blaze::{blaze_engine::Dep, blaze_engine::external::ExternalRun, session::Prepared};
use pixi_compute_engine::{ComputeCtx, Key};

/// A run's step graph, as part of a key: equal by run id.
#[derive(Clone)]
pub struct RunHandle {
    id: u64,
    run: Arc<ExternalRun>,
}

impl RunHandle {
    pub fn new(run: Arc<ExternalRun>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        RunHandle {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            run,
        }
    }
}

impl PartialEq for RunHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for RunHandle {}

impl Hash for RunHandle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

impl fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze run {}", self.id)
    }
}

/// A failed step (shared by every key that depended on it).
#[derive(Clone, Debug)]
pub struct StepError(pub Arc<anyhow::Error>);

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

/// One blaze step: its dependencies, then the step. The value is what it
/// spawned.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlazeStep {
    pub run: RunHandle,
    pub id: String,
}

impl fmt::Display for BlazeStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)
    }
}

impl Key for BlazeStep {
    type Value = Result<Arc<Vec<String>>, StepError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let deps = self
            .run
            .run
            .deps(&self.id)
            .map_err(|e| StepError(Arc::new(e)))?;
        let items: Vec<(RunHandle, Dep)> =
            deps.into_iter().map(|d| (self.run.clone(), d)).collect();
        let results = ctx
            .compute_join(items, async |ctx, (run, dep): (RunHandle, Dep)| match dep {
                Dep::On(id) => ctx.compute(&BlazeStep { run, id }).await.map(|_| ()),
                Dep::Subtree(id) => ctx.compute(&BlazeSubtree { run, id }).await,
            })
            .await;
        for r in results {
            r?;
        }
        self.run
            .run
            .execute(&self.id)
            .await
            .map(Arc::new)
            .map_err(|e| StepError(Arc::new(e)))
    }
}

/// A step and everything it spawned, transitively.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlazeSubtree {
    pub run: RunHandle,
    pub id: String,
}

impl fmt::Display for BlazeSubtree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (subtree)", self.id)
    }
}

impl Key for BlazeSubtree {
    type Value = Result<(), StepError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let spawned = ctx
            .compute(&BlazeStep {
                run: self.run.clone(),
                id: self.id.clone(),
            })
            .await?;
        let items: Vec<(RunHandle, String)> = spawned
            .iter()
            .map(|id| (self.run.clone(), id.clone()))
            .collect();
        let results = ctx
            .compute_join(items, async |ctx, (run, id): (RunHandle, String)| {
                ctx.compute(&BlazeSubtree { run, id }).await
            })
            .await;
        results.into_iter().collect()
    }
}

/// Run a prepared blaze run's targets on pixi's compute engine.
pub async fn execute(ctx: &mut ComputeCtx, prepared: &Prepared) -> anyhow::Result<()> {
    execute_run(ctx, prepared.run.clone(), &prepared.targets).await
}

/// Run the subtrees of `targets` in `run` on pixi's compute engine.
pub async fn execute_run(
    ctx: &mut ComputeCtx,
    run: Arc<ExternalRun>,
    targets: &[String],
) -> anyhow::Result<()> {
    let run = RunHandle::new(run);
    let items: Vec<(RunHandle, String)> =
        targets.iter().map(|id| (run.clone(), id.clone())).collect();
    let results = ctx
        .compute_join(items, async |ctx, (run, id): (RunHandle, String)| {
            ctx.compute(&BlazeSubtree { run, id }).await
        })
        .await;
    for r in results {
        r.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}
