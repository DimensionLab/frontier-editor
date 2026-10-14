// SPDX-License-Identifier: Apache-2.0
//! The embedded spec schemas compile, resolve by kind and
//! validate real documents from the repository.

#![cfg(feature = "schema")]

use frontier_editor::schema::SchemaRegistry;
use frontier_editor::{Editor, FileKind, Language};

const RUNSPEC: &str = r#"apiVersion: frontier.run/v1
kind: Run
name: ahmed-pointmlp
task: train
config: configs/ahmed_pointmlp.yaml
overrides: [optim.lr=5e-4]
datasets:
  - path: datasets/my-dataset
    source: {type: ingest, source: ahmedml, runs: 1-40}
resources: {gpus: 1, gpu_type: any, gpu_mem_gb: 0, cpus: 8, memory_gb: 32, disk_gb: 20, time_limit: 2h}
image: registry.example.com/frontier-trainer:1.0
env: {HF_HOME: /workspace/hf}
artifacts: []
checkpoint: {resume: true, sync_every_s: 120}
retries: 1
smoke: false
"#;

#[test]
fn every_builtin_schema_compiles() {
    let reg = SchemaRegistry::builtin();
    let mut ids = reg.ids();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            "benchmark-spec",
            "frontier-config",
            "frontier-train-config",
            "hub-entry",
            "jobspec",
            "pipeline-v1",
            "plugin-manifest",
            "recipe",
            "runspec",
        ]
    );
    for kind in [
        FileKind::TrainConfig,
        FileKind::RunSpec,
        FileKind::JobSpec,
        FileKind::Recipe,
        FileKind::Pipeline,
        FileKind::HubEntry,
        FileKind::BenchmarkSpec,
        FileKind::Settings,
        FileKind::PluginManifest,
    ] {
        assert!(reg.resolve("x", kind).is_some(), "{kind:?} has no schema");
    }
}

#[test]
fn runspec_example_validates_and_errors_are_located() {
    let reg = SchemaRegistry::builtin();
    let entry = reg
        .resolve("runspec.yaml", FileKind::RunSpec)
        .expect("runspec schema");
    assert!(SchemaRegistry::validate(&entry, RUNSPEC, Language::Yaml).is_empty());
    let bad = RUNSPEC
        .replace("task: train", "task: trian")
        .replace("retries: 1", "retries: -1");
    let d = SchemaRegistry::validate(&entry, &bad, Language::Yaml);
    let codes: Vec<&str> = d.iter().filter_map(|d| d.code.as_deref()).collect();
    assert_eq!(codes, vec!["/task", "/retries"]);
    let task_at = bad.find("trian").expect("trian");
    assert_eq!(d[0].range.start, task_at);
}

#[test]
fn editor_picks_the_schema_from_the_registry() {
    let reg = SchemaRegistry::builtin();
    let mut e = Editor::open("jobs/new/runspec.yaml", RUNSPEC);
    e.use_registry(&reg);
    assert_eq!(e.kind(), FileKind::RunSpec);
    assert!(e.diagnostics().is_empty());
    e.set_text("task: nope\n").expect("edit");
    assert!(!e.diagnostics().is_empty());
}

#[test]
fn settings_schema_accepts_a_minimal_toml() {
    let reg = SchemaRegistry::builtin();
    let entry = reg
        .resolve("frontier.toml", FileKind::Settings)
        .expect("config schema");
    let ok = SchemaRegistry::validate(
        &entry,
        "[client]\nserver_url = \"http://localhost:8080\"\n",
        Language::Toml,
    );
    assert!(ok.is_empty(), "{ok:?}");
    let bad = SchemaRegistry::validate(&entry, "[client]\nnot_a_key = 1\n", Language::Toml);
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].code.as_deref(), Some("/client"));
}

#[test]
fn train_config_example_from_repo_validates() {
    // Monorepo only: the trainer's shipped configs; skipped when the tree is absent.
    let rel: std::path::PathBuf = ["..", "..", "packages", "frontier-train", "configs"]
        .iter()
        .collect();
    let Ok(dir) = std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
    else {
        return;
    };
    let reg = SchemaRegistry::builtin();
    let entry = reg
        .resolve("x", FileKind::TrainConfig)
        .expect("train schema");
    let mut checked = 0;
    for f in dir.flatten() {
        let p = f.path();
        if p.extension().is_some_and(|e| e == "yaml") {
            let text = std::fs::read_to_string(&p).expect("read");
            let d = SchemaRegistry::validate(&entry, &text, Language::Yaml);
            assert!(
                d.is_empty(),
                "{}: {:?}",
                p.display(),
                d.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
            checked += 1;
        }
    }
    assert!(checked > 0);
}
