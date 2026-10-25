//! Vision mixer input cost: process CPU load of a CPU-backend mixer with N
//! inputs, in a configuration like lmix's (1080p30 program, 720p multiview,
//! 4 PiPs, 8 DSK layers, stinger, preview output). Prints tables; asserts
//! only that the flows run and every mixer delivers frames.
//!
//! ```text
//! cargo test --release --test vision_mixer_input_bench -- --ignored --nocapture
//! ```
//!
//! Three rows per N in 8, 16, 32 (and, for comparison, the 2 sources alone
//! and a 2-input mixer with both fed):
//! - `sources only`: the N test sources into fakesinks, no mixer (the
//!   baseline to subtract);
//! - `all fed`: every input fed by a live 1080p30 source, only input 0 on
//!   program (all other dist pads at alpha 0);
//! - `2 fed`: inputs 0 and 1 fed, the other N-2 inputs built but unlinked
//!   (an empty slot of a fixed-size mixer).
//!
//! The PGM and MV columns are the output frame rates at the sinks: below the
//! configured 30 fps the mixer cannot keep up and drops frames, and its CPU
//! number then understates the cost of a full-rate mix.
//!
//! Linux only: the load is read from `/proc/self/stat`. Numbers depend on the
//! machine and on what else runs on it; compare rows of one run.

#![cfg(target_os = "linux")]

use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::element::Link;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const BLOCK: &str = "bench-mixer";
const WARMUP: Duration = Duration::from_secs(4);
const MEASURE: Duration = Duration::from_secs(10);

