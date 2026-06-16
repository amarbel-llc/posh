//! Diagnostic (task #8, option i): enable mosh's own MOSH_PREDICTION_LOG and
//! capture the predictor's decision trace, to find why the overlay renders
//! blank. predictionlog.cc is compiled into the shim, so setting the env makes
//! the engine log every decision (cell created? culled? shown?).
//!
//! Own test binary so its process-global logging/clock can't perturb the golden
//! tests. Not an assertion — it prints the trace for analysis (`--nocapture`).

use mosh_ffi::{DisplayPreference, MoshPredictor};

#[test]
fn trace_always_ls_prediction() {
    let log_path = std::env::temp_dir().join("mosh-predict-trace.log");
    let _ = std::fs::remove_file(&log_path);
    // Must be set before the first prediction_log_enabled() probe (cached).
    std::env::set_var("MOSH_PREDICTION_LOG", &log_path);

    MoshPredictor::set_clock(1000);
    let mut p = MoshPredictor::new(20, 3, DisplayPreference::Always, false);
    p.set_send_interval(50);
    p.feed_server(b"$ ");
    p.set_frame_acked(1);
    p.set_frame_late_acked(1);
    p.set_frame_sent(2);
    p.key(b'l');
    p.set_frame_sent(3);
    p.key(b's');
    let render = p.render();

    let trace = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| format!("<no prediction log written: {e}>"));
    eprintln!("=== render ===\n{render}\n=== trace ===\n{trace}\n=== end ===");
}
