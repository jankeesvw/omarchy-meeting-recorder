//! Shows the transcribing animation on its own, fed with simulated progress.
//!
//!     cargo run --release --example transcribe_animation [full|minimal|off]

#[allow(dead_code)]
#[path = "../src/animation.rs"]
mod animation;
#[allow(dead_code)]
#[path = "../src/theme.rs"]
mod theme;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;

const LINES: &[&str] = &[
    "Okay, let's get started with the weekly sync.",
    "The new recorder writes both tracks separately now.",
    "Can you share your screen for a second?",
    "Transcription runs after the call, so the GPU stays free.",
    "Let's pick this up again on Thursday.",
];

fn main() -> glib::ExitCode {
    let motion = std::env::args()
        .nth(1)
        .map(|key| animation::Motion::from_key(&key))
        .unwrap_or_default();
    let app = adw::Application::builder()
        .application_id("com.jankeesvw.OmarchyMeetingRecorder.AnimationPreview")
        .build();
    app.connect_startup(|_| theme::follow(|| {}));
    app.connect_activate(move |app| {
        let anim = animation::TranscribeAnimation::new();
        anim.set_motion(motion);
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Transcribe animation preview")
            .default_width(480)
            .default_height(700)
            .child(anim.widget())
            .build();
        anim.set_running(true);

        // A 24 second loop: loading, then two tracks, a line every few seconds.
        let tick = Rc::new(Cell::new(0u32));
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let n = tick.get();
            tick.set(n + 1);
            let t = f64::from(n % 240) / 10.0;
            let (stage, progress) = match t {
                t if t < 2.0 => ("Saving audio", 0.0),
                t if t < 4.0 => ("Loading model", 0.0),
                t if t < 23.0 => ("Transcribing", (t - 4.0) / 19.0),
                _ => ("Transcribing", 1.0),
            };
            anim.set_stage(stage);
            anim.set_progress(progress);
            if n % 25 == 5 {
                anim.push_text(LINES[(n / 25) as usize % LINES.len()]);
            }
            glib::ControlFlow::Continue
        });
        window.present();
    });
    app.run_with_args::<&str>(&[])
}