/// User + system CPU time of this process so far.
fn process_cpu() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat");
    // Fields after the parenthesised command name; utime and stime are the
    // 14th and 15th fields of the whole line.
    let rest = &stat[stat.rfind(')').expect("comm") + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    Duration::from_nanos(ticks * 1_000_000_000 / hz.max(1))
}

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// A live 1080p30 test source `src{i}` ending in `caps{i}`.
fn add_source(flow: &mut Flow, i: usize) {
    let pattern = match i {
        0 => "smpte",
        1 => "ball",
        _ => "solid-color",
    };
    flow.elements.push(elem(
        &format!("src{i}"),
        "videotestsrc",
        vec![
            ("pattern", PV::String(pattern.into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        &format!("caps{i}"),
        "capsfilter",
        vec![(
            "caps",
            PV::String("video/x-raw,format=I420,width=1920,height=1080,framerate=30/1".into()),
        )],
    ));
    flow.links.push(Link {
        from: format!("src{i}:src"),
        to: format!("caps{i}:sink"),
    });
}

fn sink(flow: &mut Flow, id: &str, from: String) {
    flow.elements
        .push(elem(id, "fakesink", vec![("sync", PV::Bool(true))]));
    flow.links.push(Link {
        from,
        to: format!("{id}:sink"),
    });
}

#[derive(Clone, Copy, Debug)]
enum Case {
    SourcesOnly,
    AllFed,
    TwoFed,
}

fn flow_for(n: usize, case: Case) -> Flow {
    let mut flow = Flow::new(format!("vm_input_bench_{n}_{case:?}"));
    let fed = match case {
        Case::TwoFed => 2,
        Case::SourcesOnly | Case::AllFed => n,
    };
    for i in 0..fed {
        add_source(&mut flow, i);
    }
    if let Case::SourcesOnly = case {
        for i in 0..fed {
            sink(&mut flow, &format!("sink{i}"), format!("caps{i}:src"));
        }
        return flow;
    }
    let props: HashMap<String, PV> = [
        ("compositor_preference", PV::String("cpu".into())),
        ("num_inputs", PV::UInt(n as u64)),
        ("pgm_resolution", PV::String("1920x1080".into())),
        ("multiview_resolution", PV::String("1280x720".into())),
        ("pgm_framerate", PV::String("30/1".into())),
        ("num_pips", PV::String("4".into())),
        ("num_dsk_inputs", PV::String("8".into())),
        ("enable_stinger", PV::Bool(true)),
        ("enable_preview_out", PV::Bool(true)),
        ("initial_pgm_input", PV::UInt(0)),
        ("initial_pvw_input", PV::UInt(1)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: props,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    for i in 0..fed {
        flow.links.push(Link {
            from: format!("caps{i}:src"),
            to: format!("{BLOCK}:video_in_{i}"),
        });
    }
    sink(&mut flow, "pgmsink", format!("{BLOCK}:pgm_out"));
    sink(&mut flow, "mvsink", format!("{BLOCK}:multiview_out"));
    sink(&mut flow, "pvwsink", format!("{BLOCK}:pvw_out"));
    flow
}

/// Frames `sink` has rendered so far (its `stats`), 0 if it is missing.
async fn rendered(state: &AppState, flow_id: &strom_types::FlowId, sink: &str) -> u64 {
    let pipelines = state.pipelines_read().await;
    pipelines
        .get(flow_id)
        .and_then(|m| m.pipeline().by_name(sink))
        .map(|e| {
            e.property::<gstreamer::Structure>("stats")
                .get::<u64>("rendered")
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// What one run measured.
struct Sample {
    /// CPU load, fraction of one core.
    load: f64,
    /// Program and multiview output frame rates (0 without a mixer).
    pgm_fps: f64,
    mv_fps: f64,
}

/// Run `flow` and measure its CPU load and output frame rates.
async fn measure(dir: &std::path::Path, flow: Flow) -> Sample {
    let storage = NamedTempFile::new().unwrap();
    let blocks = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage.path()),
        blocks.path(),
        dir,
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    let flow_id = flow.id;
    state.upsert_flow(flow).await.unwrap();
    state.start_flow(&flow_id).await.unwrap();
    tokio::time::sleep(WARMUP).await;
    let (pgm0, mv0) = (
        rendered(&state, &flow_id, "pgmsink").await,
        rendered(&state, &flow_id, "mvsink").await,
    );
    let (cpu0, t0) = (process_cpu(), Instant::now());
    tokio::time::sleep(MEASURE).await;
    let secs = t0.elapsed().as_secs_f64();
    let load = (process_cpu() - cpu0).as_secs_f64() / secs;
    let pgm_fps = rendered(&state, &flow_id, "pgmsink")
        .await
        .saturating_sub(pgm0) as f64
        / secs;
    let mv_fps = rendered(&state, &flow_id, "mvsink")
        .await
        .saturating_sub(mv0) as f64
        / secs;
    state.stop_flow(&flow_id).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    Sample {
        load,
        pgm_fps,
        mv_fps,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn vision_mixer_input_cost() {
    gstreamer::init().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    // The 2-input mixer with both inputs fed: what the "2 fed" rows of the
    // larger mixers compare with.
    let src2 = run(dir.path(), 2, Case::SourcesOnly, &mut rows).await;
    let mix2 = run(dir.path(), 2, Case::AllFed, &mut rows).await;
    let mut costs = Vec::new();
    for n in [8usize, 16, 32] {
        let src = run(dir.path(), n, Case::SourcesOnly, &mut rows).await;
        let all = run(dir.path(), n, Case::AllFed, &mut rows).await;
        let two = run(dir.path(), n, Case::TwoFed, &mut rows).await;
        let slots = (n - 2) as f64;
        // A fed input: the all-fed mixer minus the 2-fed one of the same
        // size (both without their sources), over the N-2 extra inputs.
        let fed = ((all - src) - (two - src2)) / slots;
        // An empty slot: the 2-fed mixer with N slots minus the 2-input
        // mixer, over the N-2 empty slots.
        let empty = (two - mix2) / slots;
        costs.push((n, fed, empty));
    }

    eprintln!("\n| N | case | CPU (% of one core) | PGM fps | MV fps |");
    eprintln!("|---|---|---|---|---|");
    for (n, case, sample) in &rows {
        eprintln!(
            "| {n} | {case:?} | {:.0} | {:.1} | {:.1} |",
            sample.load * 100.0,
            sample.pgm_fps,
            sample.mv_fps
        );
    }
    eprintln!("\n| N | per fed input (% of one core) | per empty slot (% of one core) |");
    eprintln!("|---|---|---|");
    for (n, fed, empty) in costs {
        eprintln!("| {n} | {:.1} | {:.1} |", fed * 100.0, empty * 100.0);
    }
}

/// Measure one case, print and record it, and return its CPU load. A mixer
/// that delivers no frames fails the run instead of giving a plausible row.
async fn run(
    dir: &std::path::Path,
    n: usize,
    case: Case,
    rows: &mut Vec<(usize, Case, Sample)>,
) -> f64 {
    let sample = measure(dir, flow_for(n, case)).await;
    eprintln!(
        "N={n} {case:?}: {:.0} % of one core, PGM {:.1} fps, MV {:.1} fps",
        sample.load * 100.0,
        sample.pgm_fps,
        sample.mv_fps
    );
    if !matches!(case, Case::SourcesOnly) {
        assert!(
            sample.pgm_fps > 0.0 && sample.mv_fps > 0.0,
            "N={n} {case:?}: the mixer delivered no frames"
        );
    }
    let load = sample.load;
    rows.push((n, case, sample));
    load
}
