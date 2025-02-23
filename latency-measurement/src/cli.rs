mod measurement;
mod state;

use crate::measurement::create_measurement_pipeline;
use crate::state::MeasureState;
use audio_device::audio_connection::{EventLoop, create_minimal_event_loop};
use audio_thread_priority::promote_current_thread_to_real_time;
use std::io;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

fn measure(
    state: &Arc<Mutex<MeasureState>>,
    finished_recv: &Receiver<Duration>,
    start_time: SystemTime,
    name: String,
) -> Option<Duration> {
    println!(
        "[{}ms] Measuring {} latency...",
        SystemTime::now()
            .duration_since(start_time)
            .unwrap()
            .as_millis(),
        &name
    );
    {
        let mut state = state.lock().unwrap();
        *state = MeasureState::DetectNoiseProfile;
    }
    sleep(Duration::from_millis(150));
    let prompt_time = {
        let mut state = state.lock().unwrap();
        let prompt_time = SystemTime::now();
        *state = MeasureState::ReadyToMeasure {
            ready_time: prompt_time,
        };
        prompt_time
    };
    let duration = finished_recv.recv_timeout(Duration::from_secs(2)).ok()?;
    println!(
        "[{}ms] \tTime since prompt to finish is {}ms",
        SystemTime::now()
            .duration_since(start_time)
            .unwrap()
            .as_millis(),
        SystemTime::now()
            .duration_since(prompt_time)
            .unwrap()
            .as_millis()
    );
    Some(duration)
}

pub fn cli(
    state: Arc<Mutex<MeasureState>>,
    finished_recv: Receiver<Duration>,
    start_time: SystemTime,
) {
    // The first ENTER isn't registered when running the app via CLion
    println!("Plug a cable from your mic port to your speaker port, then press ENTER twice");
    io::stdin().read_line(&mut String::new()).unwrap();
    io::stdin().read_line(&mut String::new()).unwrap();
    let system_latency = measure(&state, &finished_recv, start_time, "loopback".to_string())
        .expect("Should take less than 2s for loopback latency");
    println!(
        "[{}ms] Loopback latency is {}ms",
        SystemTime::now()
            .duration_since(start_time)
            .unwrap()
            .as_millis(),
        system_latency.as_millis()
    );

    loop {
        println!("Adjust cables to measure external system, then press ENTER");
        io::stdin().read_line(&mut String::new()).unwrap();
        if let Some(external_latency) = measure(
            &state,
            &finished_recv,
            start_time,
            "external system".to_string(),
        ) {
            println!(
                "[{}ms] Round trip latency is {}ms",
                SystemTime::now()
                    .duration_since(start_time)
                    .unwrap()
                    .as_millis(),
                external_latency.as_millis()
            );
            println!(
                "[{}ms] External system latency is {}",
                SystemTime::now()
                    .duration_since(start_time)
                    .unwrap()
                    .as_millis(),
                external_latency
                    .checked_sub(system_latency)
                    .map(|d| format!("{}ms", d.as_millis()))
                    .unwrap_or_else(|| "<negative duration>".to_string())
            );
        } else {
            println!(
                "[{}ms] Did not get response from external system",
                SystemTime::now()
                    .duration_since(start_time)
                    .unwrap()
                    .as_millis()
            );
            {
                let mut state = state.lock().unwrap();
                *state = MeasureState::Idle;
            }
        }
    }
}

fn main() {
    let start_time = SystemTime::now();
    let state = Arc::new(Mutex::new(MeasureState::Idle));
    let (tx, rx) = channel();

    {
        promote_current_thread_to_real_time(20, 48000).unwrap();
        let state = state.clone();
        std::thread::spawn(move || {
            let (mic, playback) = create_measurement_pipeline(state, tx, start_time);
            let event_loop = create_minimal_event_loop(mic, playback);
            event_loop.run();
        });
    }

    cli(state, rx, start_time);
}
