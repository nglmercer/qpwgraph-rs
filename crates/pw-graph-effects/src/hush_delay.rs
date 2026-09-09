//! Preallocated aligned dry-path delay for Hush.
//!
//! Hush's neural path has synthesis and scheduling latency. Keeping the dry
//! fallback in its own small ring makes bypass, startup, underrun, and worker
//! recovery use the same timeline without allocating in `process()`.

pub(crate) struct DryDelay {
    pub(crate) samples: Vec<f32>,
    position: usize,
    delay: usize,
}

impl DryDelay {
    pub(crate) fn new(samples: usize) -> Self {
        Self {
            samples: vec![0.0; samples.max(1)],
            position: 0,
            delay: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.samples.fill(0.0);
        self.position = 0;
    }

    pub(crate) fn set_delay(&mut self, delay: usize) {
        let delay = delay.min(self.samples.len());
        if self.delay != delay {
            self.reset();
            self.delay = delay;
        }
    }

    /// Change the logical delay without clearing the history. The ring is
    /// preallocated during preparation, so quantum-aware latency growth stays
    /// realtime safe and the dry timeline remains continuous during recovery.
    pub(crate) fn set_delay_preserve(&mut self, delay: usize) {
        self.delay = delay.min(self.samples.len());
        if self.delay > 0 {
            self.position %= self.samples.len();
        } else {
            self.position = 0;
        }
    }

    pub(crate) fn process(&mut self, input: &[f32], output: &mut [f32]) {
        if self.delay == 0 {
            output.copy_from_slice(input);
            return;
        }
        let capacity = self.samples.len();
        for (sample, delayed) in input.iter().zip(output.iter_mut()) {
            let read = (self.position + capacity - self.delay % capacity) % capacity;
            *delayed = self.samples[read];
            self.samples[self.position] = *sample;
            self.position = (self.position + 1) % capacity;
        }
    }
}
