use parking_lot::Mutex;
use std::time::{Duration, Instant};

/// Opt-in measurements for one window. Dropping the monitor stops collection,
/// including Metal's presentation callbacks. Reading it never requests a frame.
pub struct FrameMonitor {
    sample: Mutex<FrameSample>,
}

/// Work measured since the previous sample. CPU time covers scene construction,
/// not GPU execution. Presented frames come from the native compositor callback.
pub struct FrameSample {
    started: Instant,
    /// Wall time covered by this sample.
    pub elapsed: Duration,
    /// Drawables actually presented (including presentations of cached scenes).
    pub presented_frames: u64,
    /// Newly built GPUI scenes.
    pub scenes: u64,
    /// Total CPU time spent building those scenes.
    pub scene_cpu: Duration,
    /// Longest CPU scene build, excluding time spent idle between frames.
    pub longest_scene_cpu: Duration,
}

impl Default for FrameSample {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            elapsed: Duration::ZERO,
            presented_frames: 0,
            scenes: 0,
            scene_cpu: Duration::ZERO,
            longest_scene_cpu: Duration::ZERO,
        }
    }
}

impl FrameMonitor {
    pub(super) fn new() -> Self {
        Self {
            sample: Mutex::new(FrameSample::default()),
        }
    }

    /// Returns and resets the counters atomically, using actual elapsed time.
    pub fn sample(&self) -> FrameSample {
        let mut sample = self.sample.lock();
        let next = FrameSample::default();
        sample.elapsed = next.started.duration_since(sample.started);
        std::mem::replace(&mut *sample, next)
    }

    pub(super) fn record_scene(&self, cpu: Duration) {
        let mut sample = self.sample.lock();
        sample.scenes += 1;
        sample.scene_cpu += cpu;
        sample.longest_scene_cpu = sample.longest_scene_cpu.max(cpu);
    }

    pub(crate) fn record_presentation(&self) {
        self.sample.lock().presented_frames += 1;
    }
}
