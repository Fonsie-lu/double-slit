//! Small radix-2 complex FFT. Used off-thread for wavetable band-limiting and on
//! the GUI thread for the spectrum analyser; never called from the audio thread.

use crate::dsp::common::TWO_PI;

/// Precomputed twiddle factors and a Hann window for one power-of-two size.
/// Immutable once built, so one plan can be shared by every caller and thread.
pub struct Fft {
    n: usize,
    levels: u32,
    cos_table: Vec<f32>,
    sin_table: Vec<f32>,
    window: Vec<f32>,
}

impl Fft {
    /// `size` must be a power of two.
    pub fn new(size: usize) -> Self {
        assert!(size.is_power_of_two(), "FFT size {size} is not a power of two");
        let levels = size.trailing_zeros();
        let half = size / 2;
        let mut cos_table = Vec::with_capacity(half);
        let mut sin_table = Vec::with_capacity(half);
        for i in 0..half {
            let a = -2.0 * std::f64::consts::PI * i as f64 / size as f64;
            cos_table.push(a.cos() as f32);
            sin_table.push(a.sin() as f32);
        }
        let window = (0..size)
            .map(|i| 0.5 - 0.5 * (TWO_PI * i as f32 / size as f32).cos())
            .collect();
        Self {
            n: size,
            levels,
            cos_table,
            sin_table,
            window,
        }
    }

    pub fn size(&self) -> usize {
        self.n
    }

    /// In-place forward transform of split real/imaginary arrays. Only the
    /// first `size()` entries of each slice are touched.
    pub fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        self.transform(re, im, false);
    }

    /// In-place inverse transform, normalised by 1/n.
    pub fn inverse(&self, re: &mut [f32], im: &mut [f32]) {
        self.transform(re, im, true);
        let norm = 1.0 / self.n as f32;
        for v in re[..self.n].iter_mut().chain(im[..self.n].iter_mut()) {
            *v *= norm;
        }
    }

    /// Magnitude spectrum of a real signal (first n/2 bins), Hann-windowed.
    pub fn magnitudes(&self, input: &[f32], mags_out: &mut Vec<f32>) {
        let n = self.n;
        let mut re: Vec<f32> = input[..n].iter().zip(&self.window).map(|(x, w)| x * w).collect();
        let mut im = vec![0.0; n];
        self.forward(&mut re, &mut im);
        let scale = 2.0 / n as f32;
        mags_out.clear();
        mags_out.extend((0..n / 2).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() * scale));
    }

    fn transform(&self, re: &mut [f32], im: &mut [f32], inverse: bool) {
        let n = self.n;
        // Bit reversal permutation.
        for i in 0..n {
            let mut j = 0;
            for b in 0..self.levels {
                j |= ((i >> b) & 1) << (self.levels - 1 - b);
            }
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }

        let mut size = 2;
        while size <= n {
            let half = size / 2;
            let step = n / size;
            let mut i = 0;
            while i < n {
                let mut k = 0;
                for a in i..i + half {
                    let b = a + half;
                    let c = self.cos_table[k];
                    let s = if inverse { -self.sin_table[k] } else { self.sin_table[k] };
                    let tre = re[b] * c - im[b] * s;
                    let tim = re[b] * s + im[b] * c;
                    re[b] = re[a] - tre;
                    im[b] = im[a] - tim;
                    re[a] += tre;
                    im[a] += tim;
                    k += step;
                }
                i += size;
            }
            size *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_then_inverse_is_identity() {
        let fft = Fft::new(64);
        let orig: Vec<f32> = (0..64).map(|i| ((i * 7) % 13) as f32 - 6.0).collect();
        let mut re = orig.clone();
        let mut im = vec![0.0; 64];
        fft.forward(&mut re, &mut im);
        fft.inverse(&mut re, &mut im);
        for (a, b) in re.iter().zip(&orig) {
            assert!((a - b).abs() < 1.0e-4);
        }
        assert!(im.iter().all(|v| v.abs() < 1.0e-4));
    }

    #[test]
    fn magnitudes_find_a_sine() {
        let fft = Fft::new(256);
        let input: Vec<f32> = (0..256).map(|i| (TWO_PI * 8.0 * i as f32 / 256.0).sin()).collect();
        let mut mags = Vec::new();
        fft.magnitudes(&input, &mut mags);
        assert_eq!(mags.len(), 128);
        let peak = (0..128).max_by(|a, b| mags[*a].total_cmp(&mags[*b])).unwrap();
        assert_eq!(peak, 8);
        // Hann window halves the coherent gain.
        assert!((mags[8] - 0.5).abs() < 0.01, "{}", mags[8]);
    }
}
