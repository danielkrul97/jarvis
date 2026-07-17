use image::imageops::{self, FilterType};
use image::RgbImage;

/// dHash: zmenšení na 9x8 v odstínech šedi, bit = jas(x) > jas(x+1).
/// Stabilní vůči drobnému šumu, citlivý na změnu rozložení obrazovky.
pub fn dhash(img: &RgbImage) -> u64 {
    let gray = imageops::grayscale(img);
    let small = imageops::resize(&gray, 9, 8, FilterType::Triangle);
    let mut hash = 0u64;
    let mut bit = 0u32;
    for y in 0..8 {
        for x in 0..8 {
            if small.get_pixel(x, y)[0] > small.get_pixel(x + 1, y)[0] {
                hash |= 1 << bit;
            }
            bit += 1;
        }
    }
    hash
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn gradient(rising: bool) -> RgbImage {
        RgbImage::from_fn(64, 64, |x, _y| {
            let v = if rising { (x * 4) as u8 } else { 255 - (x * 4) as u8 };
            Rgb([v, v, v])
        })
    }

    #[test]
    fn identical_images_have_zero_distance() {
        let a = gradient(true);
        let b = gradient(true);
        assert_eq!(hamming(dhash(&a), dhash(&b)), 0);
    }

    #[test]
    fn opposite_gradients_are_far_apart() {
        let a = gradient(true);
        let b = gradient(false);
        assert!(hamming(dhash(&a), dhash(&b)) > 32);
    }

    #[test]
    fn hamming_counts_bits() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hamming(0b1010, 0b0110), 2);
    }
}
