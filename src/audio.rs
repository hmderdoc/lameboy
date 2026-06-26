use rodio::Source;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Audio sample rate (GB outputs at ~44100 Hz)
pub const AUDIO_SAMPLE_RATE: u32 = 44100;

/// Lock-free SPSC (single-producer, single-consumer) ring buffer for audio samples.
/// Eliminates ~88,000 mutex locks per second.
pub struct AudioBuffer {
    samples: Box<[f32]>,
    read_pos: AtomicUsize,
    write_pos: AtomicUsize,
    mask: usize,
    last_sample: std::sync::atomic::AtomicU32, // For smooth underrun handling
}

impl AudioBuffer {
    /// Create a new buffer. Capacity will be rounded up to next power of 2.
    pub fn new(capacity: usize) -> Self {
        use std::sync::atomic::AtomicU32;
        // Round up to power of 2 for fast modulo via bitwise AND
        let capacity = capacity.next_power_of_two();
        Self {
            samples: vec![0.0; capacity].into_boxed_slice(),
            read_pos: AtomicUsize::new(0),
            write_pos: AtomicUsize::new(0),
            mask: capacity - 1,
            last_sample: AtomicU32::new(0), // 0.0f32 as bits
        }
    }
    
    /// Get available samples in buffer
    #[allow(dead_code)]
    pub fn available(&self) -> usize {
        let read = self.read_pos.load(Ordering::Relaxed);
        let write = self.write_pos.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }

    /// Push samples from the producer (main emulation thread)
    pub fn push_samples(&self, samples: &[f32]) {
        let mut write = self.write_pos.load(Ordering::Relaxed);
        for &sample in samples {
            // Safety: mask ensures we're always in bounds
            unsafe {
                let ptr = self.samples.as_ptr() as *mut f32;
                *ptr.add(write & self.mask) = sample;
            }
            write = write.wrapping_add(1);
        }
        self.write_pos.store(write, Ordering::Release);
    }

    /// Pop a single sample from the consumer (audio thread)
    #[inline]
    pub fn pop_sample(&self) -> f32 {
        let read = self.read_pos.load(Ordering::Relaxed);
        let write = self.write_pos.load(Ordering::Acquire);
        
        // Check if buffer is empty - fade last sample to avoid pops
        if read == write {
            let last_bits = self.last_sample.load(Ordering::Relaxed);
            let last = f32::from_bits(last_bits);
            // Fade toward zero to avoid sudden silence (pop)
            let faded = last * 0.99;
            self.last_sample.store(faded.to_bits(), Ordering::Relaxed);
            return faded;
        }
        
        // Safety: mask ensures we're always in bounds
        let sample = unsafe {
            *self.samples.as_ptr().add(read & self.mask)
        };
        self.read_pos.store(read.wrapping_add(1), Ordering::Release);
        
        // Store for smooth underrun handling
        self.last_sample.store(sample.to_bits(), Ordering::Relaxed);
        sample
    }
}

// Safety: AudioBuffer uses atomics for synchronization and is designed for SPSC access
unsafe impl Send for AudioBuffer {}
unsafe impl Sync for AudioBuffer {}

/// Audio source that reads from shared lock-free buffer
pub struct GameboyAudioSource {
    buffer: Arc<AudioBuffer>,
}

impl GameboyAudioSource {
    pub fn new(buffer: Arc<AudioBuffer>) -> Self {
        Self { buffer }
    }
}

impl Iterator for GameboyAudioSource {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        // No mutex lock - just atomic reads!
        Some(self.buffer.pop_sample())
    }
}

impl Source for GameboyAudioSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        2 // Stereo
    }

    fn sample_rate(&self) -> u32 {
        AUDIO_SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None // Infinite stream
    }
}

