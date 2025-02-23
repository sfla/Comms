use audio_thread_priority::{AudioThreadPriorityError, promote_current_thread_to_real_time};

pub fn bump_priority() -> Result<(), AudioThreadPriorityError> {
    // Using 128 samples because that's the smallest "engine period" that I could
    // glean on any of my devices on Windows.
    // Note that the parameters to this function only appear to matter for macOS and
    // Linux.

    if cfg!(windows) || !cfg!(debug_assertions) {
        promote_current_thread_to_real_time(128, 48000)?;
    }
    Ok(())
}
