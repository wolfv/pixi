//! blaze step graphs executed by pixi's compute engine: every step is a key.

use std::sync::{Arc, Mutex};

use pixi_blaze::blaze::blaze_engine::{
    ActionRunner, Context, Dep, FinishedEvent, Reporter, Step, StepOutput, external::ExternalRun,
    trace::Trace,
};
use pixi_compute_engine::{ComputeEngine, DependencyGraph};

type Log = Arc<Mutex<Vec<String>>>;

struct TestStep {
    id: String,
    deps: Vec<Dep>,
    spawn: Vec<(String, Vec<Dep>)>,
    fail: bool,
    log: Log,
}

#[async_trait::async_trait]
impl Step for TestStep {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn deps(&self) -> Vec<Dep> {
        self.deps.clone()
    }
    fn category(&self) -> &str {
        "test"
    }
    async fn run(&self, _cx: Arc<Context>) -> anyhow::Result<StepOutput> {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        self.log.lock().unwrap().push(self.id.clone());
        if self.fail {
            anyhow::bail!("{} failed", self.id);
        }
        Ok(StepOutput {
            spawn: self
                .spawn
                .iter()
                .map(|(id, deps)| step(id, deps.clone(), vec![], false, &self.log))
                .collect(),
            ..Default::default()
        })
    }
}

fn step(
    id: &str,
    deps: Vec<Dep>,
    spawn: Vec<(String, Vec<Dep>)>,
    fail: bool,
    log: &Log,
) -> Arc<dyn Step> {
    Arc::new(TestStep {
        id: id.into(),
        deps,
        spawn,
        fail,
        log: log.clone(),
    })
}

struct Quiet;
impl Reporter for Quiet {
    fn finished(&self, _: &FinishedEvent<'_>) {}
    fn failed(&self, _: &str, _: &anyhow::Error) {}
}

fn run(steps: Vec<Arc<dyn Step>>, name: &str) -> Arc<ExternalRun> {
    let root =
        std::env::temp_dir().join(format!("pixi-blaze-engine-{name}-{}", std::process::id()));
    let _ = fs_err::remove_dir_all(&root);
    fs_err::create_dir_all(&root).unwrap();
    let cx = Arc::new(Context::new(Arc::new(ActionRunner::new(&root, 4).unwrap())));
    ExternalRun::new(steps, cx, Arc::new(Quiet), Arc::new(Trace::default())).unwrap()
}

#[tokio::test]
async fn steps_and_discovered_steps_are_engine_keys() {
    let log: Log = Default::default();
    let d = |s: &str| Dep::On(s.to_string());
    let steps = vec![
        // `configure` discovers a graph: two compiles and a link.
        step(
            "configure",
            vec![],
            vec![
                ("cc:a.o".into(), vec![]),
                ("cc:b.o".into(), vec![]),
                ("link:lib".into(), vec![d("cc:a.o"), d("cc:b.o")]),
            ],
            false,
            &log,
        ),
        step("unrelated", vec![], vec![], false, &log),
        step(
            "package",
            vec![Dep::Subtree("configure".into())],
            vec![],
            false,
            &log,
        ),
    ];
    let run = run(steps, "keys");
    let engine = ComputeEngine::new();
    engine
        .with_ctx(async |ctx| {
            pixi_blaze::engine::execute_run(ctx, run.clone(), &["package".to_string()]).await
        })
        .await
        .unwrap()
        .unwrap();

    let log = log.lock().unwrap().clone();
    let pos = |s: &str| log.iter().position(|x| x == s).unwrap();
    assert!(pos("cc:a.o") < pos("link:lib") && pos("cc:b.o") < pos("link:lib"));
    assert!(pos("link:lib") < pos("package"));
    // Demand-driven: nothing asked for `unrelated`.
    assert!(!log.contains(&"unrelated".to_string()), "{log:?}");

    // The engine knows every step, including the discovered ones.
    let graph = DependencyGraph::from_engine(&engine);
    let keys: Vec<String> = graph.keys().map(|k| k.to_string()).collect();
    for id in ["configure", "cc:a.o", "cc:b.o", "link:lib", "package"] {
        let key = format!("BlazeStep({id})");
        assert!(keys.contains(&key), "{key} not a key: {keys:?}");
    }
}

#[tokio::test]
async fn a_failing_step_fails_its_dependents() {
    let log: Log = Default::default();
    let steps = vec![
        step("cc:bad.o", vec![], vec![], true, &log),
        step(
            "link",
            vec![Dep::On("cc:bad.o".into())],
            vec![],
            false,
            &log,
        ),
    ];
    let run = run(steps, "fail");
    let engine = ComputeEngine::new();
    let err = engine
        .with_ctx(async |ctx| {
            pixi_blaze::engine::execute_run(ctx, run.clone(), &["link".to_string()]).await
        })
        .await
        .unwrap()
        .unwrap_err();
    assert!(format!("{err:#}").contains("cc:bad.o"), "{err:#}");
    assert_eq!(*log.lock().unwrap(), vec!["cc:bad.o".to_string()]);
}
